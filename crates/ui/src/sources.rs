//! Scene sources: list, add, reorder, properties.

use egui::{RichText, Ui};
use fluxcapture_platform::{CaptureMethod, CaptureOptions, Crop, Fit, RectF, SourceState};
use fluxcapture_settings::{SceneSource, SourceKind, TextStyle};

use crate::app::FluxApp;
use crate::{preview, theme, widgets};

fn add(app: &mut FluxApp, name: String, kind: SourceKind, rect: RectF, fit: Fit, bottom: bool) -> u64 {
    let mut id = 0;
    app.engine.update_settings(|s| {
        let sc = s.scene_mut();
        id = sc.next_id();
        let src = SceneSource { id, name, kind, rect, fit, crop: Crop::default(), opacity: 1.0, visible: true };
        if bottom { sc.sources.insert(0, src) } else { sc.sources.push(src) }
    });
    app.selected_source = Some(id);
    id
}

const FULL: RectF = RectF { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };

pub fn show(app: &mut FluxApp, ui: &mut Ui) {
    ui.add_space(4.0);
    ui.heading(app.tr("Fontes", "Sources"));
    ui.horizontal_wrapped(|ui| {
        if ui.button(app.tr("🖥 Tela", "🖥 Display")).clicked() {
            let monitor = app.engine.devices().monitors.iter().find(|m| m.primary).map(|m| m.id.clone()).unwrap_or_default();
            let name = app.tr("Tela", "Display").to_string();
            add(app, name, SourceKind::Display { monitor, options: CaptureOptions::default() }, FULL, Fit::Contain, false);
        }
        if ui.button(app.tr("🗖 Janela", "🗖 Window")).clicked() {
            app.engine.refresh_windows();
            app.show_window_picker = true;
        }
        if ui.button(app.tr("✂ Região", "✂ Region")).clicked() {
            let monitor = app.engine.devices().monitors.iter().find(|m| m.primary).map(|m| m.id.clone()).unwrap_or_default();
            let name = app.tr("Região", "Region").to_string();
            let id = add(app, name, SourceKind::Display { monitor, options: CaptureOptions::default() }, FULL, Fit::Contain, false);
            preview::begin_region(app, id);
        }
        if ui.button(app.tr("🖼 Imagem", "🖼 Image")).clicked()
            && let Some(path) = rfd::FileDialog::new().add_filter("Imagens", &["png", "jpg", "jpeg", "bmp", "gif", "webp"]).pick_file()
        {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            add(app, name, SourceKind::Image { path }, RectF { x: 0.72, y: 0.04, w: 0.24, h: 0.24 }, Fit::Contain, false);
        }
        if ui.button(app.tr("🔤 Texto", "🔤 Text")).clicked() {
            add(
                app,
                app.tr("Texto", "Text").to_string(),
                SourceKind::Text(TextStyle::default()),
                RectF { x: 0.03, y: 0.85, w: 0.35, h: 0.1 },
                Fit::Contain,
                false,
            );
        }
        if ui.button("📷 Webcam").clicked() {
            let device = app.engine.devices().webcams.first().map(|w| w.0.clone()).unwrap_or_default();
            add(
                app,
                "Webcam".into(),
                SourceKind::Webcam { device, width: 1280, height: 720, fps: 30 },
                RectF { x: 0.74, y: 0.70, w: 0.24, h: 0.27 },
                Fit::Cover,
                false,
            );
        }
        if ui.button(app.tr("🎨 Cor", "🎨 Color")).clicked() {
            add(app, app.tr("Fundo", "Background").to_string(), SourceKind::Color { rgba: [0.08, 0.08, 0.1, 1.0] }, FULL, Fit::Stretch, true);
        }
    });
    ui.separator();
    let Some(scene) = app.engine.settings().scene().cloned() else { return };
    let states = app.engine.state().sources.clone();
    let errors = app.engine.state().source_errors.clone();
    let mut action: Option<(u64, &'static str)> = None;
    egui::ScrollArea::vertical().id_salt("source-list").max_height(ui.available_height() * 0.42).show(ui, |ui| {
        if scene.sources.is_empty() {
            ui.label(RichText::new(app.tr("Cena vazia: adicione uma fonte acima.", "Empty scene: add a source above.")).color(theme::MUTED));
        }
        // Top of the list = top layer.
        for s in scene.sources.iter().rev() {
            ui.horizontal(|ui| {
                let state = states.iter().find(|(id, _)| *id == s.id).map(|(_, st)| st.clone());
                let (color, tip) = match (&state, errors.iter().find(|(id, _)| *id == s.id)) {
                    (_, Some((_, e))) => (theme::REC, e.clone()),
                    (Some(SourceState::Active { width, height }), _) => (theme::OK, format!("{width}×{height}")),
                    (Some(SourceState::Starting), _) | (None, _) => (theme::MUTED, String::new()),
                    (Some(SourceState::Paused), _) => (theme::WARN, String::new()),
                    (Some(SourceState::Lost(m)), _) | (Some(SourceState::Failed(m)), _) => (theme::REC, m.clone()),
                };
                widgets::dot(ui, color);
                if ui.small_button(if s.visible { "👁" } else { "—" }).on_hover_text(app.tr("Mostrar / ocultar", "Show / hide")).clicked() {
                    action = Some((s.id, "toggle"));
                }
                let r = ui.selectable_label(app.selected_source == Some(s.id), &s.name);
                let r = if tip.is_empty() { r } else { r.on_hover_text(tip) };
                if r.clicked() {
                    app.selected_source = Some(s.id);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("🗑").clicked() {
                        action = Some((s.id, "delete"));
                    }
                    if ui.small_button("⏷").clicked() {
                        action = Some((s.id, "down"));
                    }
                    if ui.small_button("⏶").clicked() {
                        action = Some((s.id, "up"));
                    }
                });
            });
        }
    });
    if let Some((id, a)) = action {
        app.engine.update_settings(|s| {
            let v = &mut s.scene_mut().sources;
            let Some(i) = v.iter().position(|x| x.id == id) else { return };
            match a {
                "toggle" => {
                    if let Some(x) = v.get_mut(i) {
                        x.visible = !x.visible;
                    }
                }
                "delete" => {
                    v.remove(i);
                }
                "up" if i + 1 < v.len() => v.swap(i, i + 1),
                "down" if i > 0 => v.swap(i, i - 1),
                _ => {}
            }
        });
        if a == "delete" && app.selected_source == Some(id) {
            app.selected_source = None;
        }
    }
    ui.separator();
    if let Some(id) = app.selected_source
        && let Some(src) = scene.sources.iter().find(|s| s.id == id).cloned()
    {
        egui::ScrollArea::vertical().id_salt("props").show(ui, |ui| properties(app, ui, src));
    } else {
        ui.label(RichText::new(app.tr("Selecione uma fonte para editar.", "Select a source to edit it.")).color(theme::MUTED));
    }
}

fn properties(app: &mut FluxApp, ui: &mut Ui, mut src: SceneSource) {
    let before = src.clone();
    ui.strong(app.tr("Propriedades", "Properties"));
    egui::Grid::new("props-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
        ui.label(app.tr("Nome", "Name"));
        ui.text_edit_singleline(&mut src.name);
        ui.end_row();
        match &mut src.kind {
            SourceKind::Display { monitor, options } => {
                ui.label(app.tr("Monitor", "Monitor"));
                let monitors = app.engine.devices().monitors.clone();
                let current = monitors
                    .iter()
                    .find(|m| &m.id == monitor || (monitor.is_empty() && m.primary))
                    .map(|m| format!("{} ({}×{})", m.name, m.width, m.height))
                    .unwrap_or_default();
                egui::ComboBox::from_id_salt("monitor").selected_text(current).show_ui(ui, |ui| {
                    for m in &monitors {
                        ui.selectable_value(
                            monitor,
                            m.id.clone(),
                            format!("{} ({}×{} @ {:.0} Hz){}", m.name, m.width, m.height, m.refresh_hz, if m.hdr { " HDR" } else { "" }),
                        );
                    }
                });
                ui.end_row();
                capture_options(app, ui, options, true);
            }
            SourceKind::Window { title, process, options, handle } => {
                ui.label(app.tr("Janela", "Window"));
                ui.label(RichText::new(format!("{title} ({process})")).small());
                ui.end_row();
                ui.label("");
                if ui.button(app.tr("Trocar janela…", "Change window…")).clicked() {
                    app.engine.refresh_windows();
                    app.show_window_picker = true;
                }
                ui.end_row();
                let _ = handle;
                capture_options(app, ui, options, false);
            }
            SourceKind::Image { path } => {
                ui.label(app.tr("Arquivo", "File"));
                if ui.button(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "…".into())).clicked()
                    && let Some(p) = rfd::FileDialog::new().add_filter("Imagens", &["png", "jpg", "jpeg", "bmp", "gif", "webp"]).pick_file()
                {
                    *path = p;
                }
                ui.end_row();
            }
            SourceKind::Text(t) => {
                ui.label(app.tr("Texto", "Text"));
                ui.add(egui::TextEdit::multiline(&mut t.text).desired_rows(2));
                ui.end_row();
                ui.label(app.tr("Fonte", "Font"));
                egui::ComboBox::from_id_salt("font").selected_text(t.font.clone()).show_ui(ui, |ui| {
                    for f in [
                        "Segoe UI",
                        "Arial",
                        "Calibri",
                        "Consolas",
                        "Verdana",
                        "Tahoma",
                        "Georgia",
                        "Trebuchet MS",
                        "Impact",
                        "Times New Roman",
                        "Cascadia Code",
                    ] {
                        ui.selectable_value(&mut t.font, f.to_string(), f);
                    }
                });
                ui.end_row();
                ui.label(app.tr("Tamanho", "Size"));
                ui.add(egui::Slider::new(&mut t.size_px, 8.0..=256.0).suffix(" px"));
                ui.end_row();
                ui.label(app.tr("Cor", "Color"));
                let mut c = egui::Color32::from_rgba_unmultiplied(t.color[0], t.color[1], t.color[2], t.color[3]);
                if ui.color_edit_button_srgba(&mut c).changed() {
                    t.color = c.to_srgba_unmultiplied();
                }
                ui.end_row();
                ui.label(app.tr("Negrito", "Bold"));
                ui.checkbox(&mut t.bold, "");
                ui.end_row();
                ui.label(app.tr("Contorno", "Outline"));
                let mut on = t.outline.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    t.outline = on.then_some([0, 0, 0, 200]);
                }
                ui.end_row();
                ui.label(app.tr("Fundo", "Background"));
                let mut bg = t.background.is_some();
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut bg, "").changed() {
                        t.background = bg.then_some([0, 0, 0, 140]);
                    }
                    if let Some(b) = t.background.as_mut() {
                        let mut c = egui::Color32::from_rgba_unmultiplied(b[0], b[1], b[2], b[3]);
                        if ui.color_edit_button_srgba(&mut c).changed() {
                            *b = c.to_srgba_unmultiplied();
                        }
                    }
                });
                ui.end_row();
            }
            SourceKind::Color { rgba } => {
                ui.label(app.tr("Cor", "Color"));
                let mut c = egui::Rgba::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
                if egui::widgets::color_picker::color_edit_button_rgba(ui, &mut c, egui::widgets::color_picker::Alpha::OnlyBlend).changed() {
                    *rgba = c.to_rgba_unmultiplied();
                }
                ui.end_row();
            }
            SourceKind::Webcam { device, width, height, fps } => {
                ui.label(app.tr("Dispositivo", "Device"));
                let cams = app.engine.devices().webcams.clone();
                let current =
                    cams.iter().find(|c| &c.0 == device).map(|c| c.1.clone()).unwrap_or_else(|| app.tr("Primeira disponível", "First available").into());
                egui::ComboBox::from_id_salt("cam").selected_text(current).show_ui(ui, |ui| {
                    for (id, name) in &cams {
                        ui.selectable_value(device, id.clone(), name);
                    }
                });
                ui.end_row();
                ui.label(app.tr("Modo", "Mode"));
                let mut mode = (*width, *height);
                egui::ComboBox::from_id_salt("cam-mode").selected_text(format!("{}×{}", width, height)).show_ui(ui, |ui| {
                    for m in [(640, 480), (1280, 720), (1920, 1080)] {
                        ui.selectable_value(&mut mode, m, format!("{}×{}", m.0, m.1));
                    }
                });
                (*width, *height) = mode;
                ui.end_row();
                ui.label("FPS");
                ui.add(egui::Slider::new(fps, 5..=60));
                ui.end_row();
            }
        }
        ui.label(app.tr("Ajuste", "Fit"));
        egui::ComboBox::from_id_salt("fit").selected_text(fit_name(app, src.fit)).show_ui(ui, |ui| {
            for f in [Fit::Contain, Fit::Cover, Fit::Stretch, Fit::Original] {
                ui.selectable_value(&mut src.fit, f, fit_name(app, f));
            }
        });
        ui.end_row();
        ui.label(app.tr("Opacidade", "Opacity"));
        ui.add(egui::Slider::new(&mut src.opacity, 0.0..=1.0));
        ui.end_row();
        ui.label(app.tr("Posição (%)", "Position (%)"));
        ui.horizontal(|ui| {
            pct(ui, &mut src.rect.x);
            pct(ui, &mut src.rect.y);
        });
        ui.end_row();
        ui.label(app.tr("Tamanho (%)", "Size (%)"));
        ui.horizontal(|ui| {
            pct(ui, &mut src.rect.w);
            pct(ui, &mut src.rect.h);
        });
        ui.end_row();
        ui.label(app.tr("Recorte (px)", "Crop (px)"));
        ui.horizontal(|ui| {
            for v in [&mut src.crop.left, &mut src.crop.top, &mut src.crop.right, &mut src.crop.bottom] {
                ui.add(egui::DragValue::new(v).range(0..=16384).speed(2));
            }
        });
        ui.end_row();
    });
    ui.horizontal(|ui| {
        if ui.button(app.tr("Tela cheia", "Fill canvas")).clicked() {
            src.rect = FULL;
        }
        if matches!(src.kind, SourceKind::Display { .. }) && ui.button(app.tr("✂ Selecionar região…", "✂ Select region…")).clicked() {
            preview::begin_region(app, src.id);
            return;
        }
        if src.crop != Crop::default() && ui.button(app.tr("Sem recorte", "No crop")).clicked() {
            src.crop = Crop::default();
        }
    });
    if src != before {
        app.engine.update_settings(|s| {
            if let Some(x) = s.scene_mut().sources.iter_mut().find(|x| x.id == src.id) {
                *x = src;
            }
        });
    }
}

fn pct(ui: &mut Ui, v: &mut f32) {
    let mut p = *v * 100.0;
    if ui.add(egui::DragValue::new(&mut p).range(-100.0..=200.0).speed(0.2).suffix("%").max_decimals(1)).changed() {
        *v = p / 100.0;
    }
}

fn fit_name(app: &FluxApp, f: Fit) -> &'static str {
    match f {
        Fit::Contain => app.tr("Conter (sem cortes)", "Contain"),
        Fit::Cover => app.tr("Preencher (corta sobras)", "Cover"),
        Fit::Stretch => app.tr("Esticar", "Stretch"),
        Fit::Original => app.tr("Tamanho original", "Original size"),
    }
}

fn capture_options(app: &FluxApp, ui: &mut Ui, o: &mut CaptureOptions, monitor: bool) {
    ui.label(app.tr("Cursor", "Cursor"));
    ui.checkbox(&mut o.cursor, app.tr("capturar o cursor", "capture the cursor"));
    ui.end_row();
    ui.label(app.tr("Borda amarela", "Yellow border"));
    ui.checkbox(&mut o.border, app.tr("mostrar (Windows 11 permite ocultar)", "show (Windows 11 can hide it)"));
    ui.end_row();
    ui.label(app.tr("FPS máx. da origem", "Source max fps"));
    ui.add(egui::DragValue::new(&mut o.max_fps).range(0..=480).suffix(" fps"))
        .on_hover_text(app.tr("0 = sem limite. Limitar reduz cópias na GPU.", "0 = unlimited. Limiting reduces GPU copies."));
    ui.end_row();
    if monitor {
        ui.label(app.tr("Método", "Method"));
        egui::ComboBox::from_id_salt("method")
            .selected_text(match o.method {
                CaptureMethod::Auto => app.tr("Automático", "Automatic"),
                CaptureMethod::Wgc => "Windows.Graphics.Capture",
                CaptureMethod::Duplication => "DXGI Desktop Duplication",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut o.method, CaptureMethod::Auto, app.tr("Automático", "Automatic"));
                ui.selectable_value(&mut o.method, CaptureMethod::Wgc, "Windows.Graphics.Capture");
                ui.selectable_value(&mut o.method, CaptureMethod::Duplication, "DXGI Desktop Duplication");
            });
        ui.end_row();
    }
}
