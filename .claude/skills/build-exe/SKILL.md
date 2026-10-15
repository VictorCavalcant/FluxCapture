---
name: build-exe
description: Rebuild the release .exe files (dist/fluxcapture.exe, dist/fluxcapture-cli.exe) after ANY change to code, settings defaults, UI or assets in FluxCapture, so the user always runs the latest version. Use at the end of every task that edited files under apps/, crates/ or xtask/, and whenever the user says the change "is not showing in the .exe".
---

# build-exe — keep `dist/*.exe` in sync with the code

The user runs `dist\fluxcapture.exe`, not `cargo run`. Editing source does **not** change it until it is
rebuilt, so every task that touches Rust code ends with this skill.

## Steps

1. **Gate first.** `cargo xtask ci` must pass (layers, fmt, clippy `-D warnings`, tests). If it fails,
   fix the cause; never build around a failing gate. (Skip only if nothing under `apps/`, `crates/`
   or `xtask/` changed — docs-only edits need no rebuild.)
2. **Check for a running instance** (it locks the .exe, and it may be recording):
   ```powershell
   Get-Process fluxcapture, fluxcapture-cli -ErrorAction SilentlyContinue | Select-Object Id, Path
   ```
   - **Never kill it.** AGENTS.md §0: a recording must not be lost. Ask the user to stop/close it, or
     build anyway and handle the locked file in step 4.
3. **Build and publish:** `cargo xtask dist` (release build, fat LTO — takes a few minutes; run it in
   the background and wait for the notification). It copies the binaries to `dist/` and packs them
   into the installer `dist/FluxCapture-Setup-<version>.exe` (the file to give to other people).
   Don't pipe it through `tail`/`head`: that hides the exit code. The line `✗ missing <exe>` means the
   copy to `dist/` failed even though the build succeeded (a running instance, or antivirus scanning the
   fresh binary); `xtask` stops at the first failed copy, so the other binary is stale too.
4. **If the copy fails**, retry once with `Copy-Item target\release\<exe> dist\<exe> -Force` for both
   binaries. If the .exe is locked by a running instance, copy the fresh binary next to it instead:
   ```powershell
   Copy-Item target\release\fluxcapture.exe dist\fluxcapture.new.exe -Force
   ```
   and tell the user to close the app and rename it (or re-run this skill after closing it).
5. **Verify** the result, don't assume it:
   ```powershell
   Get-ChildItem dist\*.exe | Select-Object Name, Length, LastWriteTime
   ```
   The setup file is only written after both binaries were copied; if it is older than them, the
   packing step failed (see the `✗` line).
   `LastWriteTime` must be newer than the last source edit. Optionally smoke-test the CLI:
   `dist\fluxcapture-cli.exe devices`.
6. **Report** in one or two lines: the path, the timestamp, and anything the user has to do (e.g.
   close the running app). Do not commit `dist/` or `target/`.

## Notes

- Portable mode (`portable.txt` next to the .exe) keeps config beside the binary; rebuilding never
  touches `config\`.
- Performance-path changes also need `fluxcapture-cli bench` before/after (see `docs/performance.md`).
- Do not use `cargo build` alone: it writes `target\debug`, which is not what the user launches.
