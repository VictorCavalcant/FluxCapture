//! Notification-area icon and menu.

use fluxcapture_engine::Command;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub enum Action {
    Show,
    Command(Command),
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    record: MenuItem,
    ids: Vec<(MenuId, u8)>,
    recording: bool,
    idle_icon: Option<Icon>,
    rec_icon: Option<Icon>,
    en: bool,
}

/// A procedurally drawn icon: a ring, filled red while recording.
fn icon(recording: bool) -> Option<Icon> {
    let n = 32u32;
    let mut rgba = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            let px: [u8; 4] = if d < 7.0 {
                if recording { [230, 57, 70, 255] } else { [180, 180, 190, 255] }
            } else if (11.0..14.5).contains(&d) {
                [0, 168, 150, 255]
            } else {
                [0, 0, 0, 0]
            };
            let i = ((y * n + x) * 4) as usize;
            if let Some(p) = rgba.get_mut(i..i + 4) {
                p.copy_from_slice(&px);
            }
        }
    }
    Icon::from_rgba(rgba, n, n).ok()
}

impl Tray {
    pub fn new(en: bool) -> Result<Tray, String> {
        let t = |pt: &str, e: &str| if en { e.to_string() } else { pt.to_string() };
        let menu = Menu::new();
        let show = MenuItem::new(t("Mostrar FluxCapture", "Show FluxCapture"), true, None);
        let record = MenuItem::new(t("Gravar", "Record"), true, None);
        let pause = MenuItem::new(t("Pausar / retomar", "Pause / resume"), true, None);
        let replay = MenuItem::new(t("Ligar / desligar replay", "Replay on / off"), true, None);
        let save = MenuItem::new(t("Salvar replay", "Save replay"), true, None);
        let shot = MenuItem::new(t("Captura de tela", "Screenshot"), true, None);
        let quit = MenuItem::new(t("Sair", "Quit"), true, None);
        let sep = PredefinedMenuItem::separator();
        menu.append_items(&[&show, &sep, &record, &pause, &replay, &save, &shot, &PredefinedMenuItem::separator(), &quit]).map_err(|e| e.to_string())?;
        let ids = vec![
            (show.id().clone(), 0),
            (record.id().clone(), 1),
            (pause.id().clone(), 2),
            (replay.id().clone(), 3),
            (save.id().clone(), 4),
            (shot.id().clone(), 5),
            (quit.id().clone(), 6),
        ];
        let idle_icon = icon(false);
        let mut b = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("FluxCapture");
        if let Some(i) = idle_icon.clone() {
            b = b.with_icon(i);
        }
        let icon = b.build().map_err(|e| e.to_string())?;
        Ok(Tray { icon, record, ids, recording: false, idle_icon, rec_icon: self::icon(true), en })
    }

    pub fn poll(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        while let Ok(e) = MenuEvent::receiver().try_recv() {
            let Some((_, k)) = self.ids.iter().find(|(id, _)| *id == e.id) else { continue };
            out.push(match k {
                0 => Action::Show,
                1 => Action::Command(Command::ToggleRecording),
                2 => Action::Command(Command::TogglePause),
                3 => Action::Command(Command::ToggleReplay),
                4 => Action::Command(Command::SaveReplay),
                5 => Action::Command(Command::Screenshot),
                _ => Action::Quit,
            });
        }
        while let Ok(e) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = e {
                out.push(Action::Show);
            }
        }
        out
    }

    pub fn set_recording(&mut self, on: bool) {
        if on == self.recording {
            return;
        }
        self.recording = on;
        let _ = self.icon.set_icon(if on { self.rec_icon.clone() } else { self.idle_icon.clone() });
        let _ = self.icon.set_tooltip(Some(if on { "FluxCapture — REC" } else { "FluxCapture" }));
        self.record.set_text(match (on, self.en) {
            (true, true) => "Stop recording",
            (true, false) => "Parar gravação",
            (false, true) => "Record",
            (false, false) => "Gravar",
        });
    }
}
