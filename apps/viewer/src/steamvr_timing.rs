//! SteamVR's own account of each headset frame, read beside the OpenXR session and said every
//! [`EVERY`]: how often the viewer's frame came late or was shown again, why SteamVR reprojected
//! (its CPU or GPU reason flags, its throttling), and the GPU and CPU times it measured for the
//! viewer and for its own compositor. Read only: OpenVR is opened as a background application
//! (`VRApplication_Background`), which neither submits nor takes the scene, through the
//! `openvr_api.dll` of the SteamVR runtime the machine's OpenVR paths name. Another runtime, or
//! SteamVR not answering, leaves it out (said once).
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{HSTRING, s};

/// How often the frames since the last are said.
const EVERY: Duration = Duration::from_secs(10);
/// How often SteamVR's recent frames are read (it keeps more than this many refreshes' worth).
const READ_EVERY: Duration = Duration::from_millis(200);
/// Frames read at once: well over a [`READ_EVERY`] of refreshes at 144 Hz.
const BATCH: usize = 64;

const VR_APPLICATION_BACKGROUND: i32 = 3;
const COMPOSITOR_TABLE: &[u8] = b"FnTable:IVRCompositor_029\0";

// m_nReprojectionFlags (openvr.h).
const REPROJECTION_CPU: u32 = 0x01;
const REPROJECTION_GPU: u32 = 0x02;
const REPROJECTION_ASYNC: u32 = 0x04;
const REPROJECTION_MOTION: u32 = 0x08;
const PREDICTION_MASK: u32 = 0xF0;
const THROTTLE_MASK: u32 = 0xF00;

/// `TrackedDevicePose_t` (openvr_capi.h), only for its size.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Pose {
    matrix: [[f32; 4]; 3],
    velocity: [f32; 3],
    angular_velocity: [f32; 3],
    tracking_result: i32,
    valid: bool,
    connected: bool,
}

/// `Compositor_FrameTiming` (openvr_capi.h, OpenVR 2.x): one headset frame as SteamVR saw it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameTiming {
    size: u32,
    frame_index: u32,
    presents: u32,
    mis_presented: u32,
    dropped: u32,
    reprojection_flags: u32,
    system_time_s: f64,
    pre_submit_gpu_ms: f32,
    post_submit_gpu_ms: f32,
    total_render_gpu_ms: f32,
    compositor_render_gpu_ms: f32,
    compositor_render_cpu_ms: f32,
    compositor_idle_cpu_ms: f32,
    client_frame_interval_ms: f32,
    present_call_cpu_ms: f32,
    wait_for_present_cpu_ms: f32,
    submit_frame_ms: f32,
    wait_get_poses_called_ms: f32,
    new_poses_ready_ms: f32,
    new_frame_ready_ms: f32,
    compositor_update_start_ms: f32,
    compositor_update_end_ms: f32,
    compositor_render_start_ms: f32,
    hmd_pose: Pose,
    vsyncs_ready_for_use: u32,
    vsyncs_to_first_view: u32,
    transfer_latency_ms: f32,
}

type InitFn = unsafe extern "system" fn(*mut i32, i32) -> isize;
type InterfaceFn = unsafe extern "system" fn(*const u8, *mut i32) -> isize;
type ShutdownFn = unsafe extern "system" fn();
type GetFrameTimingsFn = unsafe extern "system" fn(*mut FrameTiming, u32) -> u32;

/// `VR_IVRCompositor_FnTable`'s entries up to `GetFrameTimings` (the 12th).
const GET_FRAME_TIMINGS_AT: usize = 11;

/// An average and the most of one measure over a window.
#[derive(Default, Clone, Copy)]
struct Spread {
    sum: f64,
    most: f32,
    n: u32,
}

impl Spread {
    fn add(&mut self, value: f32) {
        self.sum += f64::from(value);
        self.most = self.most.max(value);
        self.n += 1;
    }

    fn said(&self) -> String {
        format!("{:.2} (at most {:.2})", self.sum / f64::from(self.n.max(1)), self.most)
    }
}

/// The window's tallies.
#[derive(Default)]
struct Window {
    frames: u32,
    shown_again: u32,
    dropped: u32,
    mis_presented: u32,
    cpu: u32,
    gpu: u32,
    async_on: u32,
    motion: u32,
    predicted_ahead: u32,
    throttled: u32,
    app_gpu: Spread,
    app_gpu_pre: Spread,
    app_gpu_post: Spread,
    compositor_gpu: Spread,
    compositor_cpu: Spread,
    interval: Spread,
    new_frame_ready: Spread,
    wait_get_poses: Spread,
}

pub struct Reader {
    _library: HMODULE,
    shutdown: ShutdownFn,
    get_frame_timings: GetFrameTimingsFn,
    last_index: u32,
    read_at: Instant,
    since: Instant,
    window: Window,
}

impl Reader {
    /// SteamVR's frame timing, or none (said why).
    pub fn open() -> Option<Self> {
        match Self::try_open() {
            Ok(reader) => {
                println!("SteamVR frame timing: read every {} ms, said every {} s", READ_EVERY.as_millis(), EVERY.as_secs());
                Some(reader)
            }
            Err(why) => {
                println!("SteamVR frame timing: not read ({why})");
                None
            }
        }
    }

    fn try_open() -> Result<Self, String> {
        let runtime = steamvr_runtime().ok_or("no SteamVR runtime in the OpenVR paths")?;
        let dll = format!("{runtime}\\bin\\win64\\openvr_api.dll");
        // SAFETY: loads SteamVR's own OpenVR client library by its full path.
        let library = unsafe { LoadLibraryW(&HSTRING::from(dll.as_str())) }.map_err(|e| format!("{dll}: {e}"))?;
        let export = |name| {
            // SAFETY: an export of the library just loaded, by name.
            unsafe { GetProcAddress(library, name) }
        };
        let (Some(init), Some(interface), Some(shutdown)) = (export(s!("VR_InitInternal")), export(s!("VR_GetGenericInterface")), export(s!("VR_ShutdownInternal"))) else {
            return Err("openvr_api.dll lacks its entry points".into());
        };
        // SAFETY: the exports' documented signatures (openvr_capi.h).
        let (init, interface, shutdown) = unsafe {
            (
                std::mem::transmute::<unsafe extern "system" fn() -> isize, InitFn>(init),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, InterfaceFn>(interface),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, ShutdownFn>(shutdown),
            )
        };
        let mut error = 0;
        // SAFETY: OpenVR's init as a background application, its error written to a local.
        unsafe { init(&mut error, VR_APPLICATION_BACKGROUND) };
        if error != 0 {
            return Err(format!("OpenVR init error {error}"));
        }
        // SAFETY: the compositor's function table by its version name, the error to a local.
        let table = unsafe { interface(COMPOSITOR_TABLE.as_ptr(), &mut error) } as *const *const c_void;
        if error != 0 || table.is_null() {
            // SAFETY: undoes the init above.
            unsafe { shutdown() };
            return Err(format!("no IVRCompositor_029 (error {error})"));
        }
        // SAFETY: the table's GetFrameTimings entry (openvr_capi.h's order), a function pointer.
        let entry = unsafe { *table.add(GET_FRAME_TIMINGS_AT) };
        if entry.is_null() {
            // SAFETY: as above.
            unsafe { shutdown() };
            return Err("the compositor table has no GetFrameTimings".into());
        }
        // SAFETY: the entry's documented signature.
        let get_frame_timings = unsafe { std::mem::transmute::<*const c_void, GetFrameTimingsFn>(entry) };
        let now = Instant::now();
        Ok(Self { _library: library, shutdown, get_frame_timings, last_index: 0, read_at: now, since: now, window: Window::default() })
    }

    /// Once a loop: SteamVR's frames since the last read, every [`READ_EVERY`]; the window said every
    /// [`EVERY`].
    pub fn poll(&mut self, now: Instant) {
        if now.duration_since(self.read_at) < READ_EVERY {
            return;
        }
        self.read_at = now;
        let mut frames = [FrameTiming::default(); BATCH];
        frames[0].size = size_of::<FrameTiming>() as u32;
        // SAFETY: BATCH entries to fill, the first's size set as the call asks.
        let filled = unsafe { (self.get_frame_timings)(frames.as_mut_ptr(), BATCH as u32) } as usize;
        // Oldest first; each frame once.
        for frame in frames.iter().take(filled.min(BATCH)) {
            if frame.frame_index == 0 || frame.frame_index <= self.last_index {
                continue;
            }
            self.last_index = frame.frame_index;
            self.add(frame);
        }
        if now.duration_since(self.since) >= EVERY {
            self.say(now);
        }
    }

    fn add(&mut self, f: &FrameTiming) {
        let w = &mut self.window;
        w.frames += 1;
        w.shown_again += u32::from(f.presents > 1);
        w.dropped += f.dropped;
        w.mis_presented += f.mis_presented;
        let flags = f.reprojection_flags;
        w.cpu += u32::from(flags & REPROJECTION_CPU != 0);
        w.gpu += u32::from(flags & REPROJECTION_GPU != 0);
        w.async_on += u32::from(flags & REPROJECTION_ASYNC != 0);
        w.motion += u32::from(flags & REPROJECTION_MOTION != 0);
        w.predicted_ahead += u32::from(flags & PREDICTION_MASK != 0);
        w.throttled += u32::from(flags & THROTTLE_MASK != 0);
        w.app_gpu.add(f.total_render_gpu_ms);
        w.app_gpu_pre.add(f.pre_submit_gpu_ms);
        w.app_gpu_post.add(f.post_submit_gpu_ms);
        w.compositor_gpu.add(f.compositor_render_gpu_ms);
        w.compositor_cpu.add(f.compositor_render_cpu_ms);
        w.interval.add(f.client_frame_interval_ms);
        w.new_frame_ready.add(f.new_frame_ready_ms);
        w.wait_get_poses.add(f.wait_get_poses_called_ms);
    }

    fn say(&mut self, now: Instant) {
        let w = std::mem::take(&mut self.window);
        let seconds = now.duration_since(self.since).as_secs_f64();
        self.since = now;
        if w.frames == 0 {
            println!("SteamVR frames over {seconds:.0} s: none read");
            return;
        }
        println!(
            "SteamVR frames over {seconds:.0} s: {}; shown again {}, dropped {}, shown on the wrong refresh {}; reprojected for the CPU {}, for the GPU {}, motion smoothing {}, async on {}, predicted further ahead {}, throttled {}",
            w.frames, w.shown_again, w.dropped, w.mis_presented, w.cpu, w.gpu, w.motion, w.async_on, w.predicted_ahead, w.throttled
        );
        println!(
            "SteamVR times (ms): the viewer's GPU {} (before submit {}, after {}); the compositor's GPU {}, CPU {}; between the viewer's frames {}; its frame ready {} and its pose wait {} into the refresh",
            w.app_gpu.said(),
            w.app_gpu_pre.said(),
            w.app_gpu_post.said(),
            w.compositor_gpu.said(),
            w.compositor_cpu.said(),
            w.interval.said(),
            w.new_frame_ready.said(),
            w.wait_get_poses.said()
        );
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: undoes the background init this reader made.
        unsafe { (self.shutdown)() };
    }
}

/// The SteamVR runtime folder `%LOCALAPPDATA%\openvr\openvrpaths.vrpath` names first.
fn steamvr_runtime() -> Option<String> {
    let paths = std::env::var("LOCALAPPDATA").ok()? + "\\openvr\\openvrpaths.vrpath";
    runtime_in(&std::fs::read_to_string(paths).ok()?)
}

/// The first entry of the `"runtime"` list in an `openvrpaths.vrpath` file's text, unescaped.
fn runtime_in(text: &str) -> Option<String> {
    let after = &text[text.find("\"runtime\"")? + "\"runtime\"".len()..];
    let list = &after[after.find('[')? + 1..];
    let start = list.find('"')? + 1;
    let end = start + list[start..].find('"')?;
    Some(list[start..end].replace("\\\\", "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_timing_is_openvrs_size() {
        assert_eq!(size_of::<Pose>(), 80);
        assert_eq!(size_of::<FrameTiming>(), 192);
    }

    #[test]
    fn runtime_from_the_paths_file() {
        let text = "{\n\t\"config\" : [ \"C:\\\\Steam\\\\config\" ],\n\t\"runtime\" :\n\t[\n\t\t\"C:\\\\Program Files (x86)\\\\Steam\\\\steamapps\\\\common\\\\SteamVR\"\n\t],\n\t\"version\" : 1\n}";
        assert_eq!(runtime_in(text).as_deref(), Some("C:\\Program Files (x86)\\Steam\\steamapps\\common\\SteamVR"));
        assert_eq!(runtime_in("{}"), None);
    }
}
