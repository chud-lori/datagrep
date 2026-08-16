# datagrep — Linux packaging

Packages the **already-built** Linux Qt UI (`ui/linux/build/datagrep`) into
three formats:

| Artifact  | Tool                              | Script                       |
|-----------|-----------------------------------|------------------------------|
| AppImage  | linuxdeploy + linuxdeploy-plugin-qt | `packaging/build-appimage.sh` |
| .deb      | fpm (`-s dir -t deb`)             | `packaging/build-packages.sh` |
| .rpm      | fpm (`-s dir -t rpm`)             | `packaging/build-packages.sh` |

Nothing here touches the engine or the build: `ui/linux/CMakeLists.txt` has no
`install()`/CPack wiring on purpose, so both scripts consume the binary the
existing CMake build produces and stage it themselves (fpm's directory source
is exactly this use case). CI: `.github/workflows/linux-package.yml` runs the
same two scripts on release tags (`v*`) and `workflow_dispatch`, and uploads
`dist/` as workflow artifacts.

The version stamped on every artifact is the workspace version in the root
`Cargo.toml` (override with `VERSION=x.y.z`).

## Build the app first (all formats)

```sh
cargo build -p datagrep-ffi --release
cmake -S ui/linux -B ui/linux/build -DCMAKE_BUILD_TYPE=Release -DDATAGREP_BUILD_RUST=OFF
cmake --build ui/linux/build
```

Build-time deps are listed in `ui/linux/README.md` (Qt6, CMake ≥ 3.19,
`libdbus-1-dev`, zlib, a Rust toolchain).

## AppImage (distro-agnostic, Qt bundled)

```sh
packaging/build-appimage.sh
# -> dist/datagrep-<version>-x86_64.AppImage
```

The script downloads `linuxdeploy` and `linuxdeploy-plugin-qt` (continuous
releases) into `dist/.appimage-work/tools/` on first run, stages an AppDir from the
binary + `packaging/datagrep.desktop` + `packaging/icons/datagrep.png`, and
runs:

```sh
linuxdeploy --appdir AppDir --executable <binary> \
  --desktop-file packaging/datagrep.desktop --icon-file packaging/icons/datagrep.png \
  --plugin qt --output appimage
```

which is the workflow the AppImage packaging guide recommends for Qt apps: the
qt plugin interrogates `qmake` (we point `$QMAKE` at `qmake6`, since bare
`qmake` on Debian/Ubuntu is Qt5 or absent), then bundles the Qt runtime,
platform plugins (`libqxcb.so`), and the app's other shared-library deps into
the AppDir before linuxdeploy emits the single-file AppImage.
`APPIMAGE_EXTRACT_AND_RUN=1` is set so the tooling runs without FUSE
(containers, CI runners).

Not everything is bundled: linuxdeploy keeps base-system libraries (glibc,
libGL, …) off the AppImage per the community excludelist, and host *services*
can never be bundled — see the Secret Service note below.

## .deb and .rpm (system Qt)

```sh
gem install fpm          # plus `apt install rpm` for rpmbuild on Debian/Ubuntu
packaging/build-packages.sh
# -> dist/datagrep_<version>_amd64.deb
# -> dist/datagrep-<version>-1.x86_64.rpm
```

The script stages an FHS tree —

```
usr/bin/datagrep                                    (stripped copy)
usr/share/applications/datagrep.desktop
usr/share/icons/hicolor/256x256/apps/datagrep.png
usr/share/icons/hicolor/scalable/apps/datagrep.svg
```

— and runs fpm twice over it (`-s dir -t deb`, `-s dir -t rpm`) with per-target
runtime dependencies. Qt is **not** bundled in these packages; it comes from
the distro.

### Runtime dependencies

| Need                        | Debian/Ubuntu (`Depends`)                          | Fedora/RHEL (`Requires`) |
|-----------------------------|----------------------------------------------------|--------------------------|
| Qt6 Core/Gui/Widgets        | `libqt6core6`, `libqt6gui6`, `libqt6widgets6`      | `qt6-qtbase-gui`         |
| D-Bus client lib (keyring)  | `libdbus-1-3`                                      | `dbus-libs`              |
| zlib (flate2 in the Rust lib) | `zlib1g`                                         | `zlib`                   |
| Secret Service provider     | `gnome-keyring \| kwalletd6` (**Recommends**)      | — (see note)             |

On Ubuntu 24.04 the time_t64-renamed Qt packages (`libqt6core6t64`, …)
`Provide` the unversioned names above, so the .deb installs on both pre- and
post-t64 releases. glibc/libstdc++ are omitted as essential-on-any-desktop.
The AppImage needs none of the Qt/zlib rows (bundled) — only the host D-Bus
session and a Secret Service provider.

### Secret Service (keyring) note

`datagrep-secrets` stores connection passwords via the `keyring` crate's
**Secret Service** backend — D-Bus IPC to whatever implements
`org.freedesktop.secrets` on the session bus (GNOME Keyring, KWallet,
KeePassXC). That provider is a **host service**: no package format, AppImage
included, can bundle it. Without one the app runs, but saving/resolving
stored passwords fails. Hence the .deb *Recommends* (not Depends)
`gnome-keyring | kwalletd6`; on Fedora, GNOME/KDE spins ship a provider by
default so the .rpm declares nothing.

## CI notes

- The workflow triggers on `v*` tags and `workflow_dispatch` only; per-push
  compile verification stays in `linux-qt.yml`.
- fpm is installed with `gem install fpm` (needs `ruby ruby-dev`); the `rpm`
  apt package supplies the `rpmbuild` backend fpm's rpm target shells out to.
- The job validates `datagrep.desktop` with `desktop-file-validate` and
  inspects the finished packages (`dpkg-deb --info/--contents`,
  `rpm -qpi/--requires`) so malformed metadata fails the build.

## Sources

- AppImage packaging guide, "Native binaries" (linuxdeploy workflow for Qt
  apps): <https://docs.appimage.org/packaging-guide/from-source/native-binaries.html>
- linuxdeploy-plugin-qt (invocation, `$QMAKE`, plugin/platform bundling):
  <https://github.com/linuxdeploy/linuxdeploy-plugin-qt>
- linuxdeploy: <https://github.com/linuxdeploy/linuxdeploy>
- fpm, "Getting started" (`-s dir`, `-t deb|rpm`, `--depends`, metadata flags):
  <https://fpm.readthedocs.io/en/latest/getting-started.html>
- AppImage community excludelist (base-system libraries never bundled):
  <https://github.com/AppImageCommunity/pkg2appimage/blob/master/excludelist>
- freedesktop.org Secret Service specification:
  <https://specifications.freedesktop.org/secret-service-spec/latest/>
