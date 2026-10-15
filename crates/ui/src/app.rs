//! The application window: layout, engine ticking, toasts, close handling.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText, TextureHandle};
use fluxcapture_engine::{Command, Engine, EngineOptions, Event, EventKind};
use fluxcapture_settings::Language;

use crate::{app_icon, diagnostics, mixer, preview, settings_window, sources, theme, tray, widgets, windows};

/// A toast notification.
pub struct Toast {
    pub event: Event,
    pub at: Instant,
}

/// What the settings window shows.
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    General,
    Video,
    Recording,
    Replay,
    Audio,
    Hotkeys,
    Advanced,
}

pub struct FluxApp {
    pub engine: Engine,
    pub preview_tex: Option<TextureHandle>,
    pub preview_seq: u64,
    pub selected_source: Option<u64>,
    pub show_settings: bool,
    pub settings_tab: SettingsTab,
    pub show_wizard: bool,
    pub show_logs: bool,
    pub show_about: bool,
    pub show_window_picker: bool,
    pub region_mode: bool,
    pub drag: Option<preview::Drag>,
    pub capturing_hotkey: Option<fluxcapture_settings::HotkeyAction>,
    pub toasts: Vec<Toast>,
    pub tray: Option<tray::Tray>,
    hidden: bool,
    window_setup_done: bool,
    styled_theme: Option<fluxcapture_settings::Theme>,
    last_window_refresh: Instant,
    pub confirm_quit: bool,
    quitting: bool,
}

impl FluxApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> FluxApp {
        let engine = Engine::new(EngineOptions::default());
        let start_hidden = engine.settings().general.start_minimized && engine.settings().general.minimize_to_tray;
        let show_wizard = engine.settings().general.first_run;
        let tray = tray::Tray::new(engine.settings().general.language == Language::En).map_err(|e| log::warn!("tray icon: {e}")).ok();
        theme::apply(&cc.egui_ctx, engine.settings().general.theme);
        let mut app = FluxApp {
            engine,
            preview_tex: None,
            preview_seq: 0,
            selected_source: None,
            show_settings: false,
            settings_tab: SettingsTab::default(),
            show_wizard,
            show_logs: false,
            show_about: false,
            show_window_picker: false,
            region_mode: false,
            drag: None,
            capturing_hotkey: None,
            toasts: Vec::new(),
            tray,
            hidden: false,
            window_setup_done: false,
            styled_theme: None,
            last_window_refresh: Instant::now(),
            confirm_quit: false,
            quitting: false,
        };
        if start_hidden {
            app.hidden = true;
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        app
    }

    pub fn tr(&self, pt: &'static str, en: &'static str) -> &'static str {
        if self.engine.settings().general.language == Language::En { en } else { pt }
    }

    pub fn en(&self) -> bool {
        self.engine.settings().general.language == Language::En
    }

    pub fn run(&mut self, cmd: Command) {
        let _ = self.engine.dispatch(cmd);
    }

    pub fn show_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn setup_native_window(&mut self, frame: &eframe::Frame) {
        if self.window_setup_done {
            return;
        }
        self.window_setup_done = true;
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(h) = frame.window_handle() else { return };
        let RawWindowHandle::Win32(w) = h.as_raw() else { return };
        let hwnd = w.hwnd.get() as u64;
        // winit only sets the small (title bar) icon; the taskbar and Alt+Tab use the big one.
        let (big, small) = fluxcapture_platform::system::window_icon_sizes();
        if let Err(e) = fluxcapture_platform::system::set_window_icon(hwnd, (&app_icon::dib(big), big), (&app_icon::dib(small), small)) {
            log::info!("cannot set the window icon: {e}");
        }
        if self.engine.settings().general.exclude_from_capture
            && let Err(e) = fluxcapture_platform::system::exclude_window_from_capture(hwnd, true)
        {
            log::info!("cannot exclude the window from capture: {e}");
        }
    }

    fn poll_events(&mut self) {
        for e in self.engine.take_events() {
            self.toasts.push(Toast { event: e, at: Instant::now() });
        }
        self.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(if t.event.kind == EventKind::Error { 12 } else { 6 }));
        if self.toasts.len() > 6 {
            let n = self.toasts.len() - 6;
            self.toasts.drain(..n);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            widgets::logo(ui);
            ui.label(RichText::new("FluxCapture").strong().size(17.0).color(theme::ACCENT));
            ui.separator();
            let busy = self.engine.is_encoding();
            ui.label(self.tr("Perfil", "Profile"));
            let profiles: Vec<String> = self.engine.settings().profiles.iter().map(|p| p.name.clone()).collect();
            let mut active = self.engine.settings().active_profile;
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("profile").selected_text(profiles.get(active).cloned().unwrap_or_default()).show_ui(ui, |ui| {
                    for (i, n) in profiles.iter().enumerate() {
                        ui.selectable_value(&mut active, i, n);
                    }
                });
            })
            .response
            .on_disabled_hover_text(self.tr("Pare a gravação/replay para trocar de perfil", "Stop recording / replay to switch profiles"));
            if active != self.engine.settings().active_profile {
                self.run(Command::SetProfile(active));
            }
            ui.label(self.tr("Cena", "Scene"));
            let scenes: Vec<String> = self.engine.settings().scenes.iter().map(|s| s.name.clone()).collect();
            let mut sc = self.engine.settings().active_scene;
            egui::ComboBox::from_id_salt("scene").selected_text(scenes.get(sc).cloned().unwrap_or_default()).show_ui(ui, |ui| {
                for (i, n) in scenes.iter().enumerate() {
                    ui.selectable_value(&mut sc, i, n);
                }
            });
            if sc != self.engine.settings().active_scene {
                self.run(Command::SetScene(sc));
                self.selected_source = None;
            }
            if ui.small_button("+").on_hover_text(self.tr("Nova cena", "New scene")).clicked() {
                self.engine.update_settings(|s| {
                    let mut sc = fluxcapture_settings::Scene::default_desktop();
                    sc.name = format!("Cena {}", s.scenes.len() + 1);
                    s.scenes.push(sc);
                    s.active_scene = s.scenes.len() - 1;
                });
            }
            ui.separator();
            let st = self.engine.state();
            if st.rec.active {
                let text = if st.rec.paused {
                    format!("⏸ {} {}", self.tr("PAUSADO", "PAUSED"), st.rec.duration.hms())
                } else {
                    format!("⏺ REC {}  {}", st.rec.duration.hms(), widgets::bytes(st.rec.bytes))
                };
                widgets::pill(ui, &text, if st.rec.paused { theme::WARN } else { theme::REC });
            } else if st.rec.finishing > 0 {
                widgets::pill(ui, self.tr("Finalizando arquivo…", "Finalizing file…"), theme::WARN);
            }
            if st.replay_enabled {
                widgets::pill(ui, &format!("⟲ Replay {:.0} s", st.replay_seconds), theme::ACCENT);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⚙").on_hover_text(self.tr("Configurações", "Settings")).clicked() {
                    self.show_settings = !self.show_settings;
                }
                if ui.button("🔧").on_hover_text(self.tr("Assistente de configuração", "Setup assistant")).clicked() {
                    self.show_wizard = true;
                }
                if ui.button("📜").on_hover_text(self.tr("Logs", "Logs")).clicked() {
                    self.show_logs = !self.show_logs;
                }
                let st = self.engine.state();
                let v = &st.video;
                ui.label(
                    RichText::new(format!(
                        "CPU {:.1}%  ·  render {:.2} ms  ·  GPU {}  ·  {}×{} @ {:.0}",
                        st.cpu_process,
                        v.render_ms_avg,
                        v.gpu_ms_avg.map_or("—".to_string(), |g| format!("{g:.2} ms")),
                        st.output.0,
                        st.output.1,
                        v.fps_target
                    ))
                    .color(theme::MUTED)
                    .small(),
                );
            });
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let st = self.engine.state().clone();
        ui.horizontal(|ui| {
            let rec_text = if st.rec.active { self.tr("⏹ Parar", "⏹ Stop") } else { self.tr("⏺ Gravar", "⏺ Record") };
            let rec_btn = egui::Button::new(RichText::new(rec_text).strong().color(Color32::WHITE).size(15.0))
                .fill(if st.rec.active { theme::REC.gamma_multiply(0.8) } else { theme::REC })
                .min_size(egui::vec2(110.0, 30.0));
            if ui.add(rec_btn).clicked() {
                self.run(Command::ToggleRecording);
            }
            let pause = if st.rec.paused { self.tr("▶ Retomar", "▶ Resume") } else { self.tr("⏸ Pausar", "⏸ Pause") };
            if ui.add_enabled(st.rec.active, egui::Button::new(pause).min_size(egui::vec2(90.0, 30.0))).clicked() {
                self.run(Command::TogglePause);
            }
            ui.separator();
            let replay = if st.replay_enabled { self.tr("⟲ Replay: ligado", "⟲ Replay: on") } else { self.tr("⟲ Replay: desligado", "⟲ Replay: off") };
            if ui.add(egui::Button::new(replay).selected(st.replay_enabled).min_size(egui::vec2(0.0, 30.0))).clicked() {
                self.run(Command::ToggleReplay);
            }
            if ui.add_enabled(st.replay_enabled, egui::Button::new(self.tr("💾 Salvar replay", "💾 Save replay")).min_size(egui::vec2(0.0, 30.0))).clicked()
            {
                self.run(Command::SaveReplay);
            }
            ui.separator();
            if ui.add(egui::Button::new("📷").min_size(egui::vec2(34.0, 30.0))).on_hover_text(self.tr("Captura de tela (PNG)", "Screenshot (PNG)")).clicked()
            {
                self.run(Command::Screenshot);
            }
            if ui
                .add(egui::Button::new("📁").min_size(egui::vec2(34.0, 30.0)))
                .on_hover_text(self.tr("Abrir pasta de gravações", "Open recordings folder"))
                .clicked()
            {
                self.run(Command::OpenRecordingsFolder);
            }
            let mic = if st.mic_muted { "🔇" } else { "🎤" };
            if ui
                .add(egui::Button::new(mic).selected(st.mic_muted).min_size(egui::vec2(34.0, 30.0)))
                .on_hover_text(self.tr("Microfone mudo / ativo", "Mute / unmute microphone"))
                .clicked()
            {
                self.run(Command::ToggleMicMute);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(m) = st.minutes_left {
                    let free = st.disk_free.map(widgets::bytes).unwrap_or_default();
                    let color = if m < 10.0 {
                        theme::REC
                    } else if m < 60.0 {
                        theme::WARN
                    } else {
                        theme::MUTED
                    };
                    ui.label(RichText::new(format!("💽 {free} · ~{}", widgets::minutes(m))).color(color).small()).on_hover_text(
                        self.tr("Espaço livre e tempo de gravação restante na taxa atual", "Free space and recording time left at the current rate"),
                    );
                }
                if let Some(e) = &st.encoder {
                    ui.label(RichText::new(format!("{} {}", if e.hardware { "⚡" } else { "🐢" }, e.name)).color(theme::MUTED).small());
                }
            });
        });
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let mut remove = None;
        let mut open: Option<std::path::PathBuf> = None;
        egui::Area::new(egui::Id::new("toasts")).anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -120.0)).order(egui::Order::Foreground).show(ctx, |ui| {
            ui.set_max_width(420.0);
            for (i, t) in self.toasts.iter().enumerate() {
                let color = match t.event.kind {
                    EventKind::Info => theme::ACCENT,
                    EventKind::Success => theme::OK,
                    EventKind::Warning => theme::WARN,
                    EventKind::Error => theme::REC,
                };
                egui::Frame::popup(ui.style()).stroke(egui::Stroke::new(1.5, color)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        widgets::dot(ui, color);
                        ui.add(egui::Label::new(&t.event.text).wrap());
                        if let Some(p) = &t.event.path
                            && ui.small_button(if self.en() { "Show" } else { "Mostrar" }).clicked()
                        {
                            open = Some(p.clone());
                        }
                        if ui.small_button("✕").clicked() {
                            remove = Some(i);
                        }
                    });
                });
                ui.add_space(4.0);
            }
        });
        if let Some(i) = remove {
            self.toasts.remove(i);
        }
        if let Some(p) = open {
            let _ = fluxcapture_platform::system::reveal_in_explorer(&p);
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.quitting {
            return;
        }
        let g = &self.engine.settings().general;
        if g.minimize_to_tray && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        } else if self.engine.is_recording() && g.confirm_stop {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
    }

    pub fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        self.engine.shutdown();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for FluxApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.styled_theme != Some(self.engine.settings().general.theme) {
            theme::apply(ctx, self.engine.settings().general.theme);
            self.styled_theme = Some(self.engine.settings().general.theme);
        }
        self.engine.tick();
        self.poll_events();
        let actions = self.tray.as_mut().map(|t| t.poll()).unwrap_or_default();
        {
            for action in actions {
                match action {
                    tray::Action::Show => self.show_window(ctx),
                    tray::Action::Command(c) => self.run(c),
                    tray::Action::Quit => self.quit(ctx),
                }
            }
        }
        let rec = self.engine.state().rec.active;
        if let Some(t) = self.tray.as_mut() {
            t.set_recording(rec);
        }
        if self.last_window_refresh.elapsed() > Duration::from_secs(10) {
            self.last_window_refresh = Instant::now();
            self.engine.refresh_windows();
        }
        self.handle_close(ctx);
        // Repaint at the preview rate while visible, 10 Hz otherwise (hotkeys, status).
        let p = &self.engine.settings().profile().preview;
        let visible_preview = !self.hidden && p.enabled && !(self.engine.is_recording() && p.removed_while_recording());
        let economy = self.engine.settings().general.economy_ui;
        let hz = if visible_preview {
            p.fps.clamp(1, 60)
        } else if economy {
            4
        } else {
            10
        };
        ctx.request_repaint_after(Duration::from_millis(1000 / u64::from(hz)));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.setup_native_window(frame);
        let ctx = ui.ctx().clone();
        egui::Panel::top("top").frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(10, 6))).show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("bottom").resizable(true).default_size(210.0).min_size(150.0).show(ui, |ui| {
            ui.add_space(4.0);
            self.controls(ui);
            ui.separator();
            mixer::show(self, ui);
        });
        egui::Panel::left("sources").resizable(true).default_size(290.0).min_size(220.0).show(ui, |ui| sources::show(self, ui));
        egui::Panel::right("diagnostics").resizable(true).default_size(300.0).min_size(240.0).show(ui, |ui| diagnostics::show(self, ui));
        egui::CentralPanel::default_margins().show(ui, |ui| preview::show(self, ui));
        if self.show_settings {
            settings_window::show(self, &ctx);
        }
        windows::show_all(self, &ctx);
        self.toasts_ui(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.engine.shutdown();
    }
}

/// The window icon for winit (drawn in code, see [`app_icon`]).
fn icon() -> egui::IconData {
    let n = 256;
    egui::IconData { rgba: app_icon::rgba(n), width: n, height: n }
}

/// Opens the main window (blocks until it closes).
pub fn run() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FluxCapture")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([980.0, 620.0])
            .with_icon(Arc::new(icon())),
        ..Default::default()
    };
    eframe::run_native("FluxCapture", options, Box::new(|cc| Ok(Box::new(FluxApp::new(cc))))).map_err(|e| e.to_string())
}
