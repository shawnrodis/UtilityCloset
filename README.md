# The Utility Closet

An all-in-one Windows maintenance app: network drive mapping, System File
Checker, display cache reset, and icon cache rebuild — in one small window,
no installer, no bloat.

Built with [Rust](https://www.rust-lang.org/) and [egui](https://www.egui.rs/).
Single `.exe`, no dependencies, no console window. Requests administrator
rights on launch (every tool needs them).

## Download

Grab the latest `UtilityCloset.exe` from
[Releases](https://github.com/shawnrodis/UtilityCloset/releases) — no install
needed, just run it. (Building from source is optional; instructions below.)

## Features

- **Network Drives** — map/unmap drives with `net use`, output shown live in
  the log. Drive letters prefilled, paths start blank. Add up to 6 drives;
  the list is saved to `UtilityCloset.ini` next to the exe and restored on
  launch.
- **System File Checker** — launches `sfc /scannow` in its own window.
- **Display Cache** — deletes the GPU display-configuration registry keys
  (`GraphicsDrivers\{Configuration, Connectivity, ScaleFactors}`). Restart
  required afterwards.
- **Icon Cache** — clears the icon cache, restarts Explorer, offers a reboot.
- Follows the Windows light/dark app theme. Custom closet icon.

## Build from source

On Windows (install Rust via [rustup](https://rustup.rs/) first):

```powershell
cargo build --release
```

Cross-compile from Linux:

```bash
# mingw-w64 provides the linker and windres
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
# make sure a `windres` binary is on PATH, e.g.:
#   sudo ln -s /usr/bin/x86_64-w64-mingw32-windres /usr/local/bin/windres
cargo build --release --target x86_64-pc-windows-gnu
```

The build script compiles `assets/UtilityCloset.rc` with windres and links the
resulting object directly, embedding the exe icon and the
`requireAdministrator` manifest.

## Project layout

```
UtilityCloset/
├── src/main.rs            # the whole app (egui)
├── build.rs               # embeds icon + admin manifest via windres
├── assets/
│   ├── UtilityCloset.ico        # exe / window icon
│   ├── UtilityCloset.manifest   # requireAdministrator manifest
│   └── icon.png                 # runtime window icon (from the .ico)
└── Cargo.toml
```

## License

GPL-3.0 — see [LICENSE](LICENSE). Copyright (c) 2026 Shawn Rodis.
