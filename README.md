# record-ui

Small **pure Rust** screen recorder for **Hyprland** — a GTK4 / libadwaita UI around [`wf-recorder`](https://github.com/ammen99/wf-recorder).

| Mode | What it does |
|------|----------------|
| **Region** | Draw a rectangle, record that |
| **One monitor** | Full display + FPS picker |
| **2 monitors** | Both screens → **one** video (exactly 2 displays; layout matches Hyprland) |

CLI for keybinds. Files land in Videos; path is copied to the clipboard.

> **Initial release (v0.1).** Solid for daily recording on Hyprland, but early days — more is coming. Feedback and issues welcome.

**Planned / next ideas** (not a promise of order):

- Easier **keybind** setup (docs + helpers; maybe config-driven presets)
- More **quality / codec** options (`wf-recorder` / ffmpeg knobs without leaving the app)
- Polish for **2 monitors** (3+ still out of scope for now)
- Small UX extras as people use it

<p align="center">
  <img src="docs/screenshots/region.png" alt="record-ui — Region mode" width="320" />
  &nbsp;
  <img src="docs/screenshots/one-monitor.png" alt="record-ui — One monitor mode" width="320" />
</p>

---

## Install (Arch)

```bash
yay -S record-ui-git
# or:  paru -S record-ui-git
```

That installs the app **and** hard deps (`wf-recorder`, `slurp`, `ffmpeg`, GTK, …), puts `record-ui` on `PATH`, and registers the menu entry for **walker**.

If `yay` says *No AUR package found*, refresh the AUR cache:

```bash
yay -Sya
yay -S record-ui-git
```

Package page: https://aur.archlinux.org/packages/record-ui-git

Then:

```bash
record-ui          # GUI
# or search “record-ui” in walker
```

Optional keybind:

```conf
bind = SUPER SHIFT, A, exec, record-ui toggle-region
```

---

## Use

| Want | Do |
|------|-----|
| GUI | `record-ui` → mode → **Record** / **Stop** |
| Quick region | `record-ui toggle-region` (again to stop) |
| One monitor (CLI) | `record-ui fullscreen --output NAME --fps 60` |
| Both screens | GUI **2 monitors**, or `record-ui both` |
| List outputs | `record-ui list-outputs` |
| Quit session | `record-ui quit` |

Closing the window **does not** stop a recording — use **Stop**.

**2 monitors** needs exactly two displays (detected live via `hyprctl`, not hardcoded). 3+ not supported yet.

---

## Config (optional)

`~/.config/record-ui/config.toml` — also written when you change monitor/FPS in the GUI.

```toml
# output_dir = "/home/you/Videos"
# fullscreen_output = "DP-1"
# one_fps = 144
```

---

## Build from source

```bash
cargo install --path . --locked
# Rust toolchain + pkgconf + gtk4 + libadwaita; runtime: wf-recorder, slurp, ffmpeg
```

---

## License

MIT — [LICENSE](LICENSE).

Internals (optional): [SPEC.md](SPEC.md) · [docs/DUAL-MONITOR.md](docs/DUAL-MONITOR.md) · [packaging/aur/](packaging/aur/)
