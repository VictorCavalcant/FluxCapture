//! The audio mixer strip.

use egui::{RichText, Ui};
use fluxcapture_audio::{CompressorSettings, GateSettings, HighPassSettings, NoiseSuppressionSettings, SourceSettings};
use fluxcapture_platform::{AudioCaptureState, AudioEndpoint, AudioFlow};
use fluxcapture_settings::AudioSourceConfig;

use crate::app::FluxApp;
use crate::{theme, widgets};

fn endpoint_label(app: &FluxApp, e: &AudioEndpoint) -> String {
    match e {
        AudioEndpoint::DefaultOutput => app.tr("Saída padrão (som do sistema)", "Default output (system sound)").into(),
        AudioEndpoint::DefaultInput => app.tr("Microfone padrão", "Default microphone").into(),
        AudioEndpoint::Device { id, .. } => app
            .engine
            .devices()
            .audio
            .iter()
            .find(|d| &d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| app.tr("Dispositivo desconectado", "Disconnected device").into()),
        AudioEndpoint::Process { name, .. } => format!("{} {name}", app.tr("App:", "App:")),
    }
}

pub fn show(app: &mut FluxApp, ui: &mut Ui) {
    let sources = app.engine.settings().audio_sources.clone();
    let snap = app.engine.state().audio.clone();
    let mut changed: Option<AudioSourceConfig> = None;
    let mut remove: Option<u32> = None;
    egui::ScrollArea::horizontal().id_salt("mixer").show(ui, |ui| {
        ui.horizontal_top(|ui| {
            for src in &sources {
                let mut s = src.clone();
                egui::Frame::group(ui.style()).inner_margin(8).show(ui, |ui| {
                    ui.set_width(250.0);
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut s.enabled, "");
                            ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(140.0));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("🗑").on_hover_text(app.tr("Remover", "Remove")).clicked() {
                                    remove = Some(s.id);
                                }
                                let id = egui::Id::new(("filters", s.id));
                                let open = ui.small_button("⚙").on_hover_text(app.tr("Filtros e sincronia", "Filters and sync"));
                                egui::Popup::from_toggle_button_response(&open).id(id).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(
                                    |ui| {
                                        filters(app, ui, &mut s.settings);
                                    },
                                );
                            });
                        });
                        let state = snap.captures.iter().find(|c| c.0 == s.id).map(|c| c.2.clone());
                        let (color, label) = match &state {
                            Some(AudioCaptureState::Running { device }) => (theme::OK, device.clone()),
                            Some(AudioCaptureState::Reconnecting(m)) | Some(AudioCaptureState::Failed(m)) => (theme::REC, m.clone()),
                            _ if !s.enabled => (theme::MUTED, app.tr("desligado", "off").to_string()),
                            _ => (theme::MUTED, endpoint_label(app, &s.endpoint)),
                        };
                        ui.horizontal(|ui| {
                            widgets::dot(ui, color);
                            ui.label(RichText::new(label).small().color(theme::MUTED));
                        });
                        let meter = snap.meters.iter().find(|m| m.id.0 == s.id).map(|m| m.level.clone());
                        widgets::meter(ui, meter.as_ref(), 234.0)
                            .on_hover_text(app.tr("Pico / RMS (dBFS). Vermelho = clipping.", "Peak / RMS (dBFS). Red = clipping."));
                        ui.horizontal(|ui| {
                            let mute = if s.settings.muted { "🔇" } else { "🔊" };
                            if ui.add(egui::Button::new(mute).selected(s.settings.muted)).clicked() {
                                s.settings.muted = !s.settings.muted;
                            }
                            ui.add(egui::Slider::new(&mut s.settings.volume_db, -60.0..=20.0).suffix(" dB").show_value(true));
                        });
                        endpoint_combo(app, ui, &mut s);
                    });
                });
                if s != *src {
                    changed = Some(s);
                }
            }
            egui::Frame::group(ui.style()).inner_margin(8).show(ui, |ui| {
                ui.set_width(150.0);
                ui.vertical(|ui| {
                    ui.strong(app.tr("Master", "Master"));
                    for (i, m) in snap.master.iter().enumerate().take(6) {
                        ui.label(RichText::new(format!("{} {}", app.tr("Trilha", "Track"), i + 1)).small());
                        widgets::meter(ui, Some(m), 134.0);
                    }
                    if ui.button(app.tr("+ Fonte de áudio", "+ Audio source")).clicked() {
                        app.engine.update_settings(|s| {
                            let id = s.audio_sources.iter().map(|a| a.id).max().unwrap_or(0) + 1;
                            s.audio_sources.push(AudioSourceConfig {
                                id,
                                name: format!("Áudio {id}"),
                                endpoint: AudioEndpoint::DefaultInput,
                                enabled: true,
                                settings: SourceSettings { tracks: 1, continuous: true, ..SourceSettings::default() },
                            });
                        });
                    }
                });
            });
        });
    });
    if let Some(s) = changed {
        app.engine.update_settings(|set| {
            if let Some(x) = set.audio_sources.iter_mut().find(|x| x.id == s.id) {
                *x = s;
            }
        });
    }
    if let Some(id) = remove {
        app.engine.update_settings(|s| s.audio_sources.retain(|a| a.id != id));
    }
}

fn endpoint_combo(app: &mut FluxApp, ui: &mut Ui, s: &mut AudioSourceConfig) {
    let devices = app.engine.devices().audio.clone();
    let current = endpoint_label(app, &s.endpoint);
    let mut ep = s.endpoint.clone();
    egui::ComboBox::from_id_salt(("endpoint", s.id)).width(234.0).selected_text(current).show_ui(ui, |ui| {
        ui.selectable_value(&mut ep, AudioEndpoint::DefaultOutput, app.tr("Saída padrão (som do sistema)", "Default output (system sound)"));
        ui.selectable_value(&mut ep, AudioEndpoint::DefaultInput, app.tr("Microfone padrão", "Default microphone"));
        ui.separator();
        for d in &devices {
            let loopback = d.flow == AudioFlow::Playback;
            let label = format!("{} {}", if loopback { "🔊" } else { "🎤" }, d.name);
            ui.selectable_value(&mut ep, AudioEndpoint::Device { id: d.id.clone(), loopback }, label);
        }
        if fluxcapture_platform::audio::process_capture_supported() {
            ui.separator();
            ui.label(RichText::new(app.tr("Somente o som de um app:", "Only one app's sound:")).small());
            for p in fluxcapture_platform::audio::list_audio_processes().unwrap_or_default() {
                ui.selectable_value(&mut ep, AudioEndpoint::Process { pid: p.pid, name: p.name.clone() }, format!("▶ {}", p.name));
            }
        }
    });
    if ep != s.endpoint {
        // Microphones count missing data as dropouts; playback / app sound is silent when idle.
        s.settings.continuous = matches!(ep, AudioEndpoint::DefaultInput | AudioEndpoint::Device { loopback: false, .. });
        s.endpoint = ep;
    }
}

fn filters(app: &FluxApp, ui: &mut Ui, s: &mut SourceSettings) {
    ui.set_min_width(300.0);
    ui.strong(app.tr("Filtros (nesta ordem)", "Filters (in this order)"));
    egui::Grid::new("filters").num_columns(2).show(ui, |ui| {
        ui.label(app.tr("Ganho", "Gain"));
        ui.add(egui::Slider::new(&mut s.filters.gain_db, -20.0..=30.0).suffix(" dB"));
        ui.end_row();
        let mut hp = s.filters.high_pass.is_some();
        ui.checkbox(&mut hp, app.tr("Passa-altas", "High-pass"));
        if hp {
            let h = s.filters.high_pass.get_or_insert(HighPassSettings { cutoff_hz: 80.0 });
            ui.add(egui::Slider::new(&mut h.cutoff_hz, 20.0..=400.0).suffix(" Hz").logarithmic(true));
        } else {
            s.filters.high_pass = None;
            ui.label("");
        }
        ui.end_row();
        let mut ns = s.filters.noise_suppression.is_some();
        ui.checkbox(&mut ns, app.tr("Supressão de ruído", "Noise suppression"));
        if ns {
            let n = s.filters.noise_suppression.get_or_insert(NoiseSuppressionSettings { strength: 0.6 });
            ui.add(egui::Slider::new(&mut n.strength, 0.0..=1.0));
        } else {
            s.filters.noise_suppression = None;
            ui.label("");
        }
        ui.end_row();
        let mut gate = s.filters.gate.is_some();
        ui.checkbox(&mut gate, app.tr("Noise gate", "Noise gate"));
        if gate {
            let g = s.filters.gate.get_or_insert_with(GateSettings::default);
            ui.add(egui::Slider::new(&mut g.open_db, -80.0..=0.0).text(app.tr("abre", "open")).suffix(" dB"));
            g.close_db = g.close_db.min(g.open_db);
        } else {
            s.filters.gate = None;
            ui.label("");
        }
        ui.end_row();
        let mut comp = s.filters.compressor.is_some();
        ui.checkbox(&mut comp, app.tr("Compressor", "Compressor"));
        if comp {
            let c = s.filters.compressor.get_or_insert_with(CompressorSettings::default);
            ui.vertical(|ui| {
                ui.add(egui::Slider::new(&mut c.threshold_db, -60.0..=0.0).text(app.tr("limiar", "threshold")).suffix(" dB"));
                ui.add(egui::Slider::new(&mut c.ratio, 1.0..=20.0).text("ratio"));
                ui.add(egui::Slider::new(&mut c.makeup_db, 0.0..=24.0).text("makeup").suffix(" dB"));
            });
        } else {
            s.filters.compressor = None;
            ui.label("");
        }
        ui.end_row();
    });
    ui.separator();
    egui::Grid::new("sync").num_columns(2).show(ui, |ui| {
        ui.label(app.tr("Atraso (sincronia)", "Delay (sync)"));
        ui.add(egui::Slider::new(&mut s.sync_offset_ms, -2000..=2000).suffix(" ms"));
        ui.end_row();
        ui.label(app.tr("Balanço", "Balance"));
        ui.add(egui::Slider::new(&mut s.balance, -1.0..=1.0));
        ui.end_row();
        ui.label(app.tr("Mono", "Mono"));
        ui.checkbox(&mut s.force_mono, app.tr("misturar canais", "downmix channels"));
        ui.end_row();
    });
}
