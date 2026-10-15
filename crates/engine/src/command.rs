//! Everything is a command: UI buttons, global hotkeys, the tray menu, the CLI and the control
//! channel dispatch these.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "args", rename_all = "snake_case")]
pub enum Command {
    ToggleRecording,
    StartRecording,
    StopRecording,
    TogglePause,
    ToggleReplay,
    SaveReplay,
    Screenshot,
    ToggleMicMute,
    /// Selects a profile by index.
    SetProfile(usize),
    /// Selects a scene by index.
    SetScene(usize),
    OpenRecordingsFolder,
    /// Applies proposal `n` of the performance advisor.
    ApplyProposal(usize),
    /// Re-enumerates monitors, windows and devices.
    RefreshDevices,
    ResetAudioClip,
}

impl Command {
    /// Stable id for the control channel and logs.
    pub fn id(&self) -> &'static str {
        match self {
            Command::ToggleRecording => "record.toggle",
            Command::StartRecording => "record.start",
            Command::StopRecording => "record.stop",
            Command::TogglePause => "record.pause",
            Command::ToggleReplay => "replay.toggle",
            Command::SaveReplay => "replay.save",
            Command::Screenshot => "screenshot",
            Command::ToggleMicMute => "mic.toggle",
            Command::SetProfile(_) => "profile.set",
            Command::SetScene(_) => "scene.set",
            Command::OpenRecordingsFolder => "folder.open",
            Command::ApplyProposal(_) => "advisor.apply",
            Command::RefreshDevices => "devices.refresh",
            Command::ResetAudioClip => "audio.reset_clip",
        }
    }

    /// Parses a control-channel command: `{"name":"record.toggle"}` or `{"name":"profile.set","arg":1}`.
    pub fn from_id(id: &str, arg: Option<u64>) -> Option<Command> {
        let n = arg.and_then(|a| usize::try_from(a).ok());
        Some(match id {
            "record.toggle" => Command::ToggleRecording,
            "record.start" => Command::StartRecording,
            "record.stop" => Command::StopRecording,
            "record.pause" => Command::TogglePause,
            "replay.toggle" => Command::ToggleReplay,
            "replay.save" => Command::SaveReplay,
            "screenshot" => Command::Screenshot,
            "mic.toggle" => Command::ToggleMicMute,
            "profile.set" => Command::SetProfile(n?),
            "scene.set" => Command::SetScene(n?),
            "folder.open" => Command::OpenRecordingsFolder,
            "advisor.apply" => Command::ApplyProposal(n?),
            "devices.refresh" => Command::RefreshDevices,
            "audio.reset_clip" => Command::ResetAudioClip,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for c in [
            Command::ToggleRecording,
            Command::StartRecording,
            Command::StopRecording,
            Command::TogglePause,
            Command::ToggleReplay,
            Command::SaveReplay,
            Command::Screenshot,
            Command::ToggleMicMute,
            Command::SetProfile(2),
            Command::SetScene(0),
            Command::OpenRecordingsFolder,
            Command::ApplyProposal(1),
            Command::RefreshDevices,
            Command::ResetAudioClip,
        ] {
            let arg = match c {
                Command::SetProfile(n) | Command::SetScene(n) | Command::ApplyProposal(n) => Some(n as u64),
                _ => None,
            };
            assert_eq!(Command::from_id(c.id(), arg), Some(c));
        }
        assert_eq!(Command::from_id("profile.set", None), None);
        assert_eq!(Command::from_id("nope", None), None);
        assert_eq!(Command::from_id("scene.set", Some(u64::MAX)).is_some(), usize::try_from(u64::MAX).is_ok());
    }
}
