# datagrep — Flatpak (GTK4 UI, issue #36)

Packages the GTK4/libadwaita frontend on `org.gnome.Platform//50`
(libadwaita 1.9, GTK 4.20). This exists because the recommended window
composition needs libadwaita ≥ 1.7 and the Ubuntu 22.04 packaging floor ships
1.1 — the Flatpak runtime lifts that floor and makes gsettings schemas a
two-line install instead of the AppImage contortion. The Qt UI's
AppImage/.deb/.rpm route (`packaging/README.md`) is untouched.

The manifest builds two cargo workspaces: the root one for `datagrep-cli`,
installed as `/app/bin/datagrep-cli`, and `ui/gtk4` — its own workspace, which
links `datagrep-ffi` and so compiles the full driver graph — installed as
`/app/bin/datagrep`, which is what `command:` and the `.desktop` `Exec` both
name. `flatpak run io.github.chud_lori.datagrep` opens the GTK4 window;
`flatpak run --command=datagrep-cli io.github.chud_lori.datagrep` gets the CLI.

`generate-cargo-sources.sh` runs the generator over BOTH lockfiles and merges
the results, de-duplicated: the two workspaces share most of their graph, and
one crate unpacked twice into the same vendor directory is not something to
find out about inside a network-less sandbox.

## CI

`.github/workflows/linux-flatpak.yml` builds on every relevant change and uploads
`datagrep-gtk4.flatpak` as an artifact:

```sh
flatpak install ./datagrep-gtk4.flatpak
flatpak run io.github.chud_lori.datagrep
```

## Local build (Linux)

```sh
flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
pip install aiohttp tomlkit
packaging/flatpak/generate-cargo-sources.sh
flatpak-builder --user --install-deps-from=flathub --force-clean \
  build-dir packaging/flatpak/io.github.chud_lori.datagrep.yml
flatpak-builder --run build-dir packaging/flatpak/io.github.chud_lori.datagrep.yml datagrep
```

`cargo --offline` inside the sandbox resolves against the crates that
flatpak-builder pre-downloaded from `cargo-sources.json` — regenerate it after
any `Cargo.lock` change.
