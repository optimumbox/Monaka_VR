// A picture on a flat rectangle in tracking space (a panel the compositor shows on its own), drawn
// into an eye's image as that eye saw it: for captures that should show what the headset showed.

Texture2D picture : register(t0);
SamplerState smooth : register(s0);

cbuffer Quad : register(b0) {
    float4 view0;     // tracking to eye space, rows
    float4 view1;
    float4 view2;
    float4 tangents;  // the image edges: left, right, up, down
    float4 corners[4];  // top left, top right, bottom left, bottom right (tracking space)
    uint4 mode;         // x: 0 the values go out as read, 1 encoded to sRGB, 2 decoded to linear
};

struct Out {
    float4 position : SV_Position;
    float2 uv : TEXCOORD;
};

static const uint ORDER[6] = { 0, 1, 2, 2, 1, 3 };
static const float2 UVS[4] = { float2(0, 0), float2(1, 0), float2(0, 1), float2(1, 1) };

Out vs_main(uint vertex : SV_VertexID) {
    uint corner = ORDER[vertex];
    float4 p = float4(corners[corner].xyz, 1);
    float3 v = float3(dot(view0, p), dot(view1, p), dot(view2, p));
    float w = -v.z;
    // Clip space with w = distance ahead, so the picture is mapped in perspective.
    float x = (2 * v.x - (tangents.x + tangents.y) * w) / (tangents.y - tangents.x);
    float y = (2 * v.y - (tangents.z + tangents.w) * w) / (tangents.z - tangents.w);
    Out o;
    o.position = float4(x, y, 0.5 * w, w);
    o.uv = UVS[corner];
    return o;
}

float3 to_srgb(float3 c) {
    return c <= 0.0031308 ? c * 12.92 : 1.055 * pow(abs(c), 1.0 / 2.4) - 0.055;
}

float3 to_linear(float3 c) {
    return c <= 0.04045 ? c / 12.92 : pow((abs(c) + 0.055) / 1.055, 2.4);
}

// Four taps over the pixel's footprint: a picture drawn smaller than its pixels (a hand panel, the
// flat screen) does not shimmer.
float4 ps_main(Out i) : SV_Target {
    float2 dx = ddx(i.uv) * 0.25;
    float2 dy = ddy(i.uv) * 0.25;
    float4 c = 0.25 * (picture.Sample(smooth, i.uv + dx + dy) + picture.Sample(smooth, i.uv + dx - dy) + picture.Sample(smooth, i.uv - dx + dy) + picture.Sample(smooth, i.uv - dx - dy));
    // Premultiplied: the colour as it would be unmultiplied is what is encoded.
    if (mode.x != 0 && c.a > 0) {
        float3 plain = c.rgb / c.a;
        c.rgb = (mode.x == 1 ? to_srgb(plain) : to_linear(plain)) * c.a;
    }
    return c;
}
