# record-ui — Multi-monitor capture SPEC

**Status:** `shipped` (P0 One pickers + P1 Both dual session / GUI Both; product decisions closed 2026-07-25)  
**Parent:** `SPEC.md` v1 (daemon-on-demand, exclusive session, cooperative stop)  
**Date decisions closed:** 2026-07-25  
**Engine:** `wf-recorder` 0.6.x + `ffmpeg` (Both only)

This document is the **normative product + architecture slice** for:

1. **UI monitor picker** (One monitor mode) — **shipped**
2. **UI FPS picker** (One monitor mode) — **shipped**
3. **Both monitors** — one final file, layout-true canvas, compose **after** stop — **shipped** (CLI `both` + GUI Both mode)

It **replaces** the earlier “Feature 1 only / Both deferred forever” framing from the automated workflow. Where runtime code and this doc disagree on product intent, **this doc wins** as the contract.

---

## 1. Problem

On multi-monitor Hyprland:

- Bare `wf-recorder` without `-o`/`-g` prompts for an output on stdin → fails under the session daemon (`Failed to select output`).
- One `wf-recorder` process binds **one** `wl_output`. There is **no** multi-output canvas flag.
- Region geometry (`-g`) must lie **entirely inside one head**.
- Users want OBS-like **both monitors in one video**, plus an honest **pick which monitor** UI for single-head capture.

record-ui remains a **thin wrapper**, not a scene compositor. Both = dual capture + post-stop stitch, not a live OBS canvas.

---

## 2. Goals

| ID | Goal |
|----|------|
| G1 | GUI: choose **which monitor** for one-monitor capture. |
| G2 | GUI: choose **FPS** for one-monitor capture (including native 144 on the main head, 30, 60, Auto). |
| G3 | GUI mode **Both**: exactly two heads → **one** playable file after stop. |
| G4 | Both uses **layout-true** composite (Hyprland coordinates + black voids). **No** height-matching scale/hstack. |
| G5 | Both compose runs **after** cooperative stop (blocking stop until final file or failure). |
| G6 | Region + `toggle-region` keybind remain the primary “quick clip” path. |
| G7 | One / Both human path = **open the GUI**; no product requirement for `toggle-both` keybind. |
| G8 | Persist last One monitor + FPS to config when chosen in UI. |
| G9 | System audio stays one checkbox; never double desktop audio on Both. |

## 3. Non-goals

- Live/FIFO remux while recording.
- Re-scaled “same height” hstack as default or marketed layout.
- Both with 1 or 3+ monitors (no silent first-two / multi-select in this slice).
- Per-monitor audio devices / mic matrix.
- OBS scenes, tray, portal capture engine.
- Auto-edit Hyprland config.
- FPS picker on Region (defaults only).
- Async “Composing…” IPC state machine (stop remains blocking).

---

## 4. Modes (product)

| Mode | Label (GUI) | Capture | User chooses |
|------|-------------|---------|--------------|
| **Region** | Region | `slurp` → one `wf-recorder -g` | Area only |
| **One monitor** | One monitor | one `wf-recorder -o NAME` optional `-r FPS` | Monitor list + FPS list |
| **Both** | Both | two `wf-recorder -o` @ 60 fps → temps → `ffmpeg` layout-true → final | Nothing (always both heads when enabled) |

**Session:** still **exclusive** (Busy if not Idle). One user-visible session. Both owns **two** recorder process groups (+ optional ffmpeg during stop).

---

## 5. UI (normative)

### 5.1 Chrome

Small Adwaita window:

1. Mode row: **Region | One monitor | Both** (toggle group, exclusive).
2. **Monitor list** (combo or radio list): name + resolution (e.g. `HDMI-A-1 · 2560×1440`).  
   - **Sensitive only when mode = One monitor.**  
   - Hidden or insensitive for Region / Both.
3. **FPS list** (combo): values depend on selected monitor (see §6).  
   - **Sensitive only when mode = One monitor.**
4. **System audio** checkbox (all modes).
5. Primary **Record / Stop**, state, timer, last path, open file/folder.
6. Message line: show resolved `Output: NAME` (One), or `Both: A + B` (Both), errors, Stopping.

### 5.2 Both enablement

- Inventory length **== 2** and `ffmpeg` on `PATH` → Both **enabled**.
- Otherwise Both **disabled** with clear tooltip/message:
  - ≠2 monitors: “Both requires exactly two monitors.”
  - missing ffmpeg: “Both requires ffmpeg.”

### 5.3 Persistence (One)

On user change of monitor or FPS in One (or on successful start that used those values — **prefer on change** for snappy remember):

Write config:

```toml
fullscreen_output = "HDMI-A-1"   # Wayland output name
one_fps = 144                    # see three-way meaning below
# one_fps = 0                    # sticky Auto (GUI remember)
# # omit one_fps entirely        # CLI Auto; GUI first-run → native
```

**`one_fps` three-way meaning:**

| Value | CLI / resolve (`resolve_one_fps`) | GUI picker on load |
|-------|-----------------------------------|--------------------|
| absent / `None` | Auto (omit `-r`) | **native** (first-run default) |
| `0` | Auto (omit `-r`) | **Auto** (sticky remember) |
| `n > 0` | `-r n` | select rate `n` (include in list if non-standard) |

GUI writes `one_fps = 0` when the user picks Auto so reopen does not re-default to native.

**First launch defaults (no config):**

- Monitor = inventory entry with **largest area** (width×height); tie-break stable sort by name.
- FPS = **native** refresh of that monitor (integer Hz from inventory), not Auto.

### 5.4 Keybinds / human flow

| Intent | Path |
|--------|------|
| Quick region clip | Hyprland bind → `record-ui toggle-region` (**no GUI required**) |
| One monitor / Both / pick FPS | Open GUI (`record-ui` / `record-ui gui`) |

No product requirement for `toggle-both` / `toggle-one` keybinds.

### 5.5 Audio copy

- Checkbox label: **System audio**.
- Both + checked: single desktop audio track (`-a` only on **primary** child). Tooltip OK: “One desktop audio track (not per monitor).”

---

## 6. One monitor — resolve + FPS

### 6.1 Output resolve (start)

Priority:

1. GUI/IPC/CLI explicit `output` for this start.
2. Else config `fullscreen_output` if set and still in inventory.
3. Else if inventory length == 1 → that name.
4. Else if inventory ≥ 2 → **Err** listing known names (GUI should have forced a selection; CLI must pass `--output` or config).
5. Empty inventory → **Err**.
6. Explicit name not in inventory → **Err** (no silent fallback).

**Do not** use “focused output after clicking Record” as the multi-head resolver (focus follows the floating window).

### 6.2 FPS (One only)

Argv: if FPS is Auto → omit `-r`. Else `wf-recorder -r <N>`.

**GUI list for selected monitor** (minimum):

| Entry | Meaning |
|-------|---------|
| Auto | No `-r` (encoder/wf-recorder default) |
| Native | Integer refresh of that output (e.g. 144 or 60) |
| 60 | Always offer if not duplicate of native |
| 30 | Always offer |

Order suggestion: Auto, Native (label `144 (native)`), 60, 30 — dedupe equal values.

CLI: `record-ui fullscreen [--output NAME] [--fps N] [--audio]`  
`--fps` omitted → use config `one_fps` if set (`0` still Auto), else Auto.

Config `one_fps` (same three-way as §5.3):

- **absent / null:** CLI Auto; GUI first-run selects **native**.
- **`0`:** sticky **Auto** for GUI reopen; resolve treats as Auto (no `-r`).
- **`n > 0`:** fixed rate for CLI and GUI.

GUI session start always sends explicit IPC `fps` (`0` for Auto, else `n`) so config cannot override that start.

### 6.3 Argv

```
wf-recorder -o <NAME> [-r <FPS>] [-a] -f <path>
```

No `-g`. Never shell. Always `-o` after successful resolve.

---

## 7. Both — capture + compose

### 7.1 Preconditions (start)

- Inventory length **exactly 2** else `invalid` (list names).
- `ffmpeg` present else `dep_missing`.
- `wf-recorder` present else `dep_missing`.

### 7.2 Layout inventory

Prefer `hyprctl monitors -j`: for each head `{ name, x, y, width, height, refresh }`.  
Fallback names-only from `wf-recorder -L` **cannot** do layout-true offsets → Both start **fails** with message to use Hyprland / hyprctl (or document degraded path only if we explicitly add it later; **this SPEC requires positions** for Both).

Sort primary = minimum `(x, y)` lexicographic (left/top-most).

### 7.3 Dual capture argv

Each child:

```
wf-recorder -o <NAME> -r 60 -D -f <temp.mkv>
```

- **FPS:** **60 on both** (product decision; not 30). Independent of One’s FPS picker.
- **`-D` / `--no-damage`:** required for stitch stability.
- **`-a`:** only on **primary**, and only if session audio is on.
- Temps: under `output_dir` or XDG runtime, unique names, e.g. `.record-ui-both-<id>-A.mkv` / `-B.mkv`.

Spawn both before entering **Recording**. If second spawn fails → reap first → Idle + error (no Recording).

Early settle (~200ms): both still alive or fail start.

### 7.4 Stop (blocking — product choice A)

1. UI/CLI stop → state **Stopping**.
2. SIGINT **both** process groups (parallel signal), then joint wait with shared timeouts; escalate SIGTERM both; nuclear reap as needed (same spirit as single-child contract, applied to both).
3. If either final temp missing/empty → failure; no stitch; set `last_error`; Idle.
4. Run **ffmpeg** layout-true composite → final `rec-YYYYMMDD-HHMMSS.mp4` in `output_dir`.
5. Success only if final file size > 0 and ffmpeg exit 0:
   - Delete temps.
   - Success hooks (notify + path clipboard) on **final** path only.
6. Stop RPC **does not return** until step 5/failure completes. GUI may show `Stopping…` (optional subcopy “Composing…”).

**No** mid-stitch subscribe progress in this slice.

### 7.5 Layout-true ffmpeg (normative intent)

Canvas = axis-aligned bounding box of both heads in compositor coordinates.

Example (user machine):

- HDMI-A-1: `(0,0)` 2560×1440  
- DP-1: `(2560,180)` 1920×1080  
- Canvas ≈ 4480×1440; DP at offset `(2560, 180)`; voids = black.

Implementation sketch (informative, not exclusive):

- Scale/pad each stream to its `width×height` if needed.
- `xstack` or `overlay` at `(x,y)` from hyprctl.
- Map audio from primary only.
- Duration skew: `-shortest` or pad shorter video — pick one policy in implementation notes and test it; prefer **pad shorter with black** if easy, else `-shortest`.

**Forbidden as Both default:** scale both to same height + `hstack` (user rejected).

### 7.6 Failure modes

| Event | Behavior |
|-------|----------|
| One child dies while Recording | Force-reap peer; **no** stitch; **no** success hooks; `last_error`; Idle |
| Stitch / ffmpeg fails | `last_error` includes temp paths; **retain temps** for debug; no clipboard success claim |
| Success | Delete temps |
| Shutdown / Drop mid-Both | Reap both recorders; kill ffmpeg if running; **skip stitch**; prefer retain temps on unclean path |
| Start with audio while Both | Primary only `-a` |

### 7.7 Status fields (additive)

When recording Both (and after resolve One):

- `capture_mode`: `"region" | "one" | "both"`
- `capture_output`: e.g. `"HDMI-A-1"` or `"HDMI-A-1+DP-1"`
- `pid`: primary recorder pid (document: not sole OS child when Both)
- Optional later: `pids: [a,b]` — not required for first ship if message is clear

---

## 8. CLI / IPC

### 8.1 CLI

| Command | Behavior |
|---------|----------|
| `record-ui list-outputs` | Print inventory (names; prefer `name\tx\ty\tw\th\trefresh` or stable JSON — implementation pick; scripts need names). No daemon required. |
| `record-ui fullscreen [--audio] [--output NAME] [--fps N]` | One monitor start |
| `record-ui both [--audio]` | Both start (exactly 2 heads) |
| `record-ui toggle-region [--audio]` | Unchanged |
| `record-ui gui` / default | Open UI |
| `record-ui stop` / `status` / `quit` | Unchanged semantics |

### 8.2 IPC

- `start_fullscreen`: optional `output`, optional `fps` (number or null), `audio`.
- `start_both`: optional `audio` only (layout from live inventory).
- `status`: include `capture_mode`, `capture_output` when known.
- Existing busy / machine codes; use `invalid` for resolve/≠2; `dep_missing` for ffmpeg/wf-recorder.

### 8.3 Config keys

```toml
output_dir = "..."
audio_default = false
fullscreen_output = "HDMI-A-1"   # One monitor pin
one_fps = 144                    # n > 0 fixed rate; 0 = sticky Auto (GUI); omit = CLI Auto / GUI native first-run
# both_fps fixed at 60 in this SPEC — no config key required
```

---

## 9. Dependencies

| Binary | Region | One | Both |
|--------|--------|-----|------|
| `wf-recorder` | hard | hard | hard |
| `slurp` | hard | — | — |
| `ffmpeg` | — | — | **hard** |
| `hyprctl` | soft (inventory) | soft | **hard for layout-true positions** |
| `notify-send` / `wl-copy` / `xdg-open` | soft as parent SPEC |

---

## 10. Architecture

```
GUI ──► IPC ──► server ──► Recorder
                              ├─ Region { slurp → one child }
                              ├─ One { output, fps? → one child }
                              └─ Both { child_a, child_b, temps, layout }
                                        └─ stop: reap both → ffmpeg → final
```

- Argv arrays only; each recorder in **new process group**.
- Bounded stderr tails on failures.
- Never orphan managed children.
- GUI close = view disconnect; does **not** stop capture.

State machine: keep `Idle | SelectingRegion | Starting | Recording | Stopping`.  
Both uses Recording with dual ownership; Stopping covers dual reap + stitch (still one Stopping).

---

## 11. Test plan (acceptance)

### One + UI picker

1. Two heads, open GUI → One → list shows both → select each → argv `-o` matches; config updates.
2. FPS native 144 on HDMI, 30, Auto (no `-r`) reflected in argv.
3. Multi-head CLI without `--output`/config → fail with names; no child.
4. Invalid `--output` → fail; no child.

### Both

5. `both` / GUI Both → two children @ `-r 60` and `-D`; audio once on primary when enabled.
6. Stop → one playable final file; layout places secondary at compositor offset (unit test on filter graph or fixture coords); blacks in voids.
7. ≠2 monitors → cannot start; GUI Both disabled.
8. No ffmpeg → `dep_missing`.
9. Kill one child mid-record → peer reaped; no final success.
10. Force ffmpeg fail → temps retained; no success clipboard.
11. Busy: second start exit 2 while Both recording.
12. Region toggle keybind still works without GUI.

### Regression

13. Cooperative stop single-child still SIGINT group first.
14. GUI disconnect mid-record keeps session.

---

## 12. Implementation phases (same SPEC, ship order)

| Phase | Deliverable | User-visible | Status |
|-------|-------------|--------------|--------|
| **P0** | Inventory API stable; One: GUI monitor + FPS lists; persist config; IPC/CLI `--output`/`--fps`; hard `-o` | Can pick monitor + 144/30 in UI | **Shipped** |
| **P1** | Both: dual session, stop+stitch layout-true, GUI mode, CLI `both` | Can record both monitors into one file | **Shipped** |

### Live gaps (post-P1)

Not open product work for this SPEC — deferred / out of scope by design:

- Live/FIFO remux or async “Composing…” IPC state machine (stop remains blocking; GUI shows `Stopping…` / `Stopping… Composing…`)
- Both with 1 or 3+ monitors / multi-select
- Same-height scale + hstack as Both default
- `toggle-both` / `toggle-one` keybinds (One/Both = open GUI)
- Per-monitor audio devices

---

## 13. Decision log (grill)

| # | Decision |
|---|----------|
| 1 | Product package **B**: picker + Both in one SPEC |
| 2 | Compose **after** stop |
| 3 | Layout **true** (Hyprland coords + black); no same-height scale |
| 4 | UI **A**: Region \| One \| Both; list only in One |
| 5 | Stop Both **blocking** until final file |
| 6 | Both FPS **60** both heads; One has FPS UI (native/144, 30, 60, Auto) |
| 7 | Persist monitor + FPS (**C light**) |
| 8 | Both only if **exactly 2** monitors |
| 9 | Keybind region; One/Both via **GUI** |
| 10 | One system audio checkbox; Both primary-only `-a` |
| 11 | Peer death no stitch; stitch fail **keep temps**; success delete temps |
| 12 | UI **auto-writes** config on monitor/FPS change |

---

## 14. Relation to parent SPEC.md

Parent still owns: daemon-on-demand, socket IPC framing, cooperative stop timeouts, success = non-empty file + cooperative stop, soft deps notify/clipboard, no GTK on CLI.

This file owns: multi-monitor modes, UI pickers, Both pipeline, related CLI/IPC/config keys.

Parent Out of scope table should list GUI pickers and Both layout-true stitch as **in scope / shipped** (not deferred).
