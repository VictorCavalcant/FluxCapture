//! The preview: the downscaled canvas, with direct manipulation of the selected source (move,
//! resize) and region selection for display captures.

use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use fluxcapture_platform::{Crop, Fit, RectF};
use fluxcapture_settings::SourceKind;

use crate::app::FluxApp;
use crate::theme;

#[derive(Clone, Debug)]
pub enum Drag {
    Move { id: u64, start: RectF, from: Pos2 },
    Resize { id: u64, start: RectF, from: Pos2 },
    Region { from: Pos2, to: Pos2 },
}

fn fit_rect(avail: Rect, aspect: f32) -> Rect {
    let a = if aspect.is_finite() && aspect > 0.0 { aspect } else { 16.0 / 9.0 };
    let (w, h) = if avail.width() / avail.height().max(1.0) > a { (avail.height() * a, avail.height()) } else { (avail.width(), avail.width() / a) };
    Rect::from_center_size(avail.center(), vec2(w, h))
}

fn to_screen(r: RectF, canvas: Rect) -> Rect {
    Rect::from_min_size(pos2(canvas.left() + r.x * canvas.width(), canvas.top() + r.y * canvas.height()), vec2(r.w * canvas.width(), r.h * canvas.height()))
}

pub fn show(app: &mut FluxApp, ui: &mut egui::Ui) {
    // Upload a new preview frame if there is one.
    if let Some(f) = app.engine.preview()
        && f.seq != app.preview_seq
        && f.rgba.len() == f.width as usize * f.height as usize * 4
    {
        app.preview_seq = f.seq;
        let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
        match app.preview_tex.as_mut() {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => app.preview_tex = Some(ui.ctx().load_texture("preview", img, egui::TextureOptions::LINEAR)),
        }
    }
    let st = app.engine.state();
    let (ow, oh) = st.output;
    let avail = ui.available_rect_before_wrap();
    let canvas = fit_rect(avail.shrink(4.0), ow.max(1) as f32 / oh.max(1) as f32);
    let painter = ui.painter_at(avail);
    painter.rect_filled(canvas, 2.0, Color32::BLACK);
    let p = &app.engine.settings().profile().preview;
    let paused_for_rec = app.engine.is_recording() && p.removed_while_recording();
    if let Some(err) = &st.video_error {
        painter.text(canvas.center(), egui::Align2::CENTER_CENTER, err, egui::FontId::proportional(14.0), theme::REC);
    } else if !p.enabled || paused_for_rec {
        let msg = if paused_for_rec && p.high_quality {
            app.tr("Prévia em alta qualidade removida durante a gravação", "High-quality preview removed while recording")
        } else if paused_for_rec {
            app.tr("Prévia pausada durante a gravação (economia)", "Preview paused while recording (economy)")
        } else {
            app.tr("Prévia desligada", "Preview off")
        };
        painter.text(canvas.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(15.0), theme::MUTED);
    } else if let Some(t) = &app.preview_tex {
        painter.image(t.id(), canvas, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    } else {
        painter.text(
            canvas.center(),
            egui::Align2::CENTER_CENTER,
            app.tr("Iniciando captura…", "Starting capture…"),
            egui::FontId::proportional(15.0),
            theme::MUTED,
        );
    }
    let resp = ui.allocate_rect(avail, Sense::click_and_drag());
    let Some(scene) = app.engine.settings().scene().cloned() else { return };
    // Outlines.
    for s in scene.sources.iter().filter(|s| s.visible) {
        let r = to_screen(s.rect, canvas);
        let selected = Some(s.id) == app.selected_source;
        if selected {
            painter.rect_stroke(r, 0.0, Stroke::new(2.0, theme::ACCENT), StrokeKind::Inside);
            let h = Rect::from_center_size(r.right_bottom(), vec2(10.0, 10.0));
            painter.rect_filled(h, 2.0, theme::ACCENT);
            painter.text(r.left_top() + vec2(4.0, 2.0), egui::Align2::LEFT_TOP, &s.name, egui::FontId::proportional(12.0), theme::ACCENT);
        } else if resp.hovered() {
            painter.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_white_alpha(60)), StrokeKind::Inside);
        }
    }
    let to_canvas = |p: Pos2| -> (f32, f32) { ((p.x - canvas.left()) / canvas.width().max(1.0), (p.y - canvas.top()) / canvas.height().max(1.0)) };
    // Region selection.
    if app.region_mode {
        painter.rect_filled(canvas, 0.0, Color32::from_black_alpha(80));
        painter.text(
            canvas.center_top() + vec2(0.0, 16.0),
            egui::Align2::CENTER_TOP,
            app.tr("Arraste para selecionar a região · Esc cancela", "Drag to select the region · Esc cancels"),
            egui::FontId::proportional(15.0),
            Color32::WHITE,
        );
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            app.region_mode = false;
            app.drag = None;
        }
        if resp.drag_started()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            app.drag = Some(Drag::Region { from: pos, to: pos });
        }
        if let Some(Drag::Region { to, .. }) = app.drag.as_mut()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            *to = pos;
        }
        if let Some(Drag::Region { from, to }) = &app.drag {
            let r = Rect::from_two_pos(*from, *to).intersect(canvas);
            painter.rect_stroke(r, 0.0, Stroke::new(2.0, theme::REC), StrokeKind::Inside);
            if resp.drag_stopped() {
                let (x0, y0) = to_canvas(r.min);
                let (x1, y1) = to_canvas(r.max);
                apply_region(app, (x0, y0, x1, y1));
                app.region_mode = false;
                app.drag = None;
            }
        }
        return;
    }
    // Select / move / resize.
    if resp.drag_started()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let sel = app.selected_source.and_then(|id| scene.sources.iter().find(|s| s.id == id));
        let handle_hit = sel.is_some_and(|s| Rect::from_center_size(to_screen(s.rect, canvas).right_bottom(), vec2(16.0, 16.0)).contains(pos));
        if let (true, Some(s)) = (handle_hit, sel) {
            app.drag = Some(Drag::Resize { id: s.id, start: s.rect, from: pos });
        } else if let Some(s) = scene.sources.iter().rev().find(|s| s.visible && to_screen(s.rect, canvas).contains(pos)) {
            app.selected_source = Some(s.id);
            app.drag = Some(Drag::Move { id: s.id, start: s.rect, from: pos });
        }
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        app.selected_source = scene.sources.iter().rev().find(|s| s.visible && to_screen(s.rect, canvas).contains(pos)).map(|s| s.id);
    }
    if let (Some(drag), Some(pos)) = (app.drag.clone(), resp.interact_pointer_pos()) {
        let snap = |v: f32| if (v - v.round()).abs() < 0.01 { v.round() } else { v };
        match drag {
            Drag::Move { id, start, from } => {
                let dx = (pos.x - from.x) / canvas.width().max(1.0);
                let dy = (pos.y - from.y) / canvas.height().max(1.0);
                let r = RectF { x: snap(start.x + dx), y: snap(start.y + dy), ..start };
                set_rect(app, id, r);
            }
            Drag::Resize { id, start, from } => {
                let dx = (pos.x - from.x) / canvas.width().max(1.0);
                let dy = (pos.y - from.y) / canvas.height().max(1.0);
                let shift = ui.input(|i| i.modifiers.shift);
                let (mut w, mut h) = ((start.w + dx).max(0.02), (start.h + dy).max(0.02));
                if !shift && start.h > 0.0 {
                    // Keep the aspect ratio unless Shift is held.
                    let a = start.w / start.h;
                    if dx.abs() > dy.abs() { h = w / a } else { w = h * a }
                }
                set_rect(app, id, RectF { w: snap(start.x + w) - start.x, h: snap(start.y + h) - start.y, ..start });
            }
            Drag::Region { .. } => {}
        }
        if resp.drag_stopped() {
            app.drag = None;
        }
    }
    if ui.input(|i| i.key_pressed(egui::Key::Delete))
        && let Some(id) = app.selected_source
        && !ui.ctx().egui_wants_keyboard_input()
    {
        app.engine.update_settings(|s| s.scene_mut().sources.retain(|x| x.id != id));
        app.selected_source = None;
    }
    // Hint line.
    if app.selected_source.is_none() && !scene.sources.is_empty() && resp.hovered() {
        painter.text(
            canvas.left_bottom() + vec2(6.0, -6.0),
            egui::Align2::LEFT_BOTTOM,
            app.tr(
                "Clique numa fonte para selecionar · arraste para mover · canto para redimensionar",
                "Click a source to select · drag to move · corner to resize",
            ),
            egui::FontId::proportional(11.0),
            Color32::from_white_alpha(120),
        );
    }
}

fn set_rect(app: &mut FluxApp, id: u64, r: RectF) {
    app.engine.update_settings(|s| {
        if let Some(src) = s.scene_mut().sources.iter_mut().find(|x| x.id == id) {
            src.rect = r;
        }
    });
}

/// Starts region selection for a display source: show the whole monitor, then crop to the drag.
pub fn begin_region(app: &mut FluxApp, id: u64) {
    app.engine.update_settings(|s| {
        if let Some(src) = s.scene_mut().sources.iter_mut().find(|x| x.id == id) {
            src.crop = Crop::default();
            src.rect = RectF { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };
            src.fit = Fit::Contain;
        }
    });
    app.selected_source = Some(id);
    app.region_mode = true;
}

fn apply_region(app: &mut FluxApp, sel: (f32, f32, f32, f32)) {
    let Some(id) = app.selected_source else { return };
    let monitors = app.engine.devices().monitors.clone();
    let (cw, ch) = app.engine.state().output;
    app.engine.update_settings(|s| {
        let Some(src) = s.scene_mut().sources.iter_mut().find(|x| x.id == id) else { return };
        let SourceKind::Display { monitor, .. } = &src.kind else { return };
        let m = if monitor.is_empty() { monitors.iter().find(|m| m.primary) } else { monitors.iter().find(|m| &m.id == monitor) };
        let Some(m) = m else { return };
        let (mw, mh, cw, ch) = (m.width as f32, m.height as f32, cw.max(1) as f32, ch.max(1) as f32);
        // Where the whole monitor sits in the canvas (fit = contain).
        let sc = (cw / mw).min(ch / mh);
        let (ox, oy) = ((cw - mw * sc) / 2.0 / cw, (ch - mh * sc) / 2.0 / ch);
        let (fw, fh) = (mw * sc / cw, mh * sc / ch);
        let to_px = |fx: f32, fy: f32| (((fx - ox) / fw * mw).clamp(0.0, mw), ((fy - oy) / fh * mh).clamp(0.0, mh));
        let (x0, y0) = to_px(sel.0, sel.1);
        let (x1, y1) = to_px(sel.2, sel.3);
        if x1 - x0 < 16.0 || y1 - y0 < 16.0 {
            return;
        }
        // Even sizes keep the encoder happy when the output is "native".
        let (l, t) = (x0.round() as u32, y0.round() as u32);
        let (w, h) = (((x1 - x0).round() as u32) & !1, ((y1 - y0).round() as u32) & !1);
        src.crop = Crop { left: l, top: t, right: m.width.saturating_sub(l + w), bottom: m.height.saturating_sub(t + h) };
    });
}
