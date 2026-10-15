//! Secondary windows: setup assistant, recovery, logs, window picker, about, quit confirmation.

use egui::RichText;
use fluxcapture_platform::{CaptureOptions, Crop, Fit, RectF};
use fluxcapture_settings::{SceneSource, SourceKind};

use crate::app::FluxApp;
use crate::theme;

pub fn show_all(app: &mut FluxApp, ctx: &egui::Context) {
    if app.show_wizard {
        wizard(app, ctx);
    }
    if !app.engine.pending_recoveries().is_empty() {
        recovery(app, ctx);
    }
    if app.show_logs {
        logs(app, ctx);
    }
    if app.show_window_picker {
        window_picker(app, ctx);
    }
    if app.show_about {
        about(app, ctx);
    }
    if app.confirm_quit {
        confirm_quit(app, ctx);
    }
}

fn wizard(app: &mut FluxApp, ctx: &egui::Context) {
    let mut open = true;
    let title = app.tr("Assistente de configuração", "Setup assistant");
    let mut start = false;
    let mut apply = false;
    egui::Window::new(title).open(&mut open).collapsible(false).default_width(560.0).show(ctx, |ui| {
        match app.engine.wizard() {
            None => {
                ui.label(app.tr(
                    "O assistente mede este computador — velocidade do disco e um teste real de gravação com cada encoder de hardware — e recomenda resolução, FPS, codec e taxa de bits. Nada é alterado sem sua confirmação.",
                    "The assistant measures this computer — disk speed and a real recording test with each hardware encoder — and recommends resolution, frame rate, codec and bitrate. Nothing changes until you confirm.",
                ));
                ui.add_space(6.0);
                ui.label(RichText::new(app.tr("Leva cerca de 15 segundos. A tela principal será capturada durante o teste (nada é salvo).", "Takes about 15 seconds. The main screen is captured during the test (nothing is saved).")).small().color(theme::MUTED));
                if ui.button(app.tr("▶ Iniciar teste", "▶ Start test")).clicked() {
                    start = true;
                }
            }
            Some(w) => {
                for p in w.progress() {
                    ui.label(format!("• {p}"));
                }
                if w.running() {
                    ui.spinner();
                }
                if let Some(r) = w.report() {
                    ui.separator();
                    ui.label(format!("GPU: {}", r.gpu));
                    if let Some(d) = r.disk_write_mbps {
                        ui.label(format!("{}: {:.0} MB/s", app.tr("Disco", "Disk"), d / 8.0));
                    }
                    egui::Grid::new("tests").striped(true).show(ui, |ui| {
                        for t in &r.tests {
                            ui.label(if t.ok() { RichText::new("✔").color(theme::OK) } else { RichText::new("✖").color(theme::REC) });
                            ui.label(format!("{} {}×{}", t.codec.label(), t.width, t.height));
                            ui.label(&t.name);
                            ui.label(format!("{} {} · enc {:.1} ms", t.dropped, app.tr("perdidos", "dropped"), t.latency_ms));
                            ui.end_row();
                        }
                    });
                    if let Some(rec) = &r.recommendation {
                        ui.separator();
                        ui.label(RichText::new(app.tr("Recomendado", "Recommended")).strong());
                        ui.label(format!("{} · {} · {} @ {} fps · {} kbit/s · ~{:.1} GB/h", rec.codec.label(), rec.encoder_name, rec.resolution.label(), rec.fps.label(), rec.bitrate_kbps, rec.gb_per_hour));
                        if ui.button(app.tr("Aplicar ao perfil atual", "Apply to the current profile")).clicked() {
                            apply = true;
                        }
                    }
                    for n in &r.notes {
                        ui.label(RichText::new(format!("ⓘ {n}")).small());
                    }
                }
            }
        }
    });
    if start && let Err(e) = app.engine.start_wizard() {
        app.toasts.push(crate::app::Toast {
            event: fluxcapture_engine::Event { kind: fluxcapture_engine::EventKind::Error, text: e, path: None },
            at: std::time::Instant::now(),
        });
    }
    if apply {
        app.engine.apply_wizard();
        app.show_wizard = false;
        app.engine.close_wizard();
    }
    if !open {
        app.show_wizard = false;
        app.engine.close_wizard();
        app.engine.update_settings(|s| s.general.first_run = false);
    }
}

fn recovery(app: &mut FluxApp, ctx: &egui::Context) {
    let list: Vec<_> = app.engine.pending_recoveries().iter().map(|(_, e)| e.clone()).collect();
    let mut action: Option<(usize, bool)> = None;
    egui::Window::new(app.tr("Gravação interrompida", "Interrupted recording")).collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.label(app.tr(
            "O FluxCapture foi fechado no meio de uma gravação. O arquivo continua reproduzível; a recuperação cria uma cópia finalizada (índice e duração) sem tocar no original.",
            "FluxCapture closed in the middle of a recording. The file is still playable; recovery writes a finalized copy (index and duration) without touching the original.",
        ));
        for (i, e) in list.iter().enumerate() {
            ui.separator();
            for f in &e.files {
                ui.label(RichText::new(f.display().to_string()).small());
            }
            ui.horizontal(|ui| {
                if ui.button(app.tr("Recuperar", "Recover")).clicked() {
                    action = Some((i, true));
                }
                if ui.button(app.tr("Ignorar", "Ignore")).clicked() {
                    action = Some((i, false));
                }
            });
        }
    });
    match action {
        Some((i, true)) => app.engine.recover(i),
        Some((i, false)) => app.engine.dismiss_recovery(i),
        None => {}
    }
}

fn logs(app: &mut FluxApp, ctx: &egui::Context) {
    let mut open = true;
    let lines = app.engine.log().lines();
    egui::Window::new("Logs").open(&mut open).default_size([760.0, 420.0]).show(ctx, |ui| {
        ui.horizontal(|ui| {
            if ui.button(app.tr("Exportar relatório…", "Export report…")).clicked()
                && let Some(path) = rfd::FileDialog::new().set_file_name("fluxcapture-relatorio.txt").save_file()
            {
                let st = app.engine.state();
                let header = format!(
                    "FluxCapture {}\n{}\nGPU: {}\nEncoder: {:?}\nOutput: {}x{}\nCPU: {:.1}% / system {:.0}%\nFindings: {:?}",
                    env!("CARGO_PKG_VERSION"),
                    fluxcapture_platform::system::os_description(),
                    st.gpu_name,
                    st.encoder.as_ref().map(|e| &e.name),
                    st.output.0,
                    st.output.1,
                    st.cpu_process,
                    st.cpu_system,
                    st.findings.iter().map(|f| &f.title).collect::<Vec<_>>()
                );
                if let Err(e) = app.engine.log().export(&path, &header) {
                    log::error!("export: {e}");
                }
            }
            ui.label(
                RichText::new(app.tr("Segredos (chaves, tokens) são removidos automaticamente.", "Secrets (keys, tokens) are removed automatically."))
                    .small()
                    .color(theme::MUTED),
            );
        });
        ui.separator();
        egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
            for l in &lines {
                let color = match l.level {
                    log::Level::Error => theme::REC,
                    log::Level::Warn => theme::WARN,
                    _ => ui.visuals().text_color(),
                };
                ui.label(RichText::new(&l.text).monospace().size(11.5).color(color));
            }
        });
    });
    if !open {
        app.show_logs = false;
    }
}

fn window_picker(app: &mut FluxApp, ctx: &egui::Context) {
    let mut open = true;
    let windows = app.engine.devices().windows.clone();
    let mut chosen = None;
    egui::Window::new(app.tr("Escolher janela", "Choose a window")).open(&mut open).default_size([520.0, 420.0]).show(ctx, |ui| {
        if ui.button(app.tr("⟳ Atualizar", "⟳ Refresh")).clicked() {
            app.engine.refresh_windows();
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for w in &windows {
                let label = format!("{}  —  {}  ({}×{}){}", w.title, w.process, w.width, w.height, if w.minimized { " ▾" } else { "" });
                if ui.selectable_label(false, label).clicked() {
                    chosen = Some(w.clone());
                }
            }
        });
    });
    if let Some(w) = chosen {
        let selected = app.selected_source;
        app.engine.update_settings(|s| {
            let sc = s.scene_mut();
            let kind = SourceKind::Window { handle: w.handle, title: w.title.clone(), process: w.process.clone(), options: CaptureOptions::default() };
            // Replace the selected window source, or add a new one.
            if let Some(src) = selected.and_then(|id| sc.sources.iter_mut().find(|x| x.id == id && matches!(x.kind, SourceKind::Window { .. }))) {
                src.kind = kind;
                src.name = w.title.chars().take(40).collect();
            } else {
                let id = sc.next_id();
                sc.sources.push(SceneSource {
                    id,
                    name: w.title.chars().take(40).collect(),
                    kind,
                    rect: RectF { x: 0.0, y: 0.0, w: 1.0, h: 1.0 },
                    fit: Fit::Contain,
                    crop: Crop::default(),
                    opacity: 1.0,
                    visible: true,
                });
            }
        });
        if selected.is_none() {
            app.selected_source = app.engine.settings().scene().and_then(|s| s.sources.last()).map(|s| s.id);
        }
        app.show_window_picker = false;
    }
    if !open {
        app.show_window_picker = false;
    }
}

fn about(app: &mut FluxApp, ctx: &egui::Context) {
    let mut open = true;
    egui::Window::new("FluxCapture").open(&mut open).collapsible(false).show(ctx, |ui| {
        ui.heading(format!("FluxCapture {}", env!("CARGO_PKG_VERSION")));
        ui.label(app.tr(
            "Gravador de tela em Rust, com captura, composição e codificação inteiramente na GPU.",
            "Screen recorder in Rust, with capture, composition and encoding entirely on the GPU.",
        ));
        ui.label("MIT OR Apache-2.0");
    });
    if !open {
        app.show_about = false;
    }
}

fn confirm_quit(app: &mut FluxApp, ctx: &egui::Context) {
    let mut quit = false;
    let mut cancel = false;
    egui::Window::new(app.tr("Sair durante a gravação?", "Quit while recording?")).collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(
        ctx,
        |ui| {
            ui.label(app.tr("A gravação será finalizada e salva antes de sair.", "The recording will be finalized and saved before quitting."));
            ui.horizontal(|ui| {
                if ui.button(app.tr("Finalizar e sair", "Finish and quit")).clicked() {
                    quit = true;
                }
                if ui.button(app.tr("Cancelar", "Cancel")).clicked() {
                    cancel = true;
                }
            });
        },
    );
    if quit {
        app.confirm_quit = false;
        app.quit(ctx);
    }
    if cancel {
        app.confirm_quit = false;
    }
}
