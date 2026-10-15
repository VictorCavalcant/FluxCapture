//! Scenes (settings) → compositor layers (platform), with image and text caches.
//!
//! Images are decoded once per file version and text is rasterised once per style; the resulting
//! pixels are shared (`Arc`), so re-sending an unchanged scene never re-uploads a texture.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use fluxcapture_platform::{CaptureMethod, CaptureTarget, Layer, LayerSource, MonitorInfo, RectF, RgbaImage};
use fluxcapture_settings::{Scene, SourceKind, TextStyle};

#[derive(Default)]
pub struct SceneCache {
    images: HashMap<PathBuf, (Option<SystemTime>, Result<RgbaImage, String>)>,
    texts: HashMap<u64, RgbaImage>,
    fonts: HashMap<(String, bool), Option<Arc<FontVec>>>,
}

/// Errors per source id (images that do not load, fonts missing…).
pub type SourceErrors = Vec<(u64, String)>;

impl SceneCache {
    /// The layers of `scene` for a `w`×`h` canvas. `method` overrides each capture's method unless
    /// it is `Auto`.
    pub fn layers(&mut self, scene: &Scene, w: u32, h: u32, method: CaptureMethod) -> (Vec<Layer>, SourceErrors) {
        let mut layers = Vec::with_capacity(scene.sources.len());
        let mut errors = Vec::new();
        let (wf, hf) = (w as f32, h as f32);
        for s in &scene.sources {
            let rect = RectF { x: s.rect.x * wf, y: s.rect.y * hf, w: s.rect.w * wf, h: s.rect.h * hf };
            let source = match &s.kind {
                SourceKind::Display { monitor, options } => {
                    let mut options = options.clone();
                    if method != CaptureMethod::Auto {
                        options.method = method;
                    }
                    LayerSource::Capture { target: CaptureTarget::Monitor { id: monitor.clone() }, options }
                }
                SourceKind::Window { handle, title, process, options } => LayerSource::Capture {
                    target: CaptureTarget::Window { handle: *handle, title: title.clone(), process: process.clone() },
                    // Desktop Duplication cannot capture a window.
                    options: fluxcapture_platform::CaptureOptions { method: CaptureMethod::Wgc, ..options.clone() },
                },
                SourceKind::Image { path } => match self.image(path) {
                    Ok(img) => LayerSource::Image(img),
                    Err(e) => {
                        errors.push((s.id, e));
                        continue;
                    }
                },
                SourceKind::Text(style) => match self.text(style) {
                    Ok(img) => LayerSource::Image(img),
                    Err(e) => {
                        errors.push((s.id, e));
                        continue;
                    }
                },
                SourceKind::Color { rgba } => LayerSource::Color(*rgba),
                SourceKind::Webcam { device, width, height, fps } => LayerSource::Webcam { device: device.clone(), width: *width, height: *height, fps: *fps },
            };
            layers.push(Layer { id: s.id, source, rect, fit: s.fit, crop: s.crop, opacity: s.opacity, visible: s.visible });
        }
        (layers, errors)
    }

    fn image(&mut self, path: &Path) -> Result<RgbaImage, String> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if let Some((t, r)) = self.images.get(path)
            && *t == mtime
        {
            return r.clone();
        }
        let r = load_image(path);
        if self.images.len() > 64 {
            self.images.clear();
        }
        self.images.insert(path.to_path_buf(), (mtime, r.clone()));
        r
    }

    fn font(&mut self, family: &str, bold: bool) -> Option<Arc<FontVec>> {
        self.fonts.entry((family.to_lowercase(), bold)).or_insert_with(|| find_font(family, bold).map(Arc::new)).clone()
    }

    fn text(&mut self, style: &TextStyle) -> Result<RgbaImage, String> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        style.text.hash(&mut h);
        style.font.hash(&mut h);
        style.size_px.to_bits().hash(&mut h);
        style.color.hash(&mut h);
        style.background.hash(&mut h);
        style.outline.hash(&mut h);
        style.bold.hash(&mut h);
        let key = h.finish();
        if let Some(img) = self.texts.get(&key) {
            return Ok(img.clone());
        }
        let font = self
            .font(&style.font, style.bold)
            .or_else(|| self.font("Segoe UI", style.bold))
            .or_else(|| self.font("Arial", false))
            .ok_or("no usable font found in the system fonts folder")?;
        let img = rasterize(&font, style)?;
        if self.texts.len() > 128 {
            self.texts.clear();
        }
        self.texts.insert(key, img.clone());
        Ok(img)
    }
}

/// Decodes an image file (PNG, JPEG, BMP, GIF, WebP), bounded to 16k × 16k and 256 MB.
pub fn load_image(path: &Path) -> Result<RgbaImage, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > 256 * 1024 * 1024 {
        return Err(format!("{}: file too large", path.display()));
    }
    let mut reader = image::ImageReader::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .with_guessed_format()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(1024 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| format!("{}: {e}", path.display()))?.to_rgba8();
    let (w, h) = img.dimensions();
    RgbaImage::new(w, h, img.into_raw()).ok_or_else(|| format!("{}: unsupported size", path.display()))
}

/// Looks a font family up in the Windows fonts folder.
fn find_font(family: &str, bold: bool) -> Option<FontVec> {
    let dir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows")).join("Fonts");
    let f = family.to_lowercase();
    let known: &[(&str, &str, &str)] = &[
        ("segoe ui", "segoeui.ttf", "segoeuib.ttf"),
        ("arial", "arial.ttf", "arialbd.ttf"),
        ("calibri", "calibri.ttf", "calibrib.ttf"),
        ("consolas", "consola.ttf", "consolab.ttf"),
        ("verdana", "verdana.ttf", "verdanab.ttf"),
        ("tahoma", "tahoma.ttf", "tahomabd.ttf"),
        ("times new roman", "times.ttf", "timesbd.ttf"),
        ("georgia", "georgia.ttf", "georgiab.ttf"),
        ("trebuchet ms", "trebuc.ttf", "trebucbd.ttf"),
        ("impact", "impact.ttf", "impact.ttf"),
        ("comic sans ms", "comic.ttf", "comicbd.ttf"),
        ("cascadia code", "CascadiaCode.ttf", "CascadiaCode.ttf"),
    ];
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some((_, regular, b)) = known.iter().find(|(n, _, _)| *n == f) {
        candidates.push(dir.join(if bold { b } else { regular }));
        candidates.push(dir.join(regular));
    }
    // Any file whose name starts like the family ("roboto" → Roboto-Regular.ttf).
    let compact: String = f.chars().filter(|c| c.is_alphanumeric()).collect();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten().take(5000) {
            let name = e.file_name().to_string_lossy().to_lowercase();
            let is_font = name.ends_with(".ttf") || name.ends_with(".otf");
            if is_font && !compact.is_empty() && name.starts_with(&compact) {
                let wants_bold = name.contains("bold") || name.contains("bd.");
                if wants_bold == bold {
                    candidates.insert(candidates.len().min(1), e.path());
                } else {
                    candidates.push(e.path());
                }
            }
        }
    }
    candidates.into_iter().find_map(|p| {
        let data = std::fs::read(&p).ok()?;
        if data.len() > 64 * 1024 * 1024 {
            return None;
        }
        FontVec::try_from_vec(data).ok()
    })
}

/// Renders multi-line text with optional outline and background into straight RGBA.
fn rasterize(font: &FontVec, style: &TextStyle) -> Result<RgbaImage, String> {
    let scale = PxScale::from(style.size_px.clamp(6.0, 512.0));
    let sf = font.as_scaled(scale);
    let pad = (style.size_px * 0.25).ceil().clamp(2.0, 64.0) as i32;
    let outline = if style.outline.is_some() { (style.size_px / 16.0).ceil().clamp(1.0, 12.0) as i32 } else { 0 };
    let line_h = (sf.ascent() - sf.descent() + sf.line_gap()).ceil().max(1.0);
    let lines: Vec<&str> = style.text.lines().take(256).collect();
    let lines = if lines.is_empty() { vec![" "] } else { lines };
    // Measure.
    let width_of = |line: &str| -> f32 {
        let mut x = 0.0;
        let mut prev = None;
        for c in line.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            x += sf.h_advance(id);
            prev = Some(id);
        }
        x
    };
    let text_w = lines.iter().map(|l| width_of(l)).fold(1.0f32, f32::max).ceil() as i32;
    let w = (text_w + 2 * (pad + outline)).clamp(1, 8192);
    let h = ((line_h * lines.len() as f32).ceil() as i32 + 2 * (pad + outline)).clamp(1, 8192);
    let (wu, hu) = (w as usize, h as usize);
    let mut coverage = vec![0f32; wu * hu];
    for (li, line) in lines.iter().enumerate() {
        let mut x = (pad + outline) as f32;
        let baseline = (pad + outline) as f32 + sf.ascent() + li as f32 * line_h;
        let mut prev = None;
        for c in line.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scale, ab_glyph::point(x, baseline));
            x += sf.h_advance(id);
            prev = Some(id);
            if let Some(og) = font.outline_glyph(glyph) {
                let b = og.px_bounds();
                og.draw(|gx, gy, v| {
                    let px = b.min.x as i32 + gx as i32;
                    let py = b.min.y as i32 + gy as i32;
                    if px >= 0
                        && py >= 0
                        && px < w
                        && py < h
                        && let Some(c) = coverage.get_mut(py as usize * wu + px as usize)
                    {
                        *c = (*c + v).min(1.0);
                    }
                });
            }
        }
    }
    // Outline = dilated coverage.
    let outline_cov: Option<Vec<f32>> = (outline > 0).then(|| {
        let mut o = vec![0f32; wu * hu];
        for y in 0..h {
            for x in 0..w {
                let mut m = 0f32;
                for dy in -outline..=outline {
                    for dx in -outline..=outline {
                        if dx * dx + dy * dy > outline * outline {
                            continue;
                        }
                        let (sx, sy) = (x + dx, y + dy);
                        if sx >= 0 && sy >= 0 && sx < w && sy < h {
                            m = m.max(coverage.get(sy as usize * wu + sx as usize).copied().unwrap_or(0.0));
                        }
                    }
                }
                if let Some(v) = o.get_mut(y as usize * wu + x as usize) {
                    *v = m;
                }
            }
        }
        o
    });
    let mut px = vec![0u8; wu * hu * 4];
    let over = |dst: [f32; 4], src: [f32; 4]| -> [f32; 4] {
        let a = src[3] + dst[3] * (1.0 - src[3]);
        if a <= 0.0 {
            return [0.0; 4];
        }
        let mix = |i: usize| (src[i] * src[3] + dst[i] * dst[3] * (1.0 - src[3])) / a;
        [mix(0), mix(1), mix(2), a]
    };
    let to_f = |c: [u8; 4]| c.map(|v| f32::from(v) / 255.0);
    for i in 0..wu * hu {
        let mut c = style.background.map(to_f).unwrap_or([0.0; 4]);
        if let (Some(oc), Some(ov)) = (style.outline, outline_cov.as_ref()) {
            let mut o = to_f(oc);
            o[3] *= ov.get(i).copied().unwrap_or(0.0);
            c = over(c, o);
        }
        let mut t = to_f(style.color);
        t[3] *= coverage.get(i).copied().unwrap_or(0.0);
        c = over(c, t);
        if let Some(d) = px.get_mut(i * 4..i * 4 + 4) {
            for (k, v) in c.iter().enumerate() {
                d[k] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
    }
    RgbaImage::new(w as u32, h as u32, px).ok_or_else(|| "text image too large".into())
}

/// Native output size: the monitor of the first display source, else the size of the first window
/// source, else the primary monitor.
pub fn native_size(scene: Option<&Scene>, monitors: &[MonitorInfo], windows: &[fluxcapture_platform::WindowInfo]) -> (u32, u32) {
    let primary = monitors.iter().find(|m| m.primary).or(monitors.first()).map_or((1920, 1080), |m| (m.width, m.height));
    let Some(scene) = scene else { return primary };
    for s in scene.sources.iter().filter(|s| s.visible) {
        match &s.kind {
            SourceKind::Display { monitor, .. } => {
                let m = if monitor.is_empty() { monitors.iter().find(|m| m.primary) } else { monitors.iter().find(|m| &m.id == monitor) };
                if let Some(m) = m {
                    let cw = m.width.saturating_sub(s.crop.left + s.crop.right);
                    let ch = m.height.saturating_sub(s.crop.top + s.crop.bottom);
                    if cw >= 16 && ch >= 16 {
                        return (cw, ch);
                    }
                    return (m.width, m.height);
                }
            }
            SourceKind::Window { handle, process, .. } => {
                if let Some(w) = windows.iter().find(|w| w.handle == *handle || (!process.is_empty() && w.process.eq_ignore_ascii_case(process)))
                    && w.width >= 16
                    && w.height >= 16
                {
                    return (w.width, w.height);
                }
            }
            _ => {}
        }
    }
    primary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_renders_with_a_system_font() {
        let mut c = SceneCache::default();
        let img = c.text(&TextStyle { text: "Olá\nFluxCapture".into(), ..TextStyle::default() });
        let Ok(img) = img else { return }; // no fonts folder on this machine
        assert!(img.width > 100 && img.height > 80);
        assert!(img.data.chunks(4).any(|p| p[3] == 255 && p[0] == 255));
        // Cached: same pixels object.
        let again = c.text(&TextStyle { text: "Olá\nFluxCapture".into(), ..TextStyle::default() }).unwrap();
        assert!(Arc::ptr_eq(&img.data, &again.data));
    }

    #[test]
    fn missing_image_is_an_error() {
        assert!(load_image(Path::new("Z:\\nope\\missing.png")).is_err());
        let mut scene = Scene::default_desktop();
        scene.sources.push(fluxcapture_settings::SceneSource {
            id: 9,
            name: "img".into(),
            kind: SourceKind::Image { path: "Z:\\nope.png".into() },
            rect: RectF { x: 0.0, y: 0.0, w: 0.5, h: 0.5 },
            fit: fluxcapture_platform::Fit::Contain,
            crop: Default::default(),
            opacity: 1.0,
            visible: true,
        });
        let (layers, errors) = SceneCache::default().layers(&scene, 1920, 1080, CaptureMethod::Auto);
        assert_eq!(layers.len(), 1);
        assert_eq!(errors.len(), 1);
        assert_eq!(layers[0].rect.w, 1920.0);
    }
}
