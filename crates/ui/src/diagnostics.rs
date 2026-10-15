//! Performance and diagnostics panel: where the time goes, what is wrong, what to change.

use egui::{RichText, Ui};

use crate::app::FluxApp;
use crate::{theme, widgets};

pub fn show(app: &mut FluxApp, ui: &mut Ui) {
    ui.add_space(4.0);
    ui.heading(app.tr("Desempenho", "Performance"));
    let st = app.engine.state().clone();
    let v = &st.video;
    egui::ScrollArea::vertical().id_salt("diag").show(ui, |ui| {
        egui::Grid::new("stats").num_columns(2).striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
            let row = |ui: &mut Ui, k: &str, val: String, color: Option<egui::Color32>| {
                ui.label(RichText::new(k).color(theme::MUTED));
                match color {
                    Some(c) => ui.label(RichText::new(val).color(c)),
                    None => ui.label(val),
                };
                ui.end_row();
            };
            row(ui, "GPU", st.gpu_name.clone(), None);
            row(ui, app.tr("Saída", "Output"), format!("{}×{} @ {:.2}", st.output.0, st.output.1, v.fps_target), None);
            let frame_budget = 1000.0 / v.fps_target.max(1.0) as f32;
            row(
                ui,
                app.tr("Composição (CPU)", "Composition (CPU)"),
                format!("{:.2} ms (pior {:.1})", v.render_ms_avg, v.render_ms_max),
                (v.render_ms_max > frame_budget).then_some(theme::WARN),
            );
            row(
                ui,
                app.tr("Composição (GPU)", "Composition (GPU)"),
                v.gpu_ms_avg.map_or("—".into(), |g| format!("{g:.2} ms")),
                v.gpu_ms_avg.filter(|g| *g > frame_budget * 0.7).map(|_| theme::WARN),
            );
            row(ui, app.tr("Captura", "Capture"), v.capture_fps.iter().map(|(_, f)| format!("{f:.0}")).collect::<Vec<_>>().join(" / ") + " fps", None);
            if let Some(e) = &st.encoder {
                row(ui, "Encoder", format!("{}{}", e.name, if e.hardware { "" } else { " (software)" }), (!e.hardware).then_some(theme::WARN));
                row(
                    ui,
                    app.tr("Latência do encoder", "Encoder latency"),
                    format!("{:.1} ms · {} {}", v.encode_latency_ms_avg, v.encoder_in_flight, app.tr("na fila", "queued")),
                    None,
                );
                row(ui, app.tr("Taxa de bits", "Bitrate"), format!("{:.1} Mbit/s", v.bitrate_kbps / 1000.0), None);
            }
            let drops = v.dropped_render_lag + v.dropped_encoder_backlog;
            row(
                ui,
                app.tr("Quadros perdidos", "Dropped frames"),
                format!("{drops} ({} {} · {} {})", app.tr("render", "render"), v.dropped_render_lag, app.tr("encoder", "encoder"), v.dropped_encoder_backlog),
                (drops > 0).then_some(theme::REC),
            );
            if st.router.recording_dropped > 0 {
                row(ui, app.tr("Perdidos no disco", "Lost at the disk"), st.router.recording_dropped.to_string(), Some(theme::REC));
            }
            if v.skipped_static > 0 {
                row(ui, app.tr("Quadros estáticos (VFR)", "Static frames (VFR)"), v.skipped_static.to_string(), None);
            }
            row(ui, "CPU", format!("{:.1}% (sistema {:.0}%)", st.cpu_process, st.cpu_system), (st.cpu_system > 90.0).then_some(theme::WARN));
            row(ui, app.tr("Memória", "Memory"), format!("{} MB · {} MB {}", st.mem_process_mb, st.mem_available_mb, app.tr("livres", "free")), None);
            if st.replay_enabled {
                row(ui, "Replay", format!("{:.0} s · {}", st.replay_seconds, widgets::bytes(st.replay_bytes as u64)), None);
            }
            if v.preview_enabled {
                row(ui, app.tr("Prévia", "Preview"), format!("{:.0} fps", v.preview_fps), None);
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new(app.tr("CPU do FluxCapture (%)", "FluxCapture CPU (%)")).small().color(theme::MUTED));
        widgets::sparkline(ui, &st.history.cpu, theme::ACCENT, 34.0, 5.0);
        ui.label(RichText::new(app.tr("Composição GPU (ms)", "GPU composition (ms)")).small().color(theme::MUTED));
        widgets::sparkline(ui, &st.history.gpu_ms, theme::WARN, 34.0, 2.0);
        if st.encoding {
            ui.label(RichText::new(app.tr("Taxa de bits (kbit/s)", "Bitrate (kbit/s)")).small().color(theme::MUTED));
            widgets::sparkline(ui, &st.history.bitrate_kbps, theme::OK, 34.0, 1000.0);
        }
        ui.separator();
        ui.strong(app.tr("Diagnóstico", "Diagnostics"));
        if st.findings.is_empty() {
            ui.label(
                RichText::new(if st.rec.active {
                    app.tr("✔ Tudo certo: nenhum gargalo detectado.", "✔ All good: no bottleneck detected.")
                } else {
                    app.tr("Sem alertas.", "No alerts.")
                })
                .color(theme::OK),
            );
        }
        for f in &st.findings {
            egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, theme::severity(f.severity))).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&f.title).strong().color(theme::severity(f.severity)));
                ui.label(RichText::new(&f.detail).small());
                ui.label(RichText::new(&f.suggestion).small().italics());
            });
        }
        if !st.proposals.is_empty() {
            ui.add_space(4.0);
            ui.strong(app.tr("Ajustes sugeridos", "Suggested changes"));
            let mut apply = None;
            for (i, p) in st.proposals.iter().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(&p.title);
                    if ui.small_button(app.tr("Aplicar", "Apply")).clicked() {
                        apply = Some(i);
                    }
                });
            }
            if let Some(i) = apply {
                app.run(fluxcapture_engine::Command::ApplyProposal(i));
            }
            ui.label(
                RichText::new(app.tr(
                    "Nada muda sem sua confirmação; parâmetros travados no perfil nunca são sugeridos.",
                    "Nothing changes without your approval; parameters locked in the profile are never suggested.",
                ))
                .small()
                .color(theme::MUTED),
            );
        }
        if !st.hotkey_failures.is_empty() {
            ui.separator();
            for h in &st.hotkey_failures {
                ui.label(RichText::new(format!("⌨ {h}")).small().color(theme::WARN));
            }
        }
    });
}
