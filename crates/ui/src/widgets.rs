//! Small custom widgets: level meters, sparklines, stat rows, the status pill.

use egui::{Color32, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2, pos2, vec2};
use fluxcapture_audio::LevelMeter;
use fluxcapture_telemetry::History;

use crate::theme;

fn db_to_x(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// A stereo meter: RMS bar, peak tick, peak-hold line, red when clipped.
pub fn meter(ui: &mut Ui, level: Option<&LevelMeter>, width: f32) -> Response {
    let h = 12.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let Some(l) = level else { return resp };
    let row_h = (h - 1.0) / 2.0;
    for ch in 0..2 {
        let y0 = rect.top() + ch as f32 * (row_h + 1.0);
        let row = Rect::from_min_size(pos2(rect.left(), y0), vec2(rect.width(), row_h));
        let rms = db_to_x(l.rms_db.get(ch).copied().unwrap_or(-100.0));
        let peak = db_to_x(l.peak_db.get(ch).copied().unwrap_or(-100.0));
        let hold = db_to_x(l.peak_hold_db.get(ch).copied().unwrap_or(-100.0));
        // Green to -12 dB, yellow to -3 dB, red above.
        let segs = [(0.0, db_to_x(-12.0), theme::OK), (db_to_x(-12.0), db_to_x(-3.0), Color32::from_rgb(230, 200, 60)), (db_to_x(-3.0), 1.0, theme::REC)];
        for (a, b, c) in segs {
            let end = rms.min(b);
            if end > a {
                p.rect_filled(Rect::from_min_max(pos2(row.left() + a * row.width(), row.top()), pos2(row.left() + end * row.width(), row.bottom())), 0.0, c);
            }
        }
        let x = row.left() + peak * row.width();
        p.line_segment([pos2(x, row.top()), pos2(x, row.bottom())], Stroke::new(1.0, Color32::from_white_alpha(140)));
        let hx = row.left() + hold * row.width();
        p.line_segment([pos2(hx, row.top()), pos2(hx, row.bottom())], Stroke::new(2.0, if l.clipped { theme::REC } else { Color32::from_white_alpha(200) }));
    }
    resp
}

/// A small line graph of a history.
pub fn sparkline(ui: &mut Ui, h: &History, color: Color32, height: f32, max_hint: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let n = h.len();
    if n < 2 {
        return resp;
    }
    let max = h.max().max(max_hint).max(1e-3);
    let pts: Vec<Pos2> = h
        .iter()
        .enumerate()
        .map(|(i, v)| pos2(rect.left() + i as f32 / (n - 1) as f32 * rect.width(), rect.bottom() - (v / max).clamp(0.0, 1.0) * (rect.height() - 2.0) - 1.0))
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.5, color)));
    resp
}

/// A rounded status label.
pub fn pill(ui: &mut Ui, text: &str, fill: Color32) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), egui::FontId::proportional(13.0), Color32::WHITE);
    let size = galley.size() + vec2(16.0, 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    ui.painter().rect_filled(rect, size.y / 2.0, fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
    resp
}

/// The FluxCapture mark (drawn, no artwork): a teal ring around a red dot.
pub fn logo(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
    let c = rect.center();
    ui.painter().circle_stroke(c, 8.0, Stroke::new(2.0, theme::ACCENT));
    ui.painter().circle_filled(c, 4.0, theme::REC);
}

/// A filled circle (state dots).
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// "1.2 GB" style sizes.
pub fn bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

/// Minutes as "3 h 20 min" / "45 min".
pub fn minutes(m: f32) -> String {
    if !m.is_finite() || m < 0.0 {
        return "—".into();
    }
    let m = m.round() as u64;
    if m >= 60 { format!("{} h {:02} min", m / 60, m % 60) } else { format!("{m} min") }
}
