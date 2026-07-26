# Arch packaging (AUR)

## Name note

| AUR name | Status |
|----------|--------|
| **`record-ui-git`** | **Ours** (maintainer `francoescob`). Tracks [FrancoEscob/hyprcap](https://github.com/FrancoEscob/hyprcap), installs `/usr/bin/hyprcap`. |
| `hyprcap-git` / `hyprcap` | **Taken** by another project (different repo). Do not overwrite. |

## Users

```bash
yay -S record-ui-git
# or: paru -S record-ui-git
```

Installs `/usr/bin/hyprcap` (+ transitional `record-ui` symlink), desktop entry, and icons.

## Maintainer notes

- Active package: [`record-ui-git/PKGBUILD`](record-ui-git/PKGBUILD)
- Template only (name conflict): [`hyprcap-git/`](hyprcap-git/) — keep for reference; not publishable under that name
- AUR guidelines: https://wiki.archlinux.org/title/AUR_submission_guidelines

Local smoke from this tree:

```bash
cd packaging/aur/record-ui-git
makepkg -si
hyprcap --help
```

### Publish / update AUR (`record-ui-git`)

Requires an SSH key registered on https://aur.archlinux.org/account/ (test: `ssh aur@aur.archlinux.org` should print a welcome, not `Permission denied`).

```bash
# After pushing the app to GitHub:
cd packaging/aur/record-ui-git
makepkg --printsrcinfo > .SRCINFO

git clone ssh://aur@aur.archlinux.org/record-ui-git.git /tmp/record-ui-git-aur
cp PKGBUILD .SRCINFO /tmp/record-ui-git-aur/
cd /tmp/record-ui-git-aur
git add PKGBUILD .SRCINFO
git commit -m "record-ui-git: Hyprcap rebrand + audio matrix (system/app/mic)"
git push
```

`-git` packages compute `pkgver` from GitHub at build time. Update AUR when **PKGBUILD** / deps / install files change (not for every app commit).

When you tag releases (`v0.1.0`, …), consider a stable package with a **free** name (not `hyprcap` — taken) or keep only `-git`.
