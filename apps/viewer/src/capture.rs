//! A capture of what the headset shows, asked for with Monaka's chord ([`crate::chord`]): the next
//! [`PAIRS`] pairs as they arrive, both eyes at full size, each with its pose record, into a new
//! `capture-N` folder of the run's `captures` folder (`--captures`). With frame warping the eyes
//! take turns being real, so consecutive pairs show each eye both real and warped.
//!
//! Each pair is copied on the GPU as it arrives; the copies are read back one image per frame
//! (a few milliseconds each: a short hitch), and written as JPEG on a thread of their own (an
//! uncompressed PNG of a headset eye is tens of megabytes).
//!
//! The first pair also goes into one image, the eyes side by side ([`BOTH`]), and that file onto
//! the clipboard ([`crate::clipboard`]), ready to paste.
//!
//! What the chord was pressed for (a flicker, a frame gone wrong) is often past by then: the last
//! [`HISTORY`] pairs before it can be kept all along at half size each way ([`History`]: copied
//! and halved on the GPU as they arrive), and written too, `before-K-left.jpg` the pair K frames
//! before the chord. Only with `MONAKA_CAPTURE_HISTORY` set: kept all along it cost the viewer's GPU
//! time enough to drop the game's frames (a run of it: the viewer's GPU 0.5 ms to 10 ms, 540 of the
//! game's 900 frames shown).

use monaka_channel::d3d::{self, create_texture, read_image};
use monaka_contract::protocol::{RenderedPair, dxgi_format};
use std::path::{Path, PathBuf};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_GENERATE_MIPS, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext,
    ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

/// Pairs per capture.
pub const PAIRS: usize = 6;
/// The images' JPEG quality (1-100).
const JPEG_QUALITY: u8 = 92;
/// The first pair side by side, left eye first: the image put on the clipboard.
const BOTH: &str = "pair-1-both.jpg";
/// Pairs kept from before a capture is asked for (a second of the headset's), at half size.
pub const HISTORY: usize = 90;
/// Whether the pairs before a capture are kept (`MONAKA_CAPTURE_HISTORY` set).
fn history_wanted() -> bool {
    std::env::var_os("MONAKA_CAPTURE_HISTORY").is_some()
}

/// The most of the GPU's memory the kept pairs take (fewer kept of larger eyes).
const HISTORY_BYTES: u64 = 1 << 30;

/// The last pairs as they arrived, halved: each eye copied into a texture with a second mip, the
/// mip made, and copied into the ring. Its textures are made as it fills, two a frame.
struct History {
    /// The eyes' width, height and format it was made for.
    source: (u32, u32, DXGI_FORMAT),
    halvers: Vec<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    half: D3D11_TEXTURE2D_DESC,
    ring: Vec<[ID3D11Texture2D; 2]>,
    sequences: Vec<u64>,
    slots: usize,
    pushed: usize,
}

impl History {
    /// One for eyes like `eye`; None for a format the GPU cannot halve.
    fn new(device: &ID3D11Device, eye: &ID3D11Texture2D) -> Option<Self> {
        let desc = d3d::texture_desc(eye);
        let format = typed(desc.Format);
        if !d3d::makes_mips(device, format) || desc.Width < 2 || desc.Height < 2 {
            return None;
        }
        let halver = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 2,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
        };
        let mut halvers = Vec::new();
        for _ in 0..2 {
            let texture = create_texture(device, &halver).ok()?;
            let view = d3d::shader_view(device, &texture).ok()?;
            halvers.push((texture, view));
        }
        let half = D3D11_TEXTURE2D_DESC { BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, ..d3d::simple_texture_desc(desc.Width / 2, desc.Height / 2, format) };
        let pair_bytes = u64::from(half.Width) * u64::from(half.Height) * 4 * 2;
        let slots = HISTORY.min((HISTORY_BYTES / pair_bytes.max(1)) as usize).max(1);
        Some(Self { source: (desc.Width, desc.Height, desc.Format), halvers, half, ring: Vec::new(), sequences: Vec::new(), slots, pushed: 0 })
    }

    fn fits(&self, eye: &ID3D11Texture2D) -> bool {
        let desc = d3d::texture_desc(eye);
        self.source == (desc.Width, desc.Height, desc.Format)
    }

    /// Keeps the pair halved, over the oldest once full.
    fn keep(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext, eyes: [&ID3D11Texture2D; 2], sequence: u64) {
        let slot = self.pushed % self.slots;
        if slot == self.ring.len() {
            let (Ok(left), Ok(right)) = (create_texture(device, &self.half), create_texture(device, &self.half)) else { return };
            self.ring.push([left, right]);
            self.sequences.push(0);
        }
        for (e, eye) in eyes.into_iter().enumerate() {
            let (halver, view) = &self.halvers[e];
            // The halver is the eye's size, the ring's textures its second mip's.
            d3d::copy_subresource(context, halver, 0, eye, 0);
            d3d::make_mips(context, view);
            d3d::copy_subresource(context, &self.ring[slot][e], 0, halver, 1);
        }
        self.sequences[slot] = sequence;
        self.pushed += 1;
    }

    /// The kept pairs' slots, oldest first.
    fn order(&self) -> Vec<usize> {
        let kept = self.pushed.min(self.ring.len());
        (0..kept).map(|i| (self.pushed - kept + i) % self.slots).collect()
    }
}

/// A typeless format's plain one (the eyes may be shared typeless): the halver's views need one.
fn typed(format: DXGI_FORMAT) -> DXGI_FORMAT {
    match format.0 {
        27 => DXGI_FORMAT(28),
        90 => DXGI_FORMAT(87),
        23 => DXGI_FORMAT(24),
        other => DXGI_FORMAT(other),
    }
}

struct Kept {
    eyes: [ID3D11Texture2D; 2],
    sequence: u64,
    record: Option<RenderedPair>,
}

pub struct Capture {
    folder: PathBuf,
    /// The capture under way: its folder, and the pairs still to keep.
    current: Option<(PathBuf, usize)>,
    kept: Vec<Kept>,
    /// Images read back so far of the kept pairs (two per pair).
    read: usize,
    /// The first pair's images as read back (width, height, format, texels), left first.
    first: Vec<(u32, u32, u32, Vec<u8>)>,
    lines: Vec<String>,
    /// The pairs before ([`History`]; none until the first pair, nor for eyes it cannot halve),
    /// and, while a capture is under way, its kept slots, oldest first.
    history: Option<History>,
    history_failed: bool,
    before: Vec<usize>,
}

/// Asks `capture` (none without a folder) for a capture of the next pairs, saying why when it
/// cannot start. Whether it started.
pub fn start(capture: Option<&mut Capture>) -> bool {
    match capture.map(Capture::start) {
        Some(started) => {
            if !started {
                println!("Capture: the last one is still being saved");
            }
            started
        }
        None => {
            println!("Capture: no folder to save to (--captures)");
            false
        }
    }
}

impl Capture {
    /// Captures go into `folder` (made when the first is asked for).
    pub fn new(folder: PathBuf) -> Self {
        Self { folder, current: None, kept: Vec::new(), read: 0, first: Vec::new(), lines: Vec::new(), history: None, history_failed: !history_wanted(), before: Vec::new() }
    }

    /// Asks for a capture of the next pairs; false while one is still under way.
    pub fn start(&mut self) -> bool {
        if self.current.is_some() || !self.kept.is_empty() {
            return false;
        }
        let number = (1..).find(|n| !self.folder.join(format!("capture-{n}")).exists()).unwrap_or(1);
        let folder = self.folder.join(format!("capture-{number}"));
        if let Err(e) = std::fs::create_dir_all(&folder) {
            println!("Capture: cannot make {}: {e}", folder.display());
            return false;
        }
        println!("Capture: the next {PAIRS} pairs to {}", folder.display());
        self.before = self.history.as_ref().map(History::order).unwrap_or_default();
        self.lines = vec![format!(
            "{PAIRS} consecutive pairs as the headset showed them (left, right); pose records as the producer sent them; before them the {} pairs before the chord at half size (before-K: K frames before)",
            self.before.len()
        )];
        self.current = Some((folder, PAIRS));
        true
    }

    /// A pair arrived (`eyes` hold it now): kept while a capture wants it.
    pub fn arrived(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext, eyes: [&ID3D11Texture2D; 2], sequence: u64, record: Option<&RenderedPair>) {
        if self.current.is_none() {
            self.remember(device, context, eyes, sequence);
            return;
        }
        let Some((_, wanted)) = self.current.as_mut().filter(|(_, wanted)| *wanted > 0) else { return };
        let copy = |eye: &ID3D11Texture2D| -> Option<ID3D11Texture2D> {
            let copy = create_texture(device, &d3d::private_copy_desc(&d3d::texture_desc(eye))).ok()?;
            d3d::copy_texture(context, &copy, eye);
            Some(copy)
        };
        let (Some(left), Some(right)) = (copy(eyes[0]), copy(eyes[1])) else {
            println!("Capture: a pair could not be copied; stopped");
            self.current = None;
            return;
        };
        self.kept.push(Kept { eyes: [left, right], sequence, record: record.copied() });
        *wanted -= 1;
    }

    /// Between captures: the pair kept halved ([`History`]).
    fn remember(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext, eyes: [&ID3D11Texture2D; 2], sequence: u64) {
        if self.history_failed {
            return;
        }
        if !self.history.as_ref().is_some_and(|h| h.fits(eyes[0]) && h.fits(eyes[1])) {
            self.history = History::new(device, eyes[0]).filter(|h| h.fits(eyes[1]));
            if self.history.is_none() {
                self.history_failed = true;
                println!("Capture: the pairs before a capture cannot be kept (the eyes' format)");
                return;
            }
        }
        if let Some(history) = self.history.as_mut() {
            history.keep(device, context, eyes, sequence);
        }
    }

    /// Once a capture's pairs are all kept: reads one image back and hands it to a writer thread.
    pub fn step(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext) {
        let Some((folder, 0)) = self.current.clone() else { return };
        if self.read >= self.kept.len() * 2 {
            self.step_before(device, context, &folder);
            return;
        }
        let Some(pair) = self.kept.get(self.read / 2) else { return };
        let (index, eye) = (self.read / 2, self.read % 2);
        if eye == 0 {
            self.lines.push(format!("pair {} (sequence {}): {}", index + 1, pair.sequence, describe(pair.record.as_ref())));
        }
        let texture = &pair.eyes[eye];
        let format = d3d::texture_desc(texture).Format.0 as u32;
        match read_image(device, context, texture) {
            Ok((width, height, texels)) => {
                let path = folder.join(format!("pair-{}-{}.jpg", index + 1, ["left", "right"][eye]));
                if index == 0 {
                    self.first.push((width, height, format, texels.clone()));
                    if self.first.len() == 2 {
                        let (eyes, both) = (std::mem::take(&mut self.first), folder.join(BOTH));
                        std::thread::spawn(move || copy_both(&both, &eyes));
                    }
                }
                std::thread::spawn(move || write_jpeg(&path, width, height, format, &texels, true));
            }
            Err(e) => println!("Capture: pair {} {} unreadable: {e}", index + 1, ["left", "right"][eye]),
        }
        self.read += 1;
    }

    /// After the capture's own pairs: one of the pairs before read back and written.
    fn step_before(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext, folder: &Path) {
        let i = self.read - self.kept.len() * 2;
        let (Some(&slot), Some(history)) = (self.before.get(i / 2), self.history.as_ref()) else {
            self.finish(folder);
            return;
        };
        let (k, eye) = (self.before.len() - i / 2, i % 2);
        if eye == 0 {
            self.lines.push(format!("before {k} (sequence {}), half size", history.sequences[slot]));
        }
        let texture = &history.ring[slot][eye];
        let format = d3d::texture_desc(texture).Format.0 as u32;
        match read_image(device, context, texture) {
            Ok((width, height, texels)) => {
                let path = folder.join(format!("before-{k:02}-{}.jpg", ["left", "right"][eye]));
                std::thread::spawn(move || write_jpeg(&path, width, height, format, &texels, false));
            }
            Err(e) => println!("Capture: the pair {k} before, {} unreadable: {e}", ["left", "right"][eye]),
        }
        self.read += 1;
    }

    fn finish(&mut self, folder: &Path) {
        let _ = std::fs::write(folder.join("pairs.txt"), self.lines.join("\n") + "\n");
        println!("Capture saved: {}", folder.display());
        (self.current, self.read) = (None, 0);
        self.kept.clear();
        self.first.clear();
        self.before.clear();
        if let Some(history) = self.history.as_mut() {
            // The pairs before the next capture begin after this one.
            history.pushed = 0;
        }
    }
}

/// A pair's pose record in a line: each eye's position and orientation, or why there is none.
fn describe(record: Option<&RenderedPair>) -> String {
    let Some(record) = record else { return "no pose record (a flat screen)".into() };
    let eye = |e: usize| {
        let p = &record.eyes[e];
        format!("position {:?} orientation {:?}", p.position, p.orientation)
    };
    format!("left {}; right {}", eye(0), eye(1))
}

/// The texels as an RGB JPEG (the sRGB formats are stored as their plain ones). A warped eye's
/// alpha says where each pixel came from (`monaka_warp`'s `reproject.hlsl`): such an image also
/// gets `<name>-sources.jpg` (when `sources`; a halved eye's alpha is blended), the real eye's
/// pixels as they are, its own last frame's tinted green, a neighbour's copy solid blue, nothing
/// solid magenta.
fn write_jpeg(path: &Path, width: u32, height: u32, format: u32, texels: &[u8], sources: bool) {
    let Some(pixels) = rgba(format, texels) else {
        println!("Capture: {} not written (image format {format})", path.display());
        return;
    };
    let save = |path: &Path, rgb: Vec<u8>| {
        if let Err(e) = save_jpeg(path, width, height, &rgb) {
            println!("Capture: cannot write {}: {e}", path.display());
        }
    };
    save(path, pixels.iter().flat_map(|[r, g, b, _]| [*r, *g, *b]).collect());
    if !sources || pixels.iter().all(|p| p[3] == 255) {
        return;
    }
    let sources = pixels
        .iter()
        .flat_map(|&[r, g, b, a]| match a {
            213.. => [r, g, b],
            128..=212 => [(r as u16 * 2 / 5) as u8, (g as u16 * 2 / 5 + 153) as u8, (b as u16 * 2 / 5) as u8],
            43..=127 => [0, 0, 255],
            _ => [255, 0, 255],
        })
        .collect();
    save(&path.with_file_name(format!("{}-sources.jpg", path.file_stem().unwrap_or_default().to_string_lossy())), sources);
}

/// The texels as RGBA (the sRGB formats are stored as their plain ones); None for a format with
/// no 8-bit reading.
fn rgba(format: u32, texels: &[u8]) -> Option<Vec<[u8; 4]>> {
    use dxgi_format::*;
    let plain = match format {
        R8G8B8A8_UNORM_SRGB => R8G8B8A8_UNORM,
        B8G8R8A8_UNORM_SRGB => B8G8R8A8_UNORM,
        other => other,
    };
    texels.as_chunks::<4>().0.iter().map(|t| monaka_core::bmp::dxgi_rgba8(plain, *t)).collect()
}

fn save_jpeg(path: &Path, width: u32, height: u32, rgb: &[u8]) -> Result<(), String> {
    // Full-resolution colour (no chroma subsampling): captures are cropped and judged up close.
    let mut encoder = jpeg_encoder::Encoder::new_file(path, JPEG_QUALITY).map_err(|e| e.to_string())?;
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
    encoder.encode(rgb, width as u16, height as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| e.to_string())
}

/// Two eyes (width, height, format, texels) side by side as RGB, left first, the shorter one's
/// rows below it black: the image's width, height and pixels.
fn side_by_side(eyes: &[(u32, u32, u32, Vec<u8>)]) -> Option<(u32, u32, Vec<u8>)> {
    let [(lw, lh, lf, lt), (rw, rh, rf, rt)] = eyes else { return None };
    let (left, right) = (rgba(*lf, lt)?, rgba(*rf, rt)?);
    let (width, height) = (lw + rw, (*lh).max(*rh));
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for (pixels, w, h) in [(&left, *lw, *lh), (&right, *rw, *rh)] {
            for x in 0..w {
                let [r, g, b, _] = if y < h { pixels[(y * w + x) as usize] } else { [0; 4] };
                rgb.extend_from_slice(&[r, g, b]);
            }
        }
    }
    Some((width, height, rgb))
}

/// The first pair side by side into `path`, and that file onto the clipboard.
fn copy_both(path: &Path, eyes: &[(u32, u32, u32, Vec<u8>)]) {
    let Some((width, height, rgb)) = side_by_side(eyes) else {
        println!("Capture: {} not written (image format)", path.display());
        return;
    };
    if let Err(e) = save_jpeg(path, width, height, &rgb) {
        println!("Capture: cannot write {}: {e}", path.display());
        return;
    }
    match crate::clipboard::copy_file(path) {
        Ok(()) => println!("Capture: {} copied to the clipboard", path.display()),
        Err(e) => println!("Capture: {} not copied to the clipboard: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests;
