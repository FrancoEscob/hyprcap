# Empaquetado Arch (AUR)

## ¿Pacman oficial o AUR?

| Camino | Qué es | Realismo para un primer OSS |
|--------|--------|-------------------------------|
| **`extra` / `extra-testing` (pacman)** | Repo oficial de Arch | Difícil al inicio: hace falta un *Trusted User* o que un maintainer adopte el paquete. **No es el primer paso.** |
| **AUR** | Community packages, `yay`/`paru` | **Camino correcto** para tu primera app. Un `PKGBUILD`, revisión de la comunidad, la gente instala con un comando. |

Después de que el paquete madure (usuarios, issues, tags de release), podés mirar *Arch Linux Pro Audio* / adoptantes en repos oficiales. Empezá por AUR.

## Qué resuelve el paquete (y el .desktop)

Al instalar desde AUR el PKGBUILD:

1. Compila e instala **`/usr/bin/record-ui`** (en el PATH de todos).
2. Instala **`/usr/share/applications/record-ui.desktop`**.
3. Arch corre `update-desktop-database` al instalar → **walker / menús** ven `record-ui` sin que el usuario toque rutas a mano.

No hace falta “copiar el .desktop a mano” ni `cargo install` si usan AUR.

Runtime que el usuario debe tener (o que `depends` instala):

- hard: `wf-recorder`, `slurp`, `ffmpeg`, `gtk4`, `libadwaita`
- soft (optdepends): `hyprland`/`hyprctl`, `libnotify`, `wl-clipboard`

## Publicar en AUR (checklist de primer maintainer)

1. Cuenta en [aur.archlinux.org](https://aur.archlinux.org) (misma SSH key que usás en GitHub, o una dedicada AUR).
2. Lee: [AUR submission guidelines](https://wiki.archlinux.org/title/AUR_submission_guidelines) y [Creating packages](https://wiki.archlinux.org/title/Creating_packages).
3. Local test:

   ```bash
   cd packaging/aur/record-ui-git
   makepkg -si          # build + install local
   record-ui --help
   record-ui gui
   ```

4. Generá `.SRCINFO` (obligatorio en el git del AUR):

   ```bash
   makepkg --printsrcinfo > .SRCINFO
   ```

5. Subí **solo** el paquete al AUR (repo git aparte, no el repo de la app):

   ```bash
   git clone ssh://aur@aur.archlinux.org/record-ui-git.git
   cp PKGBUILD .SRCINFO /path/to/record-ui-git-aur/
   cd /path/to/record-ui-git-aur
   git add PKGBUILD .SRCINFO
   git commit -m "Initial: record-ui-git 0.1.0"
   git push
   ```

6. Cuando tengas **tags** `v0.1.0`, `v0.2.0`, … conviene un paquete estable `record-ui` (tarball de release) además del `-git`.

## Releases recomendados en GitHub

```bash
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0
```

Luego un PKGBUILD no-git puede usar:

```text
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
```

y un `sha256sums` real (no `SKIP`).

## Si no querés AUR todavía

Instalación “de una” local (sin AUR):

```bash
cargo install --path . --locked
install -Dm644 data/record-ui.desktop \
  ~/.local/share/applications/record-ui.desktop
# binary: ~/.cargo/bin/record-ui  → asegurate que ~/.cargo/bin esté en PATH
```

O con el PKGBUILD de este repo:

```bash
cd packaging/aur/record-ui-git
makepkg -si
```
