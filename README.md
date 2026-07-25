# record-ui

Small screen recorder for **Hyprland** (wrapper around [`wf-recorder`](https://github.com/ammen99/wf-recorder)).

- **Region** — pick a rectangle  
- **One monitor** — full head + FPS  
- **2 monitors** — both screens into **one** video (exactly 2 displays; layout matches Hyprland)

GTK4 GUI + CLI for keybinds. Files go to your Videos folder; the path is copied to the clipboard.

---

## Install (Arch)

```bash
yay -S record-ui-git
# or:  paru -S record-ui-git
```

That pulls **dependencies** (`wf-recorder`, `slurp`, `ffmpeg`, GTK, …), installs `record-ui`, and registers the app menu entry.

Then:

```bash
record-ui          # open the control window
# or search "record-ui" in walker
```

Optional keybind (Hyprland), adjust the combo if it clashes:

```conf
bind = SUPER SHIFT, A, exec, record-ui toggle-region
```

---

## Use

| Want | Do |
|------|-----|
| GUI | `record-ui` → pick mode → **Record** / **Stop** |
| Quick region (keybind) | `record-ui toggle-region` again to stop |
| One monitor from CLI | `record-ui fullscreen --output NAME --fps 60` |
| Both screens (2 only) | GUI **2 monitors**, or `record-ui both` |
| List monitors | `record-ui list-outputs` |
| Quit background session | `record-ui quit` |

Closing the window **does not** stop a recording — use **Stop** or `record-ui stop`.

**2 monitors** needs exactly two displays + `ffmpeg`. More or fewer → mode stays off (by design for now). Monitors are detected live (not hardcoded).

---

## Config (optional)

`~/.config/record-ui/config.toml` — created when you change monitor/FPS in the GUI.

```toml
# output_dir = "/home/you/Videos"
# fullscreen_output = "DP-1"
# one_fps = 144
# audio_default = false
```

---

## Build from source

```bash
cargo install --path . --locked
# needs: rust, pkgconf, gtk4, libadwaita + runtime deps above
```

Arch packaging notes for maintainers: [packaging/aur/README.md](packaging/aur/README.md).

---

## License

MIT — see [LICENSE](LICENSE).

Design / internals (optional reading): [SPEC.md](SPEC.md), [docs/DUAL-MONITOR.md](docs/DUAL-MONITOR.md).
