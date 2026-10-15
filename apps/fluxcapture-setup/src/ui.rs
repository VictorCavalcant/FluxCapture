//! The setup window: one question, one button, then "done". Written for people who have never
//! installed a program by hand: plain words, safe defaults, no administrator prompt.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use egui::{Color32, RichText};
use fluxcapture_platform::install::{MessageKind, message_box, ui_language_is_portuguese};

use crate::Mode;
use crate::app_icon;
use crate::install::{self, Failure, Plan};

const ACCENT: Color32 = Color32::from_rgb(0, 168, 150);
const ERROR: Color32 = Color32::from_rgb(230, 57, 70);
const VERSION: &str = env!("CARGO_PKG_VERSION");

enum Page {
    Ask,
    Working,
    Done,
    Failed(Failure),
}

struct SetupApp {
    exe: PathBuf,
    mode: Mode,
    pt: bool,
    page: Page,
    dir: PathBuf,
    desktop_shortcut: bool,
    launch: bool,
    installed: Option<String>,
    remove_settings: bool,
    /// Progress of the worker, in thousandths.
    progress: Arc<AtomicU32>,
    worker: Option<JoinHandle<Result<bool, Failure>>>,
    /// The running uninstaller must be deleted once this process exits (read after the window
    /// closes).
    delete_self: Arc<AtomicBool>,
    window_setup_done: bool,
}

fn t(pt: bool, pt_text: &'static str, en_text: &'static str) -> &'static str {
    if pt { pt_text } else { en_text }
}

/// A message box for errors before (or instead of) the window.
pub fn fatal(text: &str) -> ExitCode {
    let pt = ui_language_is_portuguese();
    message_box(t(pt, "Instalação do FluxCapture", "FluxCapture Setup"), text, MessageKind::Error);
    ExitCode::FAILURE
}

/// Opens the setup window (blocks until it closes).
pub fn run(exe: PathBuf, mode: Mode) -> ExitCode {
    let pt = ui_language_is_portuguese();
    let title = match mode {
        Mode::Uninstall(_) => t(pt, "Desinstalar o FluxCapture", "Uninstall FluxCapture"),
        _ => t(pt, "Instalação do FluxCapture", "FluxCapture Setup"),
    };
    let n = 64;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([580.0, 400.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_icon(Arc::new(egui::IconData { rgba: app_icon::rgba(n), width: n, height: n })),
        centered: true,
        ..Default::default()
    };
    let installed = install::installed_version();
    let dir = match &mode {
        Mode::Uninstall(dir) => dir.clone(),
        _ => install::default_dir(),
    };
    let delete_self = Arc::new(AtomicBool::new(false));
    let app = SetupApp {
        exe: exe.clone(),
        mode: mode.clone(),
        pt,
        page: Page::Ask,
        dir,
        desktop_shortcut: true,
        launch: true,
        installed,
        remove_settings: false,
        progress: Arc::new(AtomicU32::new(0)),
        worker: None,
        delete_self: delete_self.clone(),
        window_setup_done: false,
    };
    let result = eframe::run_native(
        "FluxCapture Setup",
        options,
        Box::new(move |cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    if delete_self.load(Ordering::SeqCst) {
        install::delete_after_exit(&exe);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fallback(&exe, mode, pt, &e.to_string()),
    }
}

/// No window could be opened (e.g. no OpenGL driver): ask with plain message boxes instead.
fn fallback(exe: &std::path::Path, mode: Mode, pt: bool, why: &str) -> ExitCode {
    log::info!("setup window unavailable: {why}");
    let title = t(pt, "Instalação do FluxCapture", "FluxCapture Setup");
    let result = match mode {
        Mode::Install => {
            let plan = Plan { dir: install::default_dir(), desktop_shortcut: true, register: true };
            let question = format!("{}\n\n{}", t(pt, "Instalar o FluxCapture neste computador?", "Install FluxCapture on this computer?"), plan.dir.display());
            if !message_box(title, &question, MessageKind::Question) {
                return ExitCode::SUCCESS;
            }
            install::install(exe, &plan, &mut |_| {})
                .map(|()| t(pt, "O FluxCapture foi instalado. Abra-o pelo Menu Iniciar.", "FluxCapture is installed. Open it from the Start menu."))
        }
        Mode::Uninstall(dir) => {
            let question = t(
                pt,
                "Remover o FluxCapture deste computador? Suas gravações não serão apagadas.",
                "Remove FluxCapture from this computer? Your recordings are kept.",
            );
            if !message_box(title, question, MessageKind::Question) {
                return ExitCode::SUCCESS;
            }
            install::uninstall(&dir, exe, false, true).map(|pending| {
                if pending {
                    install::delete_after_exit(exe);
                }
                t(pt, "O FluxCapture foi removido.", "FluxCapture was removed.")
            })
        }
        Mode::Empty => Err(Failure::Damaged(String::new())),
    };
    match result {
        Ok(done) => {
            message_box(title, done, MessageKind::Info);
            ExitCode::SUCCESS
        }
        Err(f) => fatal(&failure_text(pt, &f)),
    }
}

fn style(ctx: &egui::Context) {
    let dark = ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark);
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.selection.bg_fill = ACCENT.linear_multiply(if dark { 0.6 } else { 0.4 });
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    if dark {
        v.panel_fill = Color32::from_rgb(24, 25, 30);
    }
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(14.0, 6.0);
        s.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        s.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
    });
}

/// What the user reads when something went wrong: what happened and what to do.
fn failure_text(pt: bool, f: &Failure) -> String {
    match f {
        Failure::InUse => t(
            pt,
            "O FluxCapture está aberto — talvez gravando. Termine a gravação e feche o FluxCapture (se o ícone dele estiver perto do relógio, clique nele com o botão direito e escolha Sair). Depois clique em Tentar novamente.",
            "FluxCapture is open — maybe recording. Finish the recording and close FluxCapture (if its icon is next to the clock, right-click it and choose Quit). Then click Try again.",
        )
        .to_string(),
        Failure::Damaged(e) if e.is_empty() => {
            t(pt, "Este arquivo não contém o FluxCapture. Baixe o instalador novamente.", "This file does not contain FluxCapture. Download the setup program again.").to_string()
        }
        Failure::Damaged(e) => {
            format!("{}\n\n{e}", t(pt, "Este instalador está danificado ou incompleto. Baixe-o novamente.", "This setup program is damaged or incomplete. Download it again."))
        }
        Failure::NoSpace(need, have) => {
            let (need, have) = (need.div_ceil(1 << 20), have >> 20);
            if pt {
                format!("Não há espaço suficiente no disco: são necessários {need} MB e há {have} MB livres. Libere espaço e clique em Tentar novamente.")
            } else {
                format!("Not enough disk space: {need} MB needed, {have} MB free. Free some space and click Try again.")
            }
        }
        Failure::Io(e) => format!(
            "{}\n\n{e}",
            t(
                pt,
                "Não foi possível concluir. Verifique se a pasta escolhida permite gravação (ou use a pasta sugerida) e tente novamente.",
                "This could not be completed. Check that the chosen folder can be written to (or use the suggested folder) and try again.",
            )
        ),
    }
}

impl eframe::App for SetupApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        // Closing the window mid-copy would leave a half-written install.
        if self.worker.is_some() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.setup_native_window(frame);
        self.show(ui);
    }
}

impl SetupApp {
    fn t(&self, pt: &'static str, en: &'static str) -> &'static str {
        t(self.pt, pt, en)
    }

    fn installing(&self) -> bool {
        !matches!(self.mode, Mode::Uninstall(_))
    }

    /// winit only sets the small icon; the taskbar and Alt+Tab use the big one.
    fn setup_native_window(&mut self, frame: &eframe::Frame) {
        if std::mem::replace(&mut self.window_setup_done, true) {
            return;
        }
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(h) = frame.window_handle() else { return };
        let RawWindowHandle::Win32(w) = h.as_raw() else { return };
        let (big, small) = fluxcapture_platform::system::window_icon_sizes();
        if let Err(e) = fluxcapture_platform::system::set_window_icon(w.hwnd.get() as u64, (&app_icon::dib(big), big), (&app_icon::dib(small), small)) {
            log::info!("cannot set the window icon: {e}");
        }
    }

    fn start(&mut self) {
        self.progress.store(0, Ordering::Relaxed);
        let progress = self.progress.clone();
        let exe = self.exe.clone();
        let dir = self.dir.clone();
        let job: Box<dyn FnOnce() -> Result<bool, Failure> + Send> = if self.installing() {
            let plan = Plan { dir, desktop_shortcut: self.desktop_shortcut, register: true };
            Box::new(move || install::install(&exe, &plan, &mut |p| progress.store((p.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed)).map(|()| false))
        } else {
            let remove_settings = self.remove_settings;
            Box::new(move || install::uninstall(&dir, &exe, remove_settings, true))
        };
        match std::thread::Builder::new().name("setup".into()).spawn(job) {
            Ok(h) => {
                self.worker = Some(h);
                self.page = Page::Working;
            }
            Err(e) => self.page = Page::Failed(Failure::Io(e.to_string())),
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(worker) = self.worker.take_if(|w| w.is_finished()) else {
            if self.worker.is_some() {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            return;
        };
        // A panic in the worker is an error like any other (AGENTS.md §0.6).
        let result = worker.join().unwrap_or_else(|_| Err(Failure::Io("internal error".into())));
        self.page = match result {
            Ok(delete_self) => {
                self.delete_self.store(delete_self, Ordering::SeqCst);
                Page::Done
            }
            Err(f) => Page::Failed(f),
        };
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("header").frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(20, 14))).show(ui, |ui| self.header(ui));
        egui::Panel::bottom("buttons").frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(20, 12))).show(ui, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.buttons(ui, &ctx));
        });
        egui::CentralPanel::default().frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(24, 18))).show(ui, |ui| self.body(ui));
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
            let p = ui.painter();
            p.circle_filled(rect.center(), 20.0, ACCENT);
            p.circle_filled(rect.center(), 15.0, ui.visuals().panel_fill);
            p.circle_filled(rect.center(), 9.5, ERROR);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                let title = match (&self.mode, &self.installed) {
                    (Mode::Uninstall(_), _) => self.t("Desinstalar o FluxCapture", "Uninstall FluxCapture"),
                    (_, Some(_)) => self.t("Atualizar o FluxCapture", "Update FluxCapture"),
                    _ => self.t("Instalar o FluxCapture", "Install FluxCapture"),
                };
                ui.label(RichText::new(title).size(22.0).strong());
                ui.label(RichText::new(format!("{} {VERSION}", self.t("Versão", "Version"))).weak());
            });
        });
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        let pt = self.pt;
        match &self.page {
            Page::Ask => match self.mode {
                Mode::Install => self.ask_install(ui),
                Mode::Uninstall(_) => {
                    ui.label(self.t(
                        "O FluxCapture será removido deste computador.\n\nSuas gravações não serão apagadas.",
                        "FluxCapture will be removed from this computer.\n\nYour recordings are kept.",
                    ));
                    ui.add_space(12.0);
                    ui.checkbox(
                        &mut self.remove_settings,
                        t(pt, "Apagar também as configurações (perfis, cenas e registros)", "Also delete the settings (profiles, scenes and logs)"),
                    );
                }
                Mode::Empty => {
                    ui.colored_label(ERROR, failure_text(pt, &Failure::Damaged(String::new())));
                }
            },
            Page::Working => {
                ui.add_space(30.0);
                ui.label(if self.installing() { self.t("Instalando…", "Installing…") } else { self.t("Removendo…", "Removing…") });
                ui.add_space(6.0);
                let p = self.progress.load(Ordering::Relaxed) as f32 / 1000.0;
                ui.add(egui::ProgressBar::new(p).show_percentage().fill(ACCENT));
            }
            Page::Done if self.installing() => {
                ui.label(RichText::new(self.t("✔ Pronto! O FluxCapture foi instalado.", "✔ Done! FluxCapture is installed.")).size(18.0).color(ACCENT));
                ui.add_space(10.0);
                ui.label(if self.desktop_shortcut {
                    self.t(
                        "Para abri-lo depois, use o atalho FluxCapture na Área de Trabalho ou procure por FluxCapture no Menu Iniciar.",
                        "To open it later, use the FluxCapture shortcut on the desktop or search for FluxCapture in the Start menu.",
                    )
                } else {
                    self.t("Para abri-lo depois, procure por FluxCapture no Menu Iniciar.", "To open it later, search for FluxCapture in the Start menu.")
                });
                ui.add_space(14.0);
                ui.checkbox(&mut self.launch, t(pt, "Abrir o FluxCapture agora", "Open FluxCapture now"));
            }
            Page::Done => {
                ui.label(RichText::new(self.t("✔ O FluxCapture foi removido.", "✔ FluxCapture was removed.")).size(18.0).color(ACCENT));
                ui.add_space(10.0);
                ui.label(self.t("Suas gravações continuam na pasta Vídeos.", "Your recordings are still in the Videos folder."));
            }
            Page::Failed(f) => {
                ui.label(
                    RichText::new(if self.installing() {
                        self.t("Não foi possível instalar", "Setup could not finish")
                    } else {
                        self.t("Não foi possível desinstalar", "Uninstall could not finish")
                    })
                    .size(18.0)
                    .color(ERROR),
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical().show(ui, |ui| ui.label(failure_text(pt, f)));
            }
        }
    }

    fn ask_install(&mut self, ui: &mut egui::Ui) {
        let pt = self.pt;
        ui.label(match &self.installed {
            Some(v) => {
                if pt {
                    format!("O FluxCapture {v} já está instalado. Ele será atualizado; suas configurações e gravações são mantidas.")
                } else {
                    format!("FluxCapture {v} is already installed. It will be updated; your settings and recordings are kept.")
                }
            }
            None => self
                .t(
                    "O FluxCapture grava a tela, o som e a webcam do seu computador. A instalação leva poucos segundos e não pede senha de administrador.",
                    "FluxCapture records your screen, sound and webcam. Setup takes a few seconds and needs no administrator password.",
                )
                .to_string(),
        });
        ui.add_space(14.0);
        ui.label(RichText::new(t(pt, "Pasta de instalação", "Install folder")).weak());
        ui.horizontal(|ui| {
            let path = self.dir.display().to_string();
            ui.add(egui::Label::new(RichText::new(&path).monospace()).truncate()).on_hover_text(&path);
            if self.installed.is_none()
                && ui.button(t(pt, "Alterar…", "Change…")).clicked()
                && let Some(picked) = rfd::FileDialog::new().set_directory(self.dir.parent().unwrap_or(&self.dir)).pick_folder()
            {
                self.dir = install::dir_from_pick(&picked);
            }
        });
        ui.add_space(10.0);
        ui.checkbox(&mut self.desktop_shortcut, t(pt, "Criar um atalho na Área de Trabalho", "Create a desktop shortcut"));
    }

    fn buttons(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let close = |ctx: &egui::Context| ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        let primary = |ui: &mut egui::Ui, text: &str| ui.add(egui::Button::new(RichText::new(text).strong().color(Color32::WHITE)).fill(ACCENT)).clicked();
        match &self.page {
            Page::Ask => match self.mode {
                Mode::Install => {
                    let label = if self.installed.is_some() { self.t("Atualizar", "Update") } else { self.t("Instalar", "Install") };
                    if primary(ui, label) {
                        self.start();
                    }
                    if ui.button(self.t("Cancelar", "Cancel")).clicked() {
                        close(ctx);
                    }
                }
                Mode::Uninstall(_) => {
                    if primary(ui, self.t("Desinstalar", "Uninstall")) {
                        self.start();
                    }
                    if ui.button(self.t("Cancelar", "Cancel")).clicked() {
                        close(ctx);
                    }
                }
                Mode::Empty => {
                    if ui.button(self.t("Fechar", "Close")).clicked() {
                        close(ctx);
                    }
                }
            },
            Page::Working => {
                ui.add(egui::Spinner::new());
            }
            Page::Done => {
                if primary(ui, self.t("Concluir", "Finish")) {
                    if self.installing()
                        && self.launch
                        && let Err(e) = install::launch(&self.dir)
                    {
                        self.page = Page::Failed(Failure::Io(e));
                        return;
                    }
                    close(ctx);
                }
            }
            Page::Failed(_) => {
                if primary(ui, self.t("Tentar novamente", "Try again")) {
                    self.start();
                }
                if ui.button(self.t("Fechar", "Close")).clicked() {
                    close(ctx);
                }
            }
        }
    }
}
