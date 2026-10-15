// FluxCapture GPU programs: scene composition, preview and RGB -> NV12 conversion.
// Compiled at start-up with d3dcompiler_47 (vs_4_0 / ps_4_0, feature level 10.0+).

cbuffer LayerCB : register(b0)
{
    float4 dst_rect;  // NDC: x0, y0, x1, y1
    float4 src_rect;  // UV of mip 0: u0, v0, u1, v1
    float4 color;     // solid colour (straight alpha) for ps_color; .a = layer opacity otherwise
    float lod;        // mip level to sample
    float opaque;     // 1 = ignore the source alpha (desktop / window captures)
    float2 layer_pad;
};

cbuffer ConvertCB : register(b1)
{
    float4 y_coef;        // rgb weights, offset in .w
    float4 u_coef;
    float4 v_coef;
    float2 canvas_texel;  // 1 / canvas size
    float2 convert_pad;
};

Texture2D tex0 : register(t0);
SamplerState linear_clamp : register(s0);
SamplerState point_clamp : register(s1);

struct VsOut
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

// A quad as a 4-vertex triangle strip, placed by the constant buffer.
VsOut vs_quad(uint id : SV_VertexID)
{
    float2 t = float2(id & 1, id >> 1);
    VsOut o;
    o.pos = float4(lerp(dst_rect.x, dst_rect.z, t.x), lerp(dst_rect.y, dst_rect.w, t.y), 0.0, 1.0);
    o.uv = lerp(src_rect.xy, src_rect.zw, t);
    return o;
}

// A full-screen triangle.
VsOut vs_full(uint id : SV_VertexID)
{
    float2 t = float2((id << 1) & 2, id & 2);
    VsOut o;
    o.pos = float4(t * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    o.uv = t;
    return o;
}

float4 finish(float4 c)
{
    c = saturate(c);
    c.a = lerp(c.a, 1.0, opaque);
    float a = c.a * color.a;
    return float4(c.rgb * a, a); // premultiplied output
}

float4 ps_bilinear(VsOut i) : SV_Target
{
    return finish(tex0.SampleLevel(linear_clamp, i.uv, lod));
}

// Catmull-Rom bicubic in 9 bilinear taps.
float4 ps_bicubic(VsOut i) : SV_Target
{
    float2 size;
    float levels;
    tex0.GetDimensions((uint)lod, size.x, size.y, levels);
    float2 sample_pos = i.uv * size;
    float2 p1 = floor(sample_pos - 0.5) + 0.5;
    float2 f = sample_pos - p1;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 p12 = (p1 + w2 / max(w12, 1e-5)) / size;
    float2 p0 = (p1 - 1.0) / size;
    float2 p3 = (p1 + 2.0) / size;
    float4 r = 0;
    r += tex0.SampleLevel(linear_clamp, float2(p0.x, p0.y), lod) * w0.x * w0.y;
    r += tex0.SampleLevel(linear_clamp, float2(p12.x, p0.y), lod) * w12.x * w0.y;
    r += tex0.SampleLevel(linear_clamp, float2(p3.x, p0.y), lod) * w3.x * w0.y;
    r += tex0.SampleLevel(linear_clamp, float2(p0.x, p12.y), lod) * w0.x * w12.y;
    r += tex0.SampleLevel(linear_clamp, float2(p12.x, p12.y), lod) * w12.x * w12.y;
    r += tex0.SampleLevel(linear_clamp, float2(p3.x, p12.y), lod) * w3.x * w12.y;
    r += tex0.SampleLevel(linear_clamp, float2(p0.x, p3.y), lod) * w0.x * w3.y;
    r += tex0.SampleLevel(linear_clamp, float2(p12.x, p3.y), lod) * w12.x * w3.y;
    r += tex0.SampleLevel(linear_clamp, float2(p3.x, p3.y), lod) * w3.x * w3.y;
    return finish(r);
}

float lanczos3(float x)
{
    x = abs(x);
    if (x < 1e-5)
        return 1.0;
    if (x >= 3.0)
        return 0.0;
    float px = 3.14159265 * x;
    return 3.0 * sin(px) * sin(px / 3.0) / (px * px);
}

// Lanczos-3, 6x6 taps.
float4 ps_lanczos(VsOut i) : SV_Target
{
    uint l = (uint)lod;
    float2 size;
    float levels;
    tex0.GetDimensions(l, size.x, size.y, levels);
    float2 pos = i.uv * size - 0.5;
    float2 base = floor(pos);
    float2 f = pos - base;
    int2 hi = int2(size) - 1;
    float4 sum = 0;
    float wsum = 0;
    [unroll] for (int y = -2; y <= 3; y++)
    {
        float wy = lanczos3(y - f.y);
        [unroll] for (int x = -2; x <= 3; x++)
        {
            float w = lanczos3(x - f.x) * wy;
            int2 p = clamp(int2(base) + int2(x, y), int2(0, 0), hi);
            sum += tex0.Load(int3(p, l)) * w;
            wsum += w;
        }
    }
    return finish(sum / max(wsum, 1e-5));
}

float4 ps_color(VsOut i) : SV_Target
{
    return float4(color.rgb * color.a, color.a);
}

// Luma plane: one canvas pixel per output pixel.
float ps_y(VsOut i) : SV_Target
{
    float3 rgb = tex0.SampleLevel(point_clamp, i.pos.xy * canvas_texel, 0).rgb;
    return dot(rgb, y_coef.xyz) + y_coef.w;
}

// Chroma plane (half size), MPEG-2 / H.264 default siting: horizontally co-sited with the left
// luma column ([1 2 1] / 4 filter from two bilinear taps), vertically between the two rows.
float2 ps_uv(VsOut i) : SV_Target
{
    float x0 = 2.0 * i.pos.x - 1.0;
    float y = 2.0 * i.pos.y;
    float3 a = tex0.SampleLevel(linear_clamp, float2(x0, y) * canvas_texel, 0).rgb;
    float3 b = tex0.SampleLevel(linear_clamp, float2(x0 + 1.0, y) * canvas_texel, 0).rgb;
    float3 rgb = (a + b) * 0.5;
    return float2(dot(rgb, u_coef.xyz) + u_coef.w, dot(rgb, v_coef.xyz) + v_coef.w);
}
