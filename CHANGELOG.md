# Changelog

All notable changes to **Hyprcap** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project roughly follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Audio matrix** (system / app / mic → one mixed track):
  - **System sound:** Off · All PC sound (default sink monitor, or choose sink) · One app (playing PipeWire/Pulse stream)
  - **Microphone:** optional, with device picker
  - Mic + system/app mixed into a **single** audio track via temporary Pulse null-sink + loopbacks
  - App mode re-routes the sink-input so capture works while you still hear the app
- GUI controls for the matrix (replaces the single “System audio” checkbox)
- IPC: `audio_plan` on start commands; `list_audio` inventory (sinks, mics, apps)
- CLI: `--system off|all|app`, `--audio-sink`, `--audio-app`, `--mic`, `--mic-device`
- CLI: `hyprcap list-audio` (JSON inventory)
- Config sticky fields: `system_audio`, `audio_sink`, `audio_app`, `mic_default`, `mic_device`
- Legacy `--audio` / `audio_default` still map to system **all** on the default sink

### Changed

- `wf-recorder` is now invoked with `-aDEVICE` (explicit Pulse source) instead of bare `-a`, so “system audio” captures the **sink monitor** (PC sound) rather than the default input (often the mic)
- Both-monitors mode still attaches audio only on the **primary** head (one track after stitch)

### Fixed

- Desktop/system sound not recorded when the Pulse default source was a microphone

---

## [0.1.0] — 2026-03 (initial public release)

### Added

- Region / one-monitor / two-monitor capture around `wf-recorder` + `slurp`
- GTK4 / libadwaita GUI and CLI for Hyprland
- Dual-monitor layout-true stitch (exactly two heads)
- AUR package `hyprcap-git`
- Desktop entry and icons

[Unreleased]: https://github.com/FrancoEscob/hyprcap/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/FrancoEscob/hyprcap/releases/tag/v0.1.0
