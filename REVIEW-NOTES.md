# Adversarial review notes (SPEC v0 → v1)

Two read-only subagents reviewed `SPEC.md` draft v0 in parallel.

| Agent | Focus | Verdict |
|-------|--------|---------|
| Product / UX / Hyprland | Stories, close policy, toggle races, success criteria | **revise** |
| Architecture / process / IPC | Daemon ownership, SIGINT groups, GTK on CLI, test seam | **revise** |

## Critical themes (both agreed)

1. Single-instance was three policies at once → v1: **daemon-on-demand**.
2. Window close while recording underspecified → v1: **GUI disconnect only**.
3. Success = exit 0 OR non-empty file was contradictory → v1: **cooperative stop + size > 0**.
4. Toggle during slurp not modeled → v1: state `SelectingRegion` + cancel rules.
5. “Headless” misused → v1: **no GTK window**, still needs Wayland for capture.
6. IPC was “pick later” → v1: socket path, JSON lines, codes, exit table.
7. Stop hang → v1: SIGINT → timeout → SIGTERM escalation.
8. Test seam too thin → v1: mandated U* / I* matrix.

Full agent reports lived in the parent session; normative decisions are only those merged into **SPEC v1**.
