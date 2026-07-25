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

Publish (after AUR account + SSH):

```bash
makepkg --printsrcinfo > .SRCINFO
git clone ssh://aur@aur.archlinux.org/hyprcap-git.git
# copy PKGBUILD + .SRCINFO, commit, push
```

When you tag releases (`v0.1.0`, …), add a stable `hyprcap` package (tarball + real `sha256sums`) alongside `-git`.
