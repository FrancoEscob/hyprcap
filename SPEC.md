# Hyprcap (formerly record-ui) — Spec v1

**Status:** `ready-for-agent` (post adversarial product + architecture review)  
**Previous:** draft v0  
**Project path:** `~/Projects/record-ui`  
**Greenfield.** Native frontend for `wf-recorder` on Hyprland / wlroots Wayland.

Does **not** reimplement screen capture. Does **not** put video bytes on the clipboard (path only).

---

## Problem Statement

On Hyprland, short screen clips need **region selection**, clear start/stop, and knowing where the file went. Existing options fail that workflow:

- Flameshot is screenshots only (no video).
- GPU Screen Recorder’s UI is ShadowPlay-oriented; region and clipboard feel wrong on Wayland.
- Spectacle is Plasma-centric and unreliable outside KDE for recording.
- `wf-recorder` works well with `slurp` but is pure CLI: easy to not know if it recorded, how to stop cleanly, or where the file landed.

The user wants a small native app: pick a region, record, stop without fighting a terminal, get a valid file, and the **path** on the clipboard.

## Solution

Ship **record-ui**: a Rust tool with:

1. A **session server** (daemon-on-demand) that owns the exclusive recording **session** (Region/One = one `wf-recorder` child; **Both** = two process groups + post-stop `ffmpeg` stitch).
2. A **CLI client** for Hyprland keybinds (`toggle-region`, `stop`, `status`) that never initializes GTK.
3. An optional **GTK4 + libadwaita** window as a view on the same session.

Users select a region via `slurp`, record with optional system audio, see state/timer (in GUI or via `status`), stop via button or the same toggle keybind, and get a notification plus clipboard copy of the absolute output path when stop succeeds.

Capture/encoding remains entirely in `wf-recorder`.

---

## Normative process model (v1)

**Model: daemon-on-demand (single owner).** This supersedes any “single process only” wording from v0.

1. First `record-ui` invocation that needs the session **starts a server**:
   - Binds Unix socket `$XDG_RUNTIME_DIR/record-ui.sock` with mode `0600`.
   - Writes `$XDG_RUNTIME_DIR/record-ui.pid`.
   - Owns the sole `Recorder` and the exclusive recording **session**: Region/One = one `wf-recorder` child; **Both** = two `wf-recorder` process groups under the same session, plus blocking post-stop `ffmpeg` layout-true stitch.
2. Later invocations are **clients**: send one command over the socket, print result, exit (except `gui`, which stays connected as a view).
3. If bind fails with address in use → connect as client. If connect fails and PID file points to a dead process → remove stale socket/pid and become server.
4. **GUI is a client view.** Closing the window **disconnects the view only**; the server keeps recording if active.
5. **Server exit policy:** when state is `Idle` and no GUI client is connected, the server may exit immediately (v1 default: exit when idle and last client disconnects). Explicit `record-ui quit` forces stop-if-recording then server exit.
6. **CLI paths must not initialize GTK** (no Adwaita/GTK init on `toggle-region`, `stop`, `status`, `region`, `fullscreen`, `both`, `quit`).
7. At most one user-visible recording **session** managed by this app (Region/One: one OS child; Both: dual recorder children + optional stitch child during stop). External `wf-recorder` instances are ignored.

---

## User Stories

1. As a Hyprland user, I want to record a rectangular region, so that I share only the relevant UI.
2. As a Hyprland user, I want to start recording without a long-lived terminal, so that my workflow stays uninterrupted.
3. As a Hyprland user, I want a clear Stop control, so that I do not corrupt the file with a hard kill.
4. As a Hyprland user, I want stop to use cooperative SIGINT, so that the container finalizes correctly.
5. As a Hyprland user, I want to see whether recording is active, so that I am not guessing.
6. As a Hyprland user, I want an elapsed timer while recording (GUI), so that I know clip length.
7. As a Hyprland user, I want optional system audio via `wf-recorder -a`, so that demos with sound work.
8. As a Hyprland user, I want audio off by default, so that silent/private clips are the safe default.
9. As a Hyprland user, I want timestamped files under the XDG Videos directory, so that clips are easy to find and do not overwrite each other.
10. As a Hyprland user, I want a notification when recording finishes successfully, so that I know the path without logs.
11. As a Hyprland user, I want the absolute output path copied to the clipboard on success, so that I can paste it into chat or dialogs.
12. As a Hyprland user, I want a single keybind that toggles region recording, so that I can start and stop without the mouse.
13. As a Hyprland user, when I press toggle while **recording**, I want stop **without** opening slurp again.
14. As a Hyprland user, when I press toggle while **selecting a region**, I want selection canceled and return to idle (nothing records).
15. As a Hyprland user, when I open the GUI while already recording, I want live state/timer and Stop (not a second session).
16. As a Hyprland user, I want region selection via `slurp`, so that it matches native screenshot selection.
17. As a Hyprland user, I want canceling slurp (Esc) to abort cleanly with no success toast.
18. As a Hyprland user, I want missing hard dependencies reported clearly, so that I know what to install.
19. As a Hyprland user, I want a second start while busy to fail with a clear message and non-zero CLI exit, so that I never get two recorders.
20. As a Hyprland user, I want the last successful path visible in the GUI, so that I can reopen the clip.
21. As a Hyprland user, I want Open last file / Open folder via `xdg-open`, so that I can review quickly.
22. As a Hyprland user, I want a small floating window that does not dominate the workspace.
23. As a Hyprland user, I want CLI subcommands for scripting and keybinds, so that the GUI is optional.
24. As a Hyprland user, I want a `.desktop` entry for launchers (walker, etc.).
25. As a power user, I want file-based config for output dir and defaults, so that I do not reconfigure every run.
26. As a power user, I want machine-readable `status` and stable CLI exit codes, so that waybar/scripts work.
27. As a Hyprland user, if notify or clipboard fails after a good file, I still want the path in UI/CLI and a warning—not a fake total failure of the recording.
28. As a Hyprland user, I want start feedback for keybind-only flows (short “Recording started” notify when start came from CLI without GUI), so that I am not blind until stop.
29. As a Hyprland user, I want multi-monitor region capture via slurp geometry (same as known-good CLI), so that multi-head works without a custom picker.
30. As a developer, I want one testable `Recorder` behind CLI and GUI, so that behavior stays consistent.
31. As a Hyprland user, I want one-monitor fullscreen recording (`wf-recorder -o NAME`) as a secondary mode: pin `fullscreen_output` or pass `--output` / IPC `output` when multi-monitor; sole head auto-resolves; never market as all monitors. GUI monitor + FPS pickers and Both mode: see `docs/DUAL-MONITOR.md`.
32. As a Hyprland user, I want notify text to say that the **path** was copied (not the video), so that Discord/Ctrl+V expectations are honest.
33. As a Hyprland user, I want stale locks/sockets recovered when the previous server died, so that the app does not stay “busy” forever.
34. As a Hyprland user, I want `record-ui stop` to always be able to stop a session started from GUI or keybind, so that I recover without a tray icon.

## Implementation Decisions

### Product boundary
- **Wrapper, not encoder.** Capture/encode: `wf-recorder`. Region: `slurp`. Notify: `notify-send`. Clipboard path: `wl-copy`. Open: `xdg-open`.
- **Target platform:** wlroots/Hyprland + `wf-recorder` (wlr-screencopy). Not xdg-desktop-portal capture. On failure, surface the last ~4KiB of child stderr.
- **Binary / crate name:** `record-ui`.
- **Language/UI:** Rust stable; GTK4 + libadwaita **only on the `gui` path**.
- **Clipboard:** absolute filesystem path as text only. Never claim video MIME paste into apps.
- **Config:** file-only in v1 (no settings page). Path: `$XDG_CONFIG_HOME/record-ui/config.toml`.

### Config defaults (normative)

| Key | Default | Meaning |
|-----|---------|---------|
| `output_dir` | XDG Videos (`xdg-user-dir VIDEOS` if available, else `~/Videos`) | Where files are written |
| `audio_default` | `false` | System audio off unless toggled/CLI overrides |
| `copy_path` | `true` | `wl-copy` absolute path on success |
| `notify` | `true` | Desktop notifications |
| `notify_on_start_cli` | `true` | Start notify when start has no GUI client (includes output name when known) |
| `stop_timeout_ms` | `5000` | Wait after SIGINT before escalation |
| `stop_term_timeout_ms` | `2000` | Wait after SIGTERM before hard failure |
| `fullscreen_output` | *(unset)* | Wayland output for one-monitor fullscreen (`-o`). **Required when ≥2 outputs** (no focus auto-pick). Sole output auto-used when inventory length is 1. |

Create `output_dir` if missing. Filename: `rec-YYYYMMDD-HHMMSS.mp4`; if exists, append `-1`, `-2`, … Same-second collisions must not overwrite.

### CLI surface (normative)

| Command | Behavior |
|---------|----------|
| `record-ui` / `record-ui gui` | Ensure server; open/raise GUI client |
| `record-ui region [--audio]` | Start region recording (error if busy) |
| `record-ui fullscreen [--audio] [--output NAME]` | Start one-monitor capture with `-o NAME` (error if busy / unresolved) |
| `record-ui list-outputs` | Print inventory: `name\tx\ty\tw\th\trefresh` when geometry known (hyprctl); name-only fallback (`wf-recorder -L`). No daemon. |
| `record-ui toggle-region [--audio]` | Idle→region start; SelectingRegion→cancel slurp; Recording→stop; Stopping→idempotent wait/no-op |
| `record-ui stop` | Stop if recording/selecting; no-op success if idle |
| `record-ui status` | Print one JSON object on stdout (see below) |
| `record-ui quit` | Stop if needed; shutdown server |

**`status` JSON fields:** `state`, `output_path` (current or null), `pid` (primary recorder or null), `started_at_unix` (or null), `audio` (bool), `last_error` (string or null), `last_success_path` (string or null), `elapsed_ms` (or null), `capture_output` (resolved `-o` name while active, or null).

**CLI exit codes:**

| Code | Meaning |
|------|---------|
| 0 | Success / idle no-op stop / cancel slurp treated as clean abort for toggle |
| 1 | General failure (spawn, deps, I/O) |
| 2 | Busy (`AlreadyBusy`) |
| 3 | Not recording when a command required it (if we distinguish; `stop` uses 0 for no-op) |
| 4 | Dependency missing |

### IPC protocol (normative)

- Socket: `$XDG_RUNTIME_DIR/record-ui.sock` (0600).
- Framing: newline-delimited JSON (one request object per line; one response object per line).
- Requests: `ping`, `status`, `start_region`, `start_fullscreen`, `stop`, `shutdown`, `subscribe` (GUI may use subscribe or poll status).
- Request fields include optional `audio: bool` for starts.
- Response: `{ "ok": bool, "code": "<machine_code>", "message": "...", "status": { ... } }`.
- Machine codes: `ok`, `busy`, `not_recording`, `dep_missing`, `slurp_cancel`, `spawn_failed`, `stop_timeout`, `io_error`, `invalid`.

### State machine (normative)

States: `Idle` | `SelectingRegion` | `Starting` | `Recording` | `Stopping`.

```
Idle --start_region--> SelectingRegion
SelectingRegion --geometry--> Starting --child_running--> Recording
SelectingRegion --cancel/empty/toggle/stop--> Idle
Idle --start_fullscreen--> Starting --child_running--> Recording
Recording --stop/toggle--> Stopping --reaped--> Idle
Stopping --stop--> Stopping (idempotent)
Any non-Idle start_* --> error busy (stay in state)
Starting --spawn_fail--> Idle + error
```

**Busy** = any state other than `Idle`.  
**Exclusive ownership:** server acquires the right to start before running `slurp` (no second client starts slurp concurrently).  
**Lock/TOCTOU:** only the server spawns; clients never spawn `wf-recorder` themselves.

### Spawn / signal contract (normative)

- Always **argv arrays**; never `sh -c`.
- Spawn `wf-recorder` in a **new process group**; normal stop = `SIGINT` to the **process group**.
- Capture stderr (and optionally stdout) into a **bounded buffer** (~4KiB retained for errors).
- Always **reap** the child; never intentionally orphan.
- **Stop escalation:** SIGINT → wait `stop_timeout_ms` → SIGTERM to group → wait `stop_term_timeout_ms` → mark `stop_timeout` failure, force `Idle`, release session for new starts, warn that file may be corrupt. No silent SIGKILL; SIGKILL only if required to reap after TERM timeout and must be logged as nuclear.
- Server shutdown while `Recording` = same cooperative stop path.

### Region / fullscreen argv (normative v1)

- Region: `wf-recorder -g <slurp_stdout_trim> [-a] -f <path>`
- Fullscreen (one monitor): `wf-recorder -o <NAME> [-a] -f <path>` (no `-g`; **`-o` always required**)
- `slurp`: no args; **cancel** = non-zero exit **or** empty stdout → `slurp_cancel`, no error toast (clean abort).
- Geometry contract: slurp default `x,y WxH` passed through unchanged. Region geometry must lie entirely on **one** head (engine fails cross-output geometry); not a multi-head canvas.
- Fullscreen / One resolve + Both pipeline: **`docs/DUAL-MONITOR.md`** (GUI pickers, FPS, dual capture, layout-true stitch). No focus auto-pick when ≥2 heads for One.

### Success / failure after stop (normative)

Ordered evaluation after reap:

1. If wait timed out / nuclear kill → **Failure** (`stop_timeout`).
2. If output path missing or size == 0 → **Failure**.
3. If file size > 0 and stop was cooperative (SIGINT/SIGTERM path) → **Success** even if child exit code is non-zero (common with ffmpeg-style tools). Capture exit code in logs.
4. If exit code non-zero **and** not a cooperative stop context → **Failure**.
5. Do **not** require `ffprobe` in v1. Manual acceptance uses a real player for “playable.”

**On Success:**

- Update `last_success_path`.
- If `notify`: notification title/body includes absolute path and wording that **path** was copied (if copy enabled).
- If `copy_path`: `wl-copy` absolute path (best effort).
- Hook failures (notify/clipboard) → **SuccessWithWarnings** (still ok for CLI exit 0; surface warning in message/status).

**On Failure:**

- No success clipboard claim; error notify if `notify`; `last_error` set; stderr tail in message when available.

### Soft vs hard dependencies

| Binary | Class | Missing behavior |
|--------|-------|------------------|
| `wf-recorder` | Hard | Fail start; exit 4 |
| `slurp` | Hard for region | Fail region start; exit 4 |
| `ffmpeg` | Hard for **Both** only | Fail Both start; `dep_missing` |
| `hyprctl` | Soft inventory; **hard for Both layout** | Fail Both if positions unavailable |
| `notify-send` | Soft | Degrade; warn once |
| `wl-copy` | Soft | Degrade; warn once |
| `xdg-open` | Soft for open actions | Fail only that action |

### GUI behavior

- Small Adwaita window: mode (**Region | One monitor | Both**), monitor list + FPS list (One only), audio toggle, primary Record/Stop, state, timer, last path, Open file / Open folder.
- **Multi-monitor product (normative detail):** `docs/DUAL-MONITOR.md` — monitor picker, FPS picker (native/30/60/Auto), Both = dual `wf-recorder` @ 60 + **post-stop layout-true** ffmpeg compose (black voids; no same-height scale). Human path for One/Both = open GUI; region keybind stays `toggle-region`.
- Before `slurp` from GUI: avoid focus fights where reasonable; keybind path calls CLI without mapping a window first.
- Timer uses server `started_at_unix` / `elapsed_ms` so a GUI that attaches mid-recording shows the correct elapsed time.
- Microcopy on success: path copied, not video content.

### Hyprland integration

- **Docs only** in v1: example bind Super+Shift+R → `record-ui toggle-region`; window rules for floating. Do not auto-edit the user’s hypr config.
- Document that Hyprland `exec` PATH may differ from interactive shell; recommend absolute path or user PATH fix if command not found.

### Modules (logical)

- **server** — socket accept loop, owns `Recorder`.
- **client/cli** — parse args, talk socket, no GTK.
- **recorder** — state machine, spawn/stop, paths.
- **ports** — injectable: CommandSpawner, ChildHandle, Clock, Notifier, Clipboard, Paths/Env.
- **config** — load TOML defaults.
- **ui** — GTK view client only.

### Primary test seam

One seam: **`Recorder` + ports** (spawner, child signals/wait, clock, paths, notifier, clipboard).  
Server IPC is a thin adapter over the same controller.  
UI is a thin adapter.  
Ideal automated coverage does not need a Wayland compositor.

## Testing Decisions

### What good tests look like

- Assert **external behavior**: argv vectors, signals, state transitions, exit codes, hooks invoked or not.
- Do **not** make GTK widget trees the primary suite.
- Do **not** require a live Wayland session for unit tests.
- Prefer argv arrays; tests fail the design if shell interpolation appears.

### Mandated unit tests (mock spawner + fake child + fake clock + temp dirs)

| ID | Scenario | Expect |
|----|----------|--------|
| U1 | region start, slurp returns geom | argv has `-g`, geom, `-f`; state Recording |
| U2 | slurp empty/cancel | no wf-recorder; Idle; slurp_cancel |
| U3 | audio on/off | `-a` present/absent |
| U4 | fullscreen start | no `-g`; argv has `-o` + non-empty name |
| U5 | double start | second busy; one child |
| U6 | stop from Recording | SIGINT to **process group**; wait; Idle |
| U7 | double stop | idempotent |
| U8 | stop/toggle during SelectingRegion | slurp killed; Idle |
| U9 | toggle while Recording | stop, **no** slurp |
| U10 | spawn fail | Idle + error; no success hooks |
| U11 | cooperative stop, non-zero exit, file size OK | Success; hooks run |
| U12 | file missing/empty after stop | Failure; no success clipboard |
| U13 | stop timeout | escalation; Failure; can start again |
| U14 | path collision same second | unique filename |
| U15 | notify/wl-copy fail | SuccessWithWarnings |
| U16 | missing hard dep | clear error; exit semantics |
| U17 | config defaults + overrides | applied to paths/audio |

### Mandated IPC tests (temp `XDG_RUNTIME_DIR`, no GTK, no Wayland)

| ID | Scenario | Expect |
|----|----------|--------|
| I1 | start then status from second process | Recording + path |
| I2 | toggle-region twice | start then stop |
| I3 | concurrent start_region | exactly one Recording |
| I4 | stale socket + dead pid | recover and serve |
| I5 | stop with server idle | clean no-op |

### Manual Hyprland acceptance

| ID | Criterion |
|----|-----------|
| M1 | Region ≥3s, stop, file opens in a player, duration ≥2s |
| M2 | Multi-monitor region |
| M3 | Audio on/off smoke |
| M4 | Keybind toggle spam (no double recorder) |
| M5 | Close GUI while recording; `stop` still works; re-open GUI shows state or idle after stop |
| M6 | Esc in slurp → no file, no success notify |
| M7 | Missing `wf-recorder` messaging |

### Success criteria (v1 done)

1. M1 passes on the user’s Hyprland session.
2. GUI shows Idle/Recording and a ticking timer while recording.
3. Unit U6 proves SIGINT-to-group stop (not SIGKILL as first action).
4. Success path notifies + copies absolute path once; failure does not claim success.
5. `toggle-region` works **without opening the GTK window**; second invocation stops without running slurp.
6. U2/U5/I3/I4 pass.
7. Close-GUI policy matches daemon model (recording continues until stop/quit).

## Out of Scope

| Item | Mitigation in v1 |
|------|------------------|
| Tray icon | CLI `stop`/`status` + daemon; start notify for CLI |
| Video MIME clipboard | Explicit “path copied” copy |
| Replay buffer / streaming / webcam | — |
| Codec/CRF/VAAPI picker | wf-recorder defaults; manual quality later |
| GUI output/monitor + FPS pickers | **In scope** — `docs/DUAL-MONITOR.md` |
| Dual-monitor “Both” layout-true stitch | **In scope** (post-stop compose) — `docs/DUAL-MONITOR.md` |
| Same-height scaled hstack Both | Out — user rejected |
| Live remux / async Composing IPC | Out of this slice — stop blocks through stitch |
| Portal-based capture engine | Error surfaces stderr |
| Auto-edit Hyprland config | Docs examples only |
| Flathub / full packaging | Local `cargo install` / PATH notes |
| i18n | English (or author language) strings OK |
| Pause recording | Stop only |
| Mic vs system matrix | Single `-a` boolean |
| Auto-upload | — |
| Windows/macOS | — |

## Further Notes

### Discovery context

- Hyprland 0.56, Wayland, AMD; tools present: `wf-recorder`, `slurp`, `wl-clipboard`, `libnotify`, GTK4, libadwaita, Rust 1.97.
- Proven CLI: `wf-recorder -g "$(slurp)" -f ~/Videos/rec-….mp4` → valid ~12s clip.
- User rejected GSR UI (region/clipboard) and Spectacle as primary; chose custom wrapper.

### Adversarial review summary (v0 → v1)

Two independent reviews (product/UX + architecture) both returned **revise**. v1 resolves:

- Process ownership → **daemon-on-demand**
- Close window → **view disconnect, not kill**
- Toggle during slurp/recording → **explicit state transitions**
- Success after SIGINT → **cooperative stop + non-empty file**
- CLI vs GTK → **no GTK init on CLI**
- IPC → **socket + JSON lines + exit codes**
- Stop hang → **SIGINT → SIGTERM timeouts**
- Filename collisions, soft deps, status JSON, test matrix

### Issue tracker

No remote tracker configured (`gh` not available). This file is the agent source of truth. Treat **v1** as `ready-for-agent` for implementation.

### Suggested first implementation slices

1. Ports + `Recorder` state machine + unit tests (no GTK).  
2. Server/client IPC + CLI commands + IPC tests.  
3. GUI view client.  
4. Manual Hyprland acceptance + example binds in README.
