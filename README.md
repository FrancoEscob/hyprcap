# record-ui

Native frontend for [`wf-recorder`](https://github.com/ammen99/wf-recorder) on **Hyprland / wlroots**.

- **Daemon-on-demand** session server owns at most one `wf-recorder` child
- **CLI** for keybinds (`toggle-region`, `stop`, `status`, …) — never initializes GTK
- **Optional GTK4 + libadwaita** GUI as a view on the same session
- Clipboard gets the **absolute file path** (text), not video bytes

See [SPEC.md](SPEC.md) for the full product/architecture contract.

---

## Install / build

### Runtime package deps (distro)

Hard (required to record):

| Binary | Used for |
|--------|----------|
| `wf-recorder` | Capture / encode |
| `slurp` | Region selection |

Soft (features degrade if missing):

| Binary | Used for |
|--------|----------|
| `notify-send` | Desktop notifications (`libnotify`) |
| `wl-copy` | Copy output path (`wl-clipboard`) |
| `xdg-open` | Open last file / folder from GUI |

Build-time (for GUI path): `gtk4`, `libadwaita`, `pkg-config` / `pkgconf`, and a C toolchain (`base-devel` or equivalent).

### From source

```bash
# Install into ~/.cargo/bin (ensure it is on PATH)
cargo install --path .

# Or build a release binary only
cargo build --release
# → target/release/record-ui
```

Install the launcher entry (optional, for walker / app menus):

```bash
# System-wide
sudo install -Dm644 data/record-ui.desktop /usr/share/applications/record-ui.desktop

# Or user-local
install -Dm644 data/record-ui.desktop \
  "${XDG_DATA_HOME:-$HOME/.local/share}/applications/record-ui.desktop"
```

`Exec=record-ui gui` (same as bare `record-ui`) assumes the binary is on the launcher’s `PATH`. Use an absolute path in the `.desktop` file if needed.

---

## CLI

Default with no subcommand is the same as `gui`.

| Command | Behavior |
|---------|----------|
| `record-ui` / `record-ui gui` | Ensure server; open/raise GUI client |
| `record-ui region [--audio]` | Start region recording (error if busy) |
| `record-ui fullscreen [--audio]` | Start without region geometry (error if busy) |
| `record-ui toggle-region [--audio]` | Idle→region start; SelectingRegion→cancel slurp; Recording→stop; Stopping→idempotent wait/no-op |
| `record-ui stop` | Stop if recording/selecting; no-op success if idle |
| `record-ui status` | Print one JSON status object on stdout |
| `record-ui quit` | Stop if needed; shut down the session server |

`--audio` enables system audio via `wf-recorder -a` for that start. Default audio is **off** (config and CLI).

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success / idle no-op stop / slurp cancel treated as clean abort for toggle |
| 1 | General failure (spawn, I/O, …) |
| 2 | Busy (`AlreadyBusy`) |
| 4 | Hard dependency missing (`wf-recorder` / `slurp`) |

### `status` JSON (high level)

Fields include: `state`, `output_path`, `pid`, `started_at_unix`, `audio`, `last_error`, `last_success_path`, `elapsed_ms`.

### Process model (short)

1. First command that needs a session starts a **server** binding `$XDG_RUNTIME_DIR/record-ui.sock` (mode `0600`) and writing `$XDG_RUNTIME_DIR/record-ui.pid`.
2. Later invocations are **clients** over that socket.
3. Closing the GUI only disconnects the view — **recording continues** until `stop`, toggle-stop, or `quit`.
4. Only this app’s own `wf-recorder` child is managed; external instances are ignored.

Socket path: **`$XDG_RUNTIME_DIR/record-ui.sock`**.

---

## Configuration

Path: **`$XDG_CONFIG_HOME/record-ui/config.toml`**  
(default `$HOME/.config/record-ui/config.toml`). Missing file → built-in defaults.

| Key | Default | Meaning |
|-----|---------|---------|
| `output_dir` | XDG Videos (`xdg-user-dir VIDEOS` if available, else `~/Videos`) | Where files are written |
| `audio_default` | `false` | System audio off unless toggled / CLI `--audio` |
| `copy_path` | `true` | `wl-copy` absolute path on success |
| `notify` | `true` | Desktop notifications |
| `notify_on_start_cli` | `true` | Short “Recording started” when start has no GUI client |
| `stop_timeout_ms` | `5000` | Wait after SIGINT before SIGTERM |
| `stop_term_timeout_ms` | `2000` | Wait after SIGTERM before hard failure |

Filename pattern: **`rec-YYYYMMDD-HHMMSS.mp4`**. Same-second collisions append `-1`, `-2`, …  
`output_dir` is created if missing.

Example:

```toml
# ~/.config/record-ui/config.toml
output_dir = "/home/you/Videos/clips"
audio_default = false
copy_path = true
notify = true
```

---

## Dependencies: hard vs soft

| Binary | Class | Missing behavior |
|--------|-------|------------------|
| `wf-recorder` | Hard | Fail start; CLI exit **4**; clear message |
| `slurp` | Hard for region | Fail region start; exit **4** |
| `notify-send` | Soft | Degrade; warn once |
| `wl-copy` | Soft | Degrade; warn once |
| `xdg-open` | Soft (GUI open actions) | Fail only that action |

If notify/clipboard fail after a good file, recording is still **success** (path shown; warnings only — path was not necessarily copied).

---

## Hyprland integration (docs only)

**record-ui does not edit `~/.config/hypr`.** Add binds yourself.

### Keybinds

```conf
# ~/.config/hypr/hyprland.conf (example)

# Toggle region record / stop (recommended)
bind = SUPER SHIFT, R, exec, record-ui toggle-region

# Optional extras
bind = SUPER SHIFT, S, exec, record-ui stop
bind = SUPER SHIFT, G, exec, record-ui gui
```

With audio on toggle start:

```conf
bind = SUPER SHIFT, R, exec, record-ui toggle-region --audio
```

### PATH from Hyprland `exec`

Hyprland’s environment often **differs** from an interactive shell. If binds report “command not found”:

1. Prefer an **absolute path**:

   ```conf
   bind = SUPER SHIFT, R, exec, /home/YOU/.cargo/bin/record-ui toggle-region
   ```

2. Or ensure `~/.cargo/bin` (or your install prefix) is on the PATH used by Hyprland (e.g. via `env = PATH,...` / login env setup).

### Floating window rules (optional)

```conf
# Keep the small control window out of the way
# Wayland app_id matches Adwaita application_id("dev.recordui.app")
windowrulev2 = float, class:^(dev\.recordui\.app)$
windowrulev2 = pin, class:^(dev\.recordui\.app)$
# Adjust size/position to taste:
# windowrulev2 = size 360 280, class:^(dev\.recordui\.app)$
```

If rules do not match (toolkit/version quirks), check the live class/app_id with `hyprctl clients`.

### Fullscreen multi-monitor note

Fullscreen mode uses `wf-recorder` **without** `-g` (no output picker in v1). Multi-output behavior follows the installed `wf-recorder` defaults. Prefer **region** + `slurp` for multi-monitor clips.

---

## GUI

```bash
record-ui          # or: record-ui gui
```

Shows mode (region/fullscreen), audio toggle, Record/Stop, state label, elapsed timer, last success path, Open file / Open folder.

- Timer tracks server time so attaching mid-recording stays correct.
- Close window ≠ stop recording.

---

## Desktop entry

See [`data/record-ui.desktop`](data/record-ui.desktop). Install under applications as shown in [Install / build](#install--build).

---

## Manual acceptance checklist (SPEC M1–M7)

Run these on a real Hyprland session after install:

| ID | Criterion | Pass? |
|----|-----------|-------|
| **M1** | Region record ≥3s, stop, file opens in a player, duration ≥2s | ☐ |
| **M2** | Multi-monitor region (slurp across / on secondary) | ☐ |
| **M3** | Audio on/off smoke (`--audio` vs default off) | ☐ |
| **M4** | Keybind toggle spam — no double `wf-recorder` | ☐ |
| **M5** | Close GUI while recording; `record-ui stop` still works; re-open GUI shows state or idle after stop | ☐ |
| **M6** | Esc in slurp → no file, no success notify | ☐ |
| **M7** | Missing `wf-recorder` messaging is clear (exit 4) | ☐ |

Suggested smoke:

```bash
record-ui status
record-ui toggle-region    # select region, record a few seconds
record-ui toggle-region    # stop
ls "$(xdg-user-dir VIDEOS 2>/dev/null || echo ~/Videos)"/rec-*.mp4 | tail -1
record-ui quit
```

---

## Development

```bash
cargo test
cargo build --release
cargo run -- --help
```

Unit and IPC tests do **not** require a live Wayland session (mocked ports / temp `XDG_RUNTIME_DIR`).

---

## Out of scope (v1)

Tray icon, video MIME clipboard, replay buffer, streaming, portal capture engine, monitor picker, auto-edit of Hyprland config, Flathub packaging. See [SPEC.md](SPEC.md) for the full list.
