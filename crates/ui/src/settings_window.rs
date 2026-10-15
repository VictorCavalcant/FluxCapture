//! The settings window.

use egui::{RichText, Ui};
use fluxcapture_platform::{CaptureMethod, Hotkey, ScaleFilter, VideoCodecKind};
use fluxcapture_settings::template::expand;
use fluxcapture_settings::{
    AudioCodec, HotkeyAction, Language, OutputResolution, Priority, Profile, ProfileKind, RateMode, Settings, Split, Theme, TrackLayout,
};
use fluxcapture_time::FrameRate;

use crate::app::{FluxApp, SettingsTab};
use crate::theme;

pub fn show(app: &mut FluxApp, ctx: &egui::Context) {
    let mut open = true;
    let title = app.tr("Configurações", "Settings");
    let mut s = app.engine.settings().clone();
    let before = s.clone();
    egui::Window::new(title).open(&mut open).default_size([640.0, 560.0]).resizable(true).collapsible(false).show(ctx, |ui| {
        ui.horizontal(|ui| {
            for (tab, pt, en) in [
                (SettingsTab::General, "Geral", "General"),
                (SettingsTab::Video, "Vídeo", "Video"),
                (SettingsTab::Recording, "Gravação", "Recording"),
                (SettingsTab::Replay, "Replay", "Replay"),
                (SettingsTab::Audio, "Áudio", "Audio"),
                (SettingsTab::Hotkeys, "Atalhos", "Hotkeys"),
                (SettingsTab::Advanced, "Avançado", "Advanced"),
            ] {
                let label = app.tr(pt, en);
                ui.selectable_value(&mut app.settings_tab, tab, label);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| match app.settings_tab {
            SettingsTab::General => general(app, ui, &mut s),
            SettingsTab::Video => video(app, ui, &mut s),
            SettingsTab::Recording => recording(app, ui, &mut s),
            SettingsTab::Replay => replay(app, ui, &mut s),
            SettingsTab::Audio => audio(app, ui, &mut s),
            SettingsTab::Hotkeys => hotkeys(app, ui, &mut s),
            SettingsTab::Advanced => advanced(app, ui, &mut s),
        });
    });
    if s != before {
        app.engine.update_settings(|cur| *cur = s);
    }
    if !open {
        app.show_settings = false;
        app.capturing_hotkey = None;
    }
}

fn general(app: &FluxApp, ui: &mut Ui, s: &mut Settings) {
    egui::Grid::new("g").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(app.tr("Idioma", "Language"));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut s.general.language, Language::PtBr, "Português (Brasil)");
            ui.selectable_value(&mut s.general.language, Language::En, "English");
        });
        ui.end_row();
        ui.label(app.tr("Tema", "Theme"));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut s.general.theme, Theme::Dark, app.tr("Escuro", "Dark"));
            ui.selectable_value(&mut s.general.theme, Theme::Light, app.tr("Claro", "Light"));
            ui.selectable_value(&mut s.general.theme, Theme::System, app.tr("Sistema", "System"));
        });
        ui.end_row();
        ui.label(app.tr("Bandeja", "Tray"));
        ui.vertical(|ui| {
            ui.checkbox(
                &mut s.general.minimize_to_tray,
                app.tr("fechar a janela mantém o FluxCapture na bandeja", "closing the window keeps FluxCapture in the tray"),
            );
            ui.add_enabled(
                s.general.minimize_to_tray,
                egui::Checkbox::new(&mut s.general.start_minimized, app.tr("iniciar minimizado na bandeja", "start minimized to the tray")),
            );
        });
        ui.end_row();
        ui.label(app.tr("Privacidade", "Privacy"));
        ui.checkbox(
            &mut s.general.exclude_from_capture,
            app.tr(
                "ocultar as janelas do FluxCapture de gravações, prints e compartilhamento de tela",
                "hide FluxCapture's windows from recordings, screenshots and screen sharing",
            ),
        );
        ui.end_row();
        ui.label(app.tr("Parar", "Stop"));
        ui.checkbox(&mut s.general.confirm_stop, app.tr("confirmar antes de sair durante uma gravação", "confirm before quitting while recording"));
        ui.end_row();
        ui.label(app.tr("Prioridade", "Priority"));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut s.general.process_priority, Priority::Normal, app.tr("Normal", "Normal"));
            ui.selectable_value(&mut s.general.process_priority, Priority::AboveNormal, app.tr("Acima do normal", "Above normal"));
            ui.selectable_value(&mut s.general.process_priority, Priority::High, app.tr("Alta", "High"));
        });
        ui.end_row();
        ui.label(app.tr("Modo econômico", "Economy mode"));
        ui.checkbox(
            &mut s.general.economy_ui,
            app.tr("menos atualizações da interface quando a prévia está parada", "fewer UI refreshes when the preview is idle"),
        );
        ui.end_row();
    });
}

fn video(app: &FluxApp, ui: &mut Ui, s: &mut Settings) {
    let busy = app.engine.is_encoding();
    if busy {
        ui.label(
            RichText::new(app.tr("Gravando: mudanças de vídeo valem para a próxima gravação.", "Recording: video changes apply to the next recording."))
                .color(theme::WARN),
        );
    }
    ui.horizontal(|ui| {
        ui.label(app.tr("Perfil", "Profile"));
        let names: Vec<String> = s.profiles.iter().map(|p| p.name.clone()).collect();
        let mut i = s.active_profile;
        egui::ComboBox::from_id_salt("prof").selected_text(names.get(i).cloned().unwrap_or_default()).show_ui(ui, |ui| {
            for (k, n) in names.iter().enumerate() {
                ui.selectable_value(&mut i, k, n);
            }
        });
        if !busy {
            s.active_profile = i;
        }
        if ui.button(app.tr("Duplicar", "Duplicate")).clicked() {
            let mut p = s.profile().clone();
            p.name = format!("{} (2)", p.name);
            p.kind = ProfileKind::Custom;
            s.profiles.push(p);
            s.active_profile = s.profiles.len() - 1;
        }
        if s.profiles.len() > 1 && ui.button(app.tr("Excluir", "Delete")).clicked() && !busy {
            s.profiles.remove(s.active_profile);
            s.active_profile = 0;
        }
        if ui.button(app.tr("Exportar…", "Export…")).clicked()
            && let Some(path) = rfd::FileDialog::new().add_filter("FluxCapture", &["toml"]).set_file_name(format!("{}.toml", s.profile().name)).save_file()
            && let Ok(t) = fluxcapture_settings::export_profile(s.profile())
        {
            let _ = std::fs::write(path, t);
        }
        if ui.button(app.tr("Importar…", "Import…")).clicked()
            && let Some(path) = rfd::FileDialog::new().add_filter("FluxCapture", &["toml"]).pick_file()
            && let Ok(t) = std::fs::read_to_string(path)
            && let Ok(p) = fluxcapture_settings::import_profile(&t)
        {
            s.profiles.push(p);
            s.active_profile = s.profiles.len() - 1;
        }
    });
    let native = app.engine.state().native;
    let encoders = app.engine.devices().encoders.clone();
    let p: &mut Profile = s.profile_mut();
    ui.horizontal(|ui| {
        ui.label(app.tr("Nome", "Name"));
        ui.text_edit_singleline(&mut p.name);
        if ui.small_button(app.tr("Restaurar padrão", "Reset to preset")).clicked() {
            let name = p.name.clone();
            *p = Profile::preset(p.kind);
            p.name = name;
        }
    });
    ui.separator();
    egui::Grid::new("v").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(app.tr("Resolução de saída", "Output resolution"));
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("res")
                .selected_text(if p.video.resolution == OutputResolution::Native {
                    format!("{} ({}×{})", app.tr("Nativa", "Native"), native.0, native.1)
                } else {
                    p.video.resolution.label()
                })
                .show_ui(ui, |ui| {
                    for r in [OutputResolution::Native, OutputResolution::P720, OutputResolution::P1080, OutputResolution::P1440, OutputResolution::P2160] {
                        let label =
                            if r == OutputResolution::Native { format!("{} ({}×{})", app.tr("Nativa", "Native"), native.0, native.1) } else { r.label() };
                        ui.selectable_value(&mut p.video.resolution, r, label);
                    }
                    if ui.selectable_label(matches!(p.video.resolution, OutputResolution::Custom { .. }), app.tr("Personalizada", "Custom")).clicked() {
                        p.video.resolution = OutputResolution::Custom { width: native.0, height: native.1 };
                    }
                });
            if let OutputResolution::Custom { width, height } = &mut p.video.resolution {
                ui.add(egui::DragValue::new(width).range(16..=8192));
                ui.label("×");
                ui.add(egui::DragValue::new(height).range(16..=8192));
            }
            ui.checkbox(&mut p.locks.resolution, "🔒")
                .on_hover_text(app.tr("Travar: o assistente de desempenho nunca sugere mudar", "Lock: the performance advisor never suggests changing it"));
        });
        ui.end_row();
        ui.label("FPS");
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("fps").selected_text(p.video.fps.label()).show_ui(ui, |ui| {
                for f in [
                    FrameRate::FPS_24,
                    FrameRate::FPS_30,
                    FrameRate::FPS_48,
                    FrameRate::FPS_60,
                    FrameRate::FPS_120,
                    FrameRate::FPS_144,
                    FrameRate::NTSC_30,
                    FrameRate::NTSC_60,
                ] {
                    ui.selectable_value(&mut p.video.fps, f, f.label());
                }
            });
            ui.checkbox(&mut p.video.variable_frame_rate, app.tr("variável (não codifica quadros repetidos)", "variable (skip repeated frames)"));
            ui.checkbox(&mut p.locks.fps, "🔒");
        });
        ui.end_row();
        ui.label("Codec");
        ui.horizontal(|ui| {
            for c in [VideoCodecKind::H264, VideoCodecKind::Hevc] {
                ui.selectable_value(&mut p.video.codec, c, c.label());
            }
        });
        ui.end_row();
        ui.label("Encoder");
        ui.horizontal(|ui| {
            let codec = p.video.codec;
            let current = p
                .video
                .encoder
                .as_ref()
                .and_then(|id| encoders.iter().find(|e| &e.id == id))
                .map(|e| e.name.clone())
                .unwrap_or_else(|| app.tr("Automático (hardware)", "Automatic (hardware)").into());
            egui::ComboBox::from_id_salt("enc").selected_text(current).show_ui(ui, |ui| {
                ui.selectable_value(&mut p.video.encoder, None, app.tr("Automático (hardware)", "Automatic (hardware)"));
                let mut seen = Vec::new();
                for e in encoders.iter().filter(|e| e.codec == codec) {
                    if seen.contains(&e.id) {
                        continue;
                    }
                    seen.push(e.id.clone());
                    ui.selectable_value(&mut p.video.encoder, Some(e.id.clone()), format!("{} {}", if e.hardware { "⚡" } else { "🐢" }, e.name));
                }
            });
            ui.checkbox(&mut p.locks.encoder, "🔒");
        });
        ui.end_row();
        ui.label(app.tr("Controle de taxa", "Rate control"));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut p.video.rate_mode, RateMode::Quality, app.tr("Qualidade constante", "Constant quality"));
            ui.selectable_value(&mut p.video.rate_mode, RateMode::Vbr, "VBR");
            ui.selectable_value(&mut p.video.rate_mode, RateMode::Cbr, "CBR");
            ui.checkbox(&mut p.locks.quality, "🔒");
        });
        ui.end_row();
        let (w, h) = p.video.resolution.resolve(native);
        let rec = fluxcapture_settings::recommended_kbps(w, h, p.video.fps.as_f64(), p.video.codec);
        match p.video.rate_mode {
            RateMode::Quality => {
                ui.label(app.tr("Qualidade", "Quality"));
                ui.add(egui::Slider::new(&mut p.video.quality, 1..=100));
                ui.end_row();
            }
            RateMode::Vbr | RateMode::Cbr => {
                ui.label(app.tr("Taxa de bits", "Bitrate"));
                ui.horizontal(|ui| {
                    let mut auto = p.video.bitrate_kbps == 0;
                    if ui.checkbox(&mut auto, format!("{} ({} kbit/s)", app.tr("recomendada", "recommended"), rec)).changed() {
                        p.video.bitrate_kbps = if auto { 0 } else { rec };
                    }
                    if !auto {
                        ui.add(egui::DragValue::new(&mut p.video.bitrate_kbps).range(500..=300_000).speed(100).suffix(" kbit/s"));
                    }
                });
                ui.end_row();
            }
        }
        ui.label(app.tr("Preset (velocidade ↔ compressão)", "Preset (speed ↔ compression)"));
        ui.add(egui::Slider::new(&mut p.video.speed_quality, 0..=100));
        ui.end_row();
        ui.label(app.tr("Intervalo de keyframe", "Keyframe interval"));
        ui.add(egui::Slider::new(&mut p.video.keyframe_interval_s, 0.5..=10.0).suffix(" s"));
        ui.end_row();
        ui.label(app.tr("Baixa latência", "Low latency"));
        ui.checkbox(
            &mut p.video.low_latency,
            app.tr("menos memória e GPU no encoder (arquivos um pouco maiores)", "less encoder memory and GPU (slightly larger files)"),
        );
        ui.end_row();
        ui.label(app.tr("Filtro de escala", "Scaling filter"));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut p.video.scale_filter, ScaleFilter::Bilinear, app.tr("Bilinear (leve)", "Bilinear (light)"));
            ui.selectable_value(&mut p.video.scale_filter, ScaleFilter::Bicubic, app.tr("Bicúbico", "Bicubic"));
            ui.selectable_value(&mut p.video.scale_filter, ScaleFilter::Lanczos, app.tr("Lanczos (nítido)", "Lanczos (sharp)"));
        });
        ui.end_row();
        ui.label(app.tr("Faixa de cor", "Colour range"));
        ui.checkbox(
            &mut p.video.full_range,
            app.tr(
                "completa (0-255) — padrão: limitada (TV), compatível com todos os players",
                "full (0-255) — default: limited (TV), what every player expects",
            ),
        );
        ui.end_row();
        let audio_kbps = 3 * p.audio.bitrate_kbps;
        let total = p.video.rate_control(w, h).estimated_kbps(w, h, p.video.fps.as_f64()) + audio_kbps;
        ui.label(app.tr("Estimativa", "Estimate"));
        ui.label(RichText::new(format!("~{:.1} GB/h", total as f64 * 1000.0 / 8.0 * 3600.0 / 1e9)).color(theme::MUTED));
        ui.end_row();
    });
    ui.separator();
    ui.strong(app.tr("Prévia", "Preview"));
    egui::Grid::new("pv").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(app.tr("Ativa", "Enabled"));
        ui.checkbox(&mut p.preview.enabled, "");
        ui.end_row();
        ui.label(app.tr("Durante a gravação", "While recording"));
        ui.checkbox(&mut p.preview.pause_while_recording, app.tr("pausar (economiza GPU/CPU)", "pause (saves GPU/CPU)"));
        ui.end_row();
        ui.label(app.tr("Alta qualidade", "High quality"));
        ui.checkbox(
            &mut p.preview.high_quality,
            app.tr(
                "resolução quase nativa e filtro Lanczos (usa mais GPU; removida durante a gravação)",
                "near-native resolution and Lanczos filter (uses more GPU; removed while recording)",
            ),
        );
        ui.end_row();
        ui.label(app.tr("FPS da prévia", "Preview fps"));
        ui.add(egui::Slider::new(&mut p.preview.fps, 1..=60));
        ui.end_row();
        ui.label(app.tr("Tamanho máximo", "Max size"));
        ui.add(egui::Slider::new(&mut p.preview.max_size, 320..=1920).suffix(" px"));
        ui.end_row();
    });
}

fn recording(app: &FluxApp, ui: &mut Ui, s: &mut Settings) {
    egui::Grid::new("r").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(app.tr("Pasta", "Folder"));
        ui.horizontal(|ui| {
            ui.label(RichText::new(s.recording_folder().display().to_string()).small());
            if ui.button(app.tr("Escolher…", "Choose…")).clicked()
                && let Some(d) = rfd::FileDialog::new().pick_folder()
            {
                s.recording.folder = d;
            }
        });
        ui.end_row();
        ui.label(app.tr("Nome do arquivo", "File name"));
        ui.vertical(|ui| {
            ui.text_edit_singleline(&mut s.recording.filename_template);
            let (y, mo, d, h, mi, se) = fluxcapture_platform::system::local_time();
            let ex = expand(
                &s.recording.filename_template,
                fluxcapture_settings::template::Stamp { year: y, month: mo, day: d, hour: h, minute: mi, second: se },
                &s.profile().name,
                "Cena 1",
                1,
            );
            ui.label(RichText::new(format!("{ex}.mkv · {{date}} {{time}} {{profile}} {{scene}} {{counter}}")).small().color(theme::MUTED));
        });
        ui.end_row();
        ui.label(app.tr("Formato", "Format"));
        ui.vertical(|ui| {
            ui.label(
                RichText::new(app.tr(
                    "Grava em MKV (seguro contra quedas); o arquivo continua reproduzível mesmo se o PC desligar.",
                    "Records MKV (crash-safe): the file stays playable even if the PC shuts down.",
                ))
                .small(),
            );
            ui.checkbox(&mut s.recording.remux_to_mp4, app.tr("converter para MP4 ao terminar (sem recodificar)", "convert to MP4 when done (no re-encoding)"));
            ui.add_enabled(
                s.recording.remux_to_mp4,
                egui::Checkbox::new(
                    &mut s.recording.delete_mkv_after_remux,
                    app.tr("apagar o MKV depois que o MP4 estiver completo", "delete the MKV once the MP4 is complete"),
                ),
            );
        });
        ui.end_row();
        ui.label(app.tr("Dividir arquivo", "Split file"));
        ui.horizontal(|ui| {
            let mut mode = match s.recording.split {
                Split::Never => 0,
                Split::Minutes(_) => 1,
                Split::Megabytes(_) => 2,
            };
            ui.selectable_value(&mut mode, 0, app.tr("Nunca", "Never"));
            ui.selectable_value(&mut mode, 1, app.tr("Por tempo", "By time"));
            ui.selectable_value(&mut mode, 2, app.tr("Por tamanho", "By size"));
            s.recording.split = match (mode, s.recording.split) {
                (1, Split::Minutes(m)) => Split::Minutes(m),
                (1, _) => Split::Minutes(30),
                (2, Split::Megabytes(m)) => Split::Megabytes(m),
                (2, _) => Split::Megabytes(4000),
                _ => Split::Never,
            };
            match &mut s.recording.split {
                Split::Minutes(m) => {
                    ui.add(egui::DragValue::new(m).range(1..=1440).suffix(" min"));
                }
                Split::Megabytes(m) => {
                    ui.add(egui::DragValue::new(m).range(100..=1_000_000).speed(50).suffix(" MB"));
                }
                Split::Never => {}
            }
        });
        ui.end_row();
        ui.label(app.tr("Trilhas de áudio", "Audio tracks"));
        ui.vertical(|ui| {
            ui.radio_value(
                &mut s.recording.tracks,
                TrackLayout::MixAndSeparate,
                app.tr("mixagem + uma trilha por fonte (editar separado depois)", "mix + one track per source (edit separately later)"),
            );
            ui.radio_value(&mut s.recording.tracks, TrackLayout::MixOnly, app.tr("somente a mixagem", "mix only"));
        });
        ui.end_row();
        ui.label(app.tr("Espaço em disco", "Disk space"));
        ui.vertical(|ui| {
            ui.add(
                egui::DragValue::new(&mut s.recording.warn_free_mb)
                    .range(100..=1_000_000)
                    .speed(100)
                    .prefix(app.tr("avisar abaixo de ", "warn below "))
                    .suffix(" MB"),
            );
            ui.add(
                egui::DragValue::new(&mut s.recording.min_free_mb)
                    .range(100..=100_000)
                    .speed(50)
                    .prefix(app.tr("finalizar abaixo de ", "finish below "))
                    .suffix(" MB"),
            );
        });
        ui.end_row();
    });
}

fn replay(app: &FluxApp, ui: &mut Ui, s: &mut Settings) {
    ui.label(
        RichText::new(app.tr(
            "O replay guarda os últimos segundos já codificados na memória: salvar é instantâneo e não recodifica.",
            "The replay keeps the last seconds, already encoded, in memory: saving is instant and never re-encodes.",
        ))
        .small(),
    );
    egui::Grid::new("rp").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(app.tr("Duração", "Duration"));
        ui.horizontal(|ui| {
            for v in [15, 30, 60, 120] {
                ui.selectable_value(&mut s.replay.seconds, v, format!("{v} s"));
            }
            ui.add(egui::DragValue::new(&mut s.replay.seconds).range(5..=600).suffix(" s"));
        });
        ui.end_row();
        ui.label(app.tr("Limite de memória", "Memory limit"));
        ui.add(egui::Slider::new(&mut s.replay.max_memory_mb, 64..=4096).suffix(" MB"));
        ui.end_row();
        ui.label(app.tr("Formato", "Format"));
        ui.checkbox(&mut s.replay.save_mp4, app.tr("salvar como MP4", "save as MP4"));
        ui.end_row();
        ui.label(app.tr("Ao iniciar", "On start"));
        ui.checkbox(&mut s.replay.enabled_on_start, app.tr("ligar o replay junto com o FluxCapture", "turn the replay on with FluxCapture"));
        ui.end_row();
    });
}

fn audio(app: &FluxApp, ui: &mut Ui, s: &mut Settings) {
    egui::Grid::new("a").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        let p = s.profile_mut();
        ui.label(app.tr("Codec", "Codec"));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut p.audio.codec, AudioCodec::Aac, "AAC");
            ui.selectable_value(&mut p.audio.codec, AudioCodec::Pcm, app.tr("PCM (sem perdas, só MKV)", "PCM (lossless, MKV only)"));
        });
        ui.end_row();
        if p.audio.codec == AudioCodec::Aac {
            ui.label(app.tr("Taxa de bits", "Bitrate"));
            ui.horizontal(|ui| {
                for b in [96, 128, 160, 192] {
                    ui.selectable_value(&mut p.audio.bitrate_kbps, b, format!("{b} kbit/s"));
                }
            });
            ui.end_row();
        }
        ui.label(app.tr("Volume master", "Master volume"));
        ui.add(egui::Slider::new(&mut s.mixer.master_volume_db, -30.0..=12.0).suffix(" dB"));
        ui.end_row();
        ui.label(app.tr("Limitador", "Limiter"));
        ui.horizontal(|ui| {
            let mut on = s.mixer.limiter.is_some();
            ui.checkbox(&mut on, app.tr("evita clipping", "prevents clipping"));
            if on {
                let l = s.mixer.limiter.get_or_insert_with(Default::default);
                ui.add(egui::Slider::new(&mut l.ceiling_db, -12.0..=0.0).suffix(" dB"));
            } else {
                s.mixer.limiter = None;
            }
        });
        ui.end_row();
        ui.label(app.tr("Atraso do mixer", "Mixer delay"));
        ui.add(egui::Slider::new(&mut s.advanced.audio_mix_delay_ms, 80..=500).suffix(" ms")).on_hover_text(app.tr(
            "Mais atraso tolera dispositivos irregulares (sem afetar a sincronia do arquivo).",
            "More delay tolerates irregular devices (file sync is unaffected).",
        ));
        ui.end_row();
    });
}

fn vk_from_key(k: egui::Key) -> Option<u32> {
    use egui::Key::*;
    let name = k.name();
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
        && (1..=24).contains(&n)
    {
        return Some(0x6F + n);
    }
    if name.len() == 1
        && let Some(c) = name.chars().next()
        && (c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Some(c as u32);
    }
    Some(match k {
        Space => 0x20,
        Insert => 0x2D,
        Home => 0x24,
        End => 0x23,
        PageUp => 0x21,
        PageDown => 0x22,
        _ => return None,
    })
}

fn hotkeys(app: &mut FluxApp, ui: &mut Ui, s: &mut Settings) {
    ui.label(
        RichText::new(app.tr(
            "Atalhos globais funcionam mesmo com o FluxCapture minimizado ou atrás de um jogo.",
            "Global hotkeys work even when FluxCapture is minimized or behind a game.",
        ))
        .small(),
    );
    // Capture the next key press for the action being edited.
    if let Some(action) = app.capturing_hotkey {
        let pressed = ui.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                _ => None,
            })
        });
        if let Some((key, m)) = pressed {
            if key == egui::Key::Escape {
                app.capturing_hotkey = None;
            } else if let Some(vk) = vk_from_key(key) {
                let hk = Hotkey { ctrl: m.ctrl, alt: m.alt, shift: m.shift, win: false, vk };
                let slot = match action {
                    HotkeyAction::Record => &mut s.hotkeys.record,
                    HotkeyAction::Pause => &mut s.hotkeys.pause,
                    HotkeyAction::SaveReplay => &mut s.hotkeys.save_replay,
                    HotkeyAction::Screenshot => &mut s.hotkeys.screenshot,
                    HotkeyAction::ToggleMic => &mut s.hotkeys.toggle_mic,
                    HotkeyAction::ToggleReplay => &mut s.hotkeys.toggle_replay,
                };
                *slot = Some(hk);
                app.capturing_hotkey = None;
            }
        }
    }
    egui::Grid::new("hk").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
        for (action, key) in s.hotkeys.list() {
            let label = match action {
                HotkeyAction::Record => app.tr("Gravar / parar", "Record / stop"),
                HotkeyAction::Pause => app.tr("Pausar / retomar", "Pause / resume"),
                HotkeyAction::ToggleReplay => app.tr("Ativar / desativar replay", "Turn replay on / off"),
                HotkeyAction::SaveReplay => app.tr("Salvar replay", "Save replay"),
                HotkeyAction::Screenshot => app.tr("Captura de tela", "Screenshot"),
                HotkeyAction::ToggleMic => app.tr("Microfone mudo", "Mute microphone"),
            };
            ui.label(label);
            if app.capturing_hotkey == Some(action) {
                ui.label(RichText::new(app.tr("pressione a combinação… (Esc cancela)", "press the combination… (Esc cancels)")).color(theme::ACCENT));
            } else {
                ui.monospace(key.map(|k| k.label()).unwrap_or_else(|| "—".into()));
            }
            ui.horizontal(|ui| {
                if ui.small_button(app.tr("Alterar", "Change")).clicked() {
                    app.capturing_hotkey = Some(action);
                }
                if ui.small_button(app.tr("Limpar", "Clear")).clicked() {
                    match action {
                        HotkeyAction::Record => s.hotkeys.record = None,
                        HotkeyAction::Pause => s.hotkeys.pause = None,
                        HotkeyAction::SaveReplay => s.hotkeys.save_replay = None,
                        HotkeyAction::Screenshot => s.hotkeys.screenshot = None,
                        HotkeyAction::ToggleMic => s.hotkeys.toggle_mic = None,
                        HotkeyAction::ToggleReplay => s.hotkeys.toggle_replay = None,
                    }
                }
            });
            ui.end_row();
        }
    });
}

fn advanced(app: &FluxApp, ui: &mut Ui, s: &mut Settings) {
    let gpus = app.engine.devices().gpus.clone();
    egui::Grid::new("adv").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("GPU");
        let current = if s.advanced.gpu_name.is_empty() { app.tr("Automática", "Automatic").to_string() } else { s.advanced.gpu_name.clone() };
        egui::ComboBox::from_id_salt("gpu").selected_text(current).show_ui(ui, |ui| {
            if ui
                .selectable_label(
                    s.advanced.gpu_name.is_empty(),
                    app.tr("Automática (a que desenha o monitor principal)", "Automatic (the one driving the main monitor)"),
                )
                .clicked()
            {
                s.advanced.gpu_name.clear();
                s.advanced.gpu_luid = None;
            }
            for g in gpus.iter().filter(|g| !g.software) {
                if ui.selectable_label(s.advanced.gpu_name == g.name, format!("{} ({} MB)", g.name, g.dedicated_video_memory / 1_048_576)).clicked() {
                    s.advanced.gpu_name = g.name.clone();
                    s.advanced.gpu_luid = Some(g.luid);
                }
            }
        });
        ui.end_row();
        ui.label(app.tr("Método de captura de tela", "Display capture method"));
        egui::ComboBox::from_id_salt("cm")
            .selected_text(match s.advanced.capture_method {
                CaptureMethod::Auto => app.tr("Por fonte", "Per source"),
                CaptureMethod::Wgc => "Windows.Graphics.Capture",
                CaptureMethod::Duplication => "DXGI Desktop Duplication",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut s.advanced.capture_method, CaptureMethod::Auto, app.tr("Por fonte", "Per source"));
                ui.selectable_value(&mut s.advanced.capture_method, CaptureMethod::Wgc, "Windows.Graphics.Capture");
                ui.selectable_value(&mut s.advanced.capture_method, CaptureMethod::Duplication, "DXGI Desktop Duplication");
            });
        ui.end_row();
        ui.label(app.tr("Log detalhado", "Verbose log"));
        ui.checkbox(&mut s.advanced.verbose_log, app.tr("(aplica ao reiniciar)", "(applies after restart)"));
        ui.end_row();
        ui.label(app.tr("Canal de controle", "Control channel"));
        ui.horizontal(|ui| {
            let mut on = s.advanced.control_port != 0;
            if ui.checkbox(&mut on, app.tr("JSON-lines em 127.0.0.1", "JSON lines on 127.0.0.1")).changed() {
                s.advanced.control_port = if on { 9876 } else { 0 };
            }
            if on {
                ui.add(egui::DragValue::new(&mut s.advanced.control_port).range(1024..=65535));
            }
        });
        ui.end_row();
        ui.label(app.tr("Configuração", "Configuration"));
        ui.horizontal(|ui| {
            if ui.button(app.tr("Abrir pasta", "Open folder")).clicked() {
                let _ = fluxcapture_platform::system::reveal_in_explorer(app.engine.config_dir());
            }
            if ui.button(app.tr("Restaurar tudo", "Reset everything")).clicked() {
                let keep = s.general.language;
                *s = Settings::default();
                s.general.language = keep;
                s.general.first_run = false;
            }
        });
        ui.end_row();
    });
}
