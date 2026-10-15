# FluxCapture — instructions for agents

Read [`AGENTS.md`](AGENTS.md) first: its rules (never crash, never lose a recording, bounded queues,
measure before claiming, layering, licences) override everything here.

- **Start here:** [`README.md`](README.md) (features), [`docs/architecture.md`](docs/architecture.md)
  (crates, threads, data flow), [`docs/performance.md`](docs/performance.md) (how to measure),
  [`docs/decisions/`](docs/decisions) (why things are the way they are).
- **Gates before every commit:** `cargo xtask ci` (layers, fmt check, clippy `-D warnings`, tests).
  Tests that need ffmpeg/ffprobe skip without them; tests that need a GPU, monitor or audio device run
  on Windows machines only.
- **After every code change, rebuild the .exe:** the user runs `dist\fluxcapture.exe`, which only changes
  through `cargo xtask dist`. Finish any task that touched `apps/`, `crates/` or `xtask/` with the
  `build-exe` skill ([`.claude/skills/build-exe`](.claude/skills/build-exe/SKILL.md)); never kill a running
  instance (it may be recording).
- **`unsafe` lives only in `crates/platform`**, with a `// SAFETY:` comment on every block and a safe,
  `Result`-returning API. Check windows-rs signatures in the cargo registry source before writing FFI.
- **Run the app:** `cargo run -p fluxcapture`. To drive it, set `advanced.control_port` (or use a test
  configuration via `FLUXCAPTURE_CONFIG_DIR`) and send JSON lines (see docs/architecture.md). Look at the
  result: `PrintWindow` the app window rather than screenshotting the user's screen.
- **End-to-end checks:** `fluxcapture-cli record --seconds 5 --out target/x --config target/cfg`, then
  `ffprobe -count_frames` and `ffmpeg -v error -i FILE -f null -`. Never leave test recordings in the
  user's Videos folder.
- **Performance changes** come with `fluxcapture-cli bench` numbers before and after.
