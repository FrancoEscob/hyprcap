# Arch packaging (AUR)

## Users

```bash
yay -S hyprcap-git
# or: paru -S hyprcap-git
```

Installs `/usr/bin/hyprcap`, desktop entry, and icons. Hard deps (`wf-recorder`, `slurp`, `ffmpeg`, GTK) come with the package.

## Maintainer notes

- PKGBUILD: [`hyprcap-git/PKGBUILD`](hyprcap-git/PKGBUILD)
- AUR guidelines: https://wiki.archlinux.org/title/AUR_submission_guidelines

Local smoke:

```bash
cd packaging/aur/hyprcap-git
makepkg -si
hyprcap --help
```

Publish (after AUR account + SSH key registered at https://aur.archlinux.org/account/):

```bash
# 1) After pushing the app repo to GitHub (hyprcap-git tracks git HEAD):
cd packaging/aur/hyprcap-git
makepkg --printsrcinfo > .SRCINFO

# 2) Clone or update the AUR package repo (first time only):
git clone ssh://aur@aur.archlinux.org/hyprcap-git.git /tmp/hyprcap-git-aur
cp PKGBUILD .SRCINFO /tmp/hyprcap-git-aur/
cd /tmp/hyprcap-git-aur
git add PKGBUILD .SRCINFO
git commit -m "hyprcap-git: audio matrix (system/app/mic)"
git push
```

`-git` packages do **not** need a version bump for every app commit: `pkgver()` is computed from the cloned GitHub tree at build time. Update AUR when **PKGBUILD** / deps / install files change.

When you tag releases (`v0.1.0`, …), add a stable `hyprcap` package (tarball + real `sha256sums`) alongside `-git`.

### AUR SSH

If `ssh aur@aur.archlinux.org` prints `Permission denied (publickey)`, add an SSH key in your AUR account and use it for `aur.archlinux.org` (see ArchWiki AUR submission guidelines).
