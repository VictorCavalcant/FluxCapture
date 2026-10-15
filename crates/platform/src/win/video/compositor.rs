//! GPU programs and draw calls: layer composition, preview scaling and RGB → NV12 conversion.

use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BUFFER_DESC,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE, D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC, D3D11_TEX2D_RTV,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT, ID3D11BlendState, ID3D11Buffer, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

use super::super::Context;
use super::super::d3d::{Gpu, compile};
use crate::{PlatformError, ScaleFilter};

const SHADERS: &str = include_str!("shaders.hlsl");

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct LayerCb {
    dst: [f32; 4],
    src: [f32; 4],
    color: [f32; 4],
    lod: f32,
    opaque: f32,
    pad: [f32; 2],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct ConvertCb {
    y: [f32; 4],
    u: [f32; 4],
    v: [f32; 4],
    texel: [f32; 2],
    pad: [f32; 2],
}

/// One textured draw.
pub struct Quad<'a> {
    pub srv: &'a ID3D11ShaderResourceView,
    /// Destination in target pixels: x0, y0, x1, y1.
    pub dst: [f32; 4],
    /// Source UV rectangle: u0, v0, u1, v1.
    pub src: [f32; 4],
    pub opacity: f32,
    pub opaque: bool,
    pub lod: f32,
    pub filter: ScaleFilter,
}

/// Compiled programs and fixed pipeline state.
pub struct Compositor {
    vs_quad: ID3D11VertexShader,
    vs_full: ID3D11VertexShader,
    ps_bilinear: ID3D11PixelShader,
    ps_bicubic: ID3D11PixelShader,
    ps_lanczos: ID3D11PixelShader,
    ps_color: ID3D11PixelShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    linear: ID3D11SamplerState,
    point: ID3D11SamplerState,
    premultiplied: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    layer_cb: ID3D11Buffer,
    convert_cb: ID3D11Buffer,
}

fn constant_buffer(gpu: &Gpu, size: u32) -> crate::Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: size.next_multiple_of(16),
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut b = None;
    // SAFETY: valid descriptor, live out-local.
    unsafe { gpu.device.CreateBuffer(&desc, None, Some(&mut b)) }.ctx("CreateBuffer")?;
    b.ok_or_else(|| PlatformError("CreateBuffer returned nothing".into()))
}

fn sampler(gpu: &Gpu, linear: bool) -> crate::Result<ID3D11SamplerState> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: if linear { D3D11_FILTER_MIN_MAG_MIP_LINEAR } else { D3D11_FILTER_MIN_MAG_MIP_POINT },
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    let mut s = None;
    // SAFETY: valid descriptor, live out-local.
    unsafe { gpu.device.CreateSamplerState(&desc, Some(&mut s)) }.ctx("CreateSamplerState")?;
    s.ok_or_else(|| PlatformError("CreateSamplerState returned nothing".into()))
}

impl Compositor {
    pub fn new(gpu: &Gpu) -> crate::Result<Compositor> {
        let vs = |entry: &str| -> crate::Result<ID3D11VertexShader> {
            let code = compile(SHADERS, entry, "vs_4_0")?;
            let mut s = None;
            // SAFETY: bytecode just produced by the compiler; live out-local.
            unsafe { gpu.device.CreateVertexShader(&code, None, Some(&mut s)) }.ctx("CreateVertexShader")?;
            s.ok_or_else(|| PlatformError("CreateVertexShader returned nothing".into()))
        };
        let ps = |entry: &str| -> crate::Result<ID3D11PixelShader> {
            let code = compile(SHADERS, entry, "ps_4_0")?;
            let mut s = None;
            // SAFETY: bytecode just produced by the compiler; live out-local.
            unsafe { gpu.device.CreatePixelShader(&code, None, Some(&mut s)) }.ctx("CreatePixelShader")?;
            s.ok_or_else(|| PlatformError("CreatePixelShader returned nothing".into()))
        };
        let mut blend_desc = D3D11_BLEND_DESC::default();
        blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_ONE,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut premultiplied = None;
        // SAFETY: valid descriptor, live out-local.
        unsafe { gpu.device.CreateBlendState(&blend_desc, Some(&mut premultiplied)) }.ctx("CreateBlendState")?;
        let raster_desc = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: true.into(), ..Default::default() };
        let mut raster = None;
        // SAFETY: valid descriptor, live out-local.
        unsafe { gpu.device.CreateRasterizerState(&raster_desc, Some(&mut raster)) }.ctx("CreateRasterizerState")?;
        Ok(Compositor {
            vs_quad: vs("vs_quad")?,
            vs_full: vs("vs_full")?,
            ps_bilinear: ps("ps_bilinear")?,
            ps_bicubic: ps("ps_bicubic")?,
            ps_lanczos: ps("ps_lanczos")?,
            ps_color: ps("ps_color")?,
            ps_y: ps("ps_y")?,
            ps_uv: ps("ps_uv")?,
            linear: sampler(gpu, true)?,
            point: sampler(gpu, false)?,
            premultiplied: premultiplied.ok_or("CreateBlendState returned nothing")?,
            raster: raster.ok_or("CreateRasterizerState returned nothing")?,
            layer_cb: constant_buffer(gpu, std::mem::size_of::<LayerCb>() as u32)?,
            convert_cb: constant_buffer(gpu, std::mem::size_of::<ConvertCb>() as u32)?,
        })
    }

    fn upload<T: Copy>(gpu: &Gpu, buf: &ID3D11Buffer, value: &T) -> crate::Result<()> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: a dynamic constant buffer at least size_of::<T>() bytes long (rounded up to 16);
        // WRITE_DISCARD gives a fresh writable block, unmapped right after the copy.
        unsafe {
            gpu.context.Map(buf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).ctx("Map (constants)")?;
            if mapped.pData.is_null() {
                gpu.context.Unmap(buf, 0);
                return Err(PlatformError("Map returned no memory".into()));
            }
            std::ptr::copy_nonoverlapping((value as *const T).cast::<u8>(), mapped.pData.cast::<u8>(), std::mem::size_of::<T>());
            gpu.context.Unmap(buf, 0);
        }
        Ok(())
    }

    /// Binds `rtv` as the only render target with a full viewport, and the shared state.
    pub fn begin(&self, gpu: &Gpu, rtv: &ID3D11RenderTargetView, width: u32, height: u32, clear: Option<[f32; 4]>) {
        let vp = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: width as f32, Height: height as f32, MinDepth: 0.0, MaxDepth: 1.0 };
        // SAFETY: plain state setting on the immediate context with live objects.
        unsafe {
            gpu.context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            gpu.context.RSSetViewports(Some(&[vp]));
            gpu.context.RSSetState(&self.raster);
            gpu.context.IASetInputLayout(None);
            gpu.context.PSSetSamplers(0, Some(&[Some(self.linear.clone()), Some(self.point.clone())]));
            if let Some(c) = clear {
                gpu.context.ClearRenderTargetView(rtv, &c);
            }
        }
    }

    /// Draws a textured quad (premultiplied blending) into the bound target of size `w`×`h`.
    pub fn draw_quad(&self, gpu: &Gpu, q: &Quad<'_>, w: u32, h: u32) -> crate::Result<()> {
        let cb = LayerCb {
            dst: to_ndc(q.dst, w, h),
            src: q.src,
            color: [1.0, 1.0, 1.0, q.opacity.clamp(0.0, 1.0)],
            lod: q.lod.max(0.0),
            opaque: if q.opaque { 1.0 } else { 0.0 },
            pad: [0.0; 2],
        };
        Self::upload(gpu, &self.layer_cb, &cb)?;
        let ps = match q.filter {
            ScaleFilter::Bilinear => &self.ps_bilinear,
            ScaleFilter::Bicubic => &self.ps_bicubic,
            ScaleFilter::Lanczos => &self.ps_lanczos,
        };
        // SAFETY: state setting + draw with live objects; 4 vertices of a strip generated in the VS.
        unsafe {
            gpu.context.OMSetBlendState(&self.premultiplied, None, u32::MAX);
            gpu.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            gpu.context.VSSetShader(&self.vs_quad, None);
            gpu.context.VSSetConstantBuffers(0, Some(&[Some(self.layer_cb.clone())]));
            gpu.context.PSSetShader(ps, None);
            gpu.context.PSSetConstantBuffers(0, Some(&[Some(self.layer_cb.clone())]));
            gpu.context.PSSetShaderResources(0, Some(&[Some(q.srv.clone())]));
            gpu.context.Draw(4, 0);
            gpu.context.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }

    /// Fills a rectangle with a straight-alpha colour.
    pub fn draw_color(&self, gpu: &Gpu, dst: [f32; 4], rgba: [f32; 4], w: u32, h: u32) -> crate::Result<()> {
        let cb =
            LayerCb { dst: to_ndc(dst, w, h), src: [0.0; 4], color: rgba.map(|c| if c.is_finite() { c.clamp(0.0, 1.0) } else { 0.0 }), ..Default::default() };
        Self::upload(gpu, &self.layer_cb, &cb)?;
        // SAFETY: as in draw_quad.
        unsafe {
            gpu.context.OMSetBlendState(&self.premultiplied, None, u32::MAX);
            gpu.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            gpu.context.VSSetShader(&self.vs_quad, None);
            gpu.context.VSSetConstantBuffers(0, Some(&[Some(self.layer_cb.clone())]));
            gpu.context.PSSetShader(&self.ps_color, None);
            gpu.context.PSSetConstantBuffers(0, Some(&[Some(self.layer_cb.clone())]));
            gpu.context.Draw(4, 0);
        }
        Ok(())
    }

    /// Converts the canvas into an NV12 surface (BT.709; TV or full range).
    pub fn convert_nv12(&self, gpu: &Gpu, canvas: &ID3D11ShaderResourceView, target: &Nv12Target, full_range: bool) -> crate::Result<()> {
        let (w, h) = (target.width, target.height);
        let (kr, kb) = (0.2126f32, 0.0722f32);
        let kg = 1.0 - kr - kb;
        let (ys, yo, cs, co) = if full_range { (1.0, 0.0, 1.0, 128.0 / 255.0) } else { (219.0 / 255.0, 16.0 / 255.0, 224.0 / 255.0, 128.0 / 255.0) };
        let cb_scale = cs / (2.0 * (1.0 - kb));
        let cr_scale = cs / (2.0 * (1.0 - kr));
        let cb = ConvertCb {
            y: [kr * ys, kg * ys, kb * ys, yo],
            u: [-kr * cb_scale, -kg * cb_scale, (1.0 - kb) * cb_scale, co],
            v: [(1.0 - kr) * cr_scale, -kg * cr_scale, -kb * cr_scale, co],
            texel: [1.0 / w.max(1) as f32, 1.0 / h.max(1) as f32],
            pad: [0.0; 2],
        };
        Self::upload(gpu, &self.convert_cb, &cb)?;
        let vp_y = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 };
        let vp_uv = D3D11_VIEWPORT { Width: (w / 2) as f32, Height: (h / 2) as f32, ..vp_y };
        // SAFETY: state setting + two full-screen-triangle draws with live objects.
        unsafe {
            let c = &gpu.context;
            c.OMSetBlendState(None, None, u32::MAX);
            c.RSSetState(&self.raster);
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs_full, None);
            c.PSSetConstantBuffers(1, Some(&[Some(self.convert_cb.clone())]));
            c.PSSetSamplers(0, Some(&[Some(self.linear.clone()), Some(self.point.clone())]));
            // Unbind the canvas as a render target *before* binding it as a shader input:
            // D3D11 silently nulls an SRV whose resource is still bound for output.
            c.OMSetRenderTargets(Some(&[Some(target.y.clone())]), None);
            c.PSSetShaderResources(0, Some(&[Some(canvas.clone())]));
            c.RSSetViewports(Some(&[vp_y]));
            c.PSSetShader(&self.ps_y, None);
            c.Draw(3, 0);
            c.OMSetRenderTargets(Some(&[Some(target.uv.clone())]), None);
            c.RSSetViewports(Some(&[vp_uv]));
            c.PSSetShader(&self.ps_uv, None);
            c.Draw(3, 0);
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

/// Pixel rectangle → NDC (y up).
fn to_ndc(r: [f32; 4], w: u32, h: u32) -> [f32; 4] {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    [r[0] / w * 2.0 - 1.0, 1.0 - r[1] / h * 2.0, r[2] / w * 2.0 - 1.0, 1.0 - r[3] / h * 2.0]
}

/// An NV12 surface with render-target views on both planes.
pub struct Nv12Target {
    pub texture: ID3D11Texture2D,
    pub y: ID3D11RenderTargetView,
    pub uv: ID3D11RenderTargetView,
    pub width: u32,
    pub height: u32,
}

/// A render-target view of one plane / format of `tex`.
pub fn rtv(gpu: &Gpu, tex: &ID3D11Texture2D, format: Option<DXGI_FORMAT>) -> crate::Result<ID3D11RenderTargetView> {
    let mut v = None;
    match format {
        Some(f) => {
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: f,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
            };
            // SAFETY: valid descriptor for a texture created with RENDER_TARGET; live out-local.
            unsafe { gpu.device.CreateRenderTargetView(tex, Some(&desc), Some(&mut v)) }.ctx("CreateRenderTargetView")?;
        }
        // SAFETY: default view of a RENDER_TARGET texture; live out-local.
        None => unsafe { gpu.device.CreateRenderTargetView(tex, None, Some(&mut v)) }.ctx("CreateRenderTargetView")?,
    }
    v.ok_or_else(|| PlatformError("CreateRenderTargetView returned nothing".into()))
}

/// A default shader-resource view of `tex`.
pub fn srv(gpu: &Gpu, tex: &ID3D11Texture2D) -> crate::Result<ID3D11ShaderResourceView> {
    let mut v = None;
    // SAFETY: default view of a SHADER_RESOURCE texture; live out-local.
    unsafe { gpu.device.CreateShaderResourceView(tex, None, Some(&mut v)) }.ctx("CreateShaderResourceView")?;
    v.ok_or_else(|| PlatformError("CreateShaderResourceView returned nothing".into()))
}
