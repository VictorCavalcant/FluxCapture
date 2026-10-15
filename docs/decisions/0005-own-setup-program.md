# 0005 — Our own setup program, per-user, no administrator prompt

**Status:** accepted

## Context

People who have never built a Rust project need one file to download and double-click. The usual
tools (Inno Setup, NSIS, WiX/MSI) are extra toolchains to install on every build machine, each with
its own script language, and some bring licences outside AGENTS.md §2.

## Decision

- `apps/fluxcapture-setup` is a small egui program. `cargo xtask dist` appends the release binaries
  to it (`[program][files][index][footer]`, see `apps/fluxcapture-setup/src/payload.rs`) and writes
  `dist/FluxCapture-Setup-<version>.exe`. Every file carries an FNV-1a checksum, so a broken download
  is reported instead of installed.
- The install is **per user**: `%LOCALAPPDATA%\Programs\FluxCapture`, a Start menu shortcut, an
  optional desktop shortcut and an entry in Settings › Apps (`HKCU\…\Uninstall\FluxCapture`). No UAC
  prompt; the embedded manifest (`asInvoker`) stops Windows from guessing that a program called
  "setup" needs one.
- The uninstaller is the same program without the appended files (`uninstall.exe`). It deletes only
  the files it knows by name, never recordings, and the settings only when the user ticks the box.
- A running FluxCapture (which may be recording) is never closed: setup asks the user to close it.
- If no OpenGL window can be opened, setup falls back to plain message boxes.

## Consequences

- One toolchain (cargo) builds everything; the shell and registry calls live in
  `platform::install` like every other OS call.
- The setup program is not code-signed: SmartScreen shows "Windows protected your PC" until it is
  signed with a certificate (More info › Run anyway).
- Installing for all users (Program Files, HKLM) is not supported; nothing needs it so far.
