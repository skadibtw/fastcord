# Releasing

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and move `CHANGELOG.md` entries under the new version.
2. Commit and push to `main`, wait for CI to pass on that commit, then tag and push:
   ```sh
   git tag v0.2.0 && git push origin v0.2.0
   ```
   Tags containing `-` (e.g. `v0.2.0-alpha.1`) publish as prereleases.
3. `.github/workflows/release.yml` refuses a tag whose commit has no successful `CI` run, checks that the tag matches the Cargo version, builds four targets, packages them with `scripts/package.sh`, and publishes one GitHub Release with `SHA256SUMS` only after every build succeeds.

Assets: `fastcord-vX-x86_64-pc-windows-msvc.exe` (the standalone Windows executable), a `.zip` of it with the licences and notices, `.tar.gz` archives for Linux and both macOS architectures, the source archive and `SHA256SUMS`. The Windows executable links libopus, OpenH264, dav1d and the C runtime statically (`+crt-static` in `.cargo/config.toml`), so it needs only system DLLs and no Visual C++ Redistributable. CI's `RUSTFLAGS=-D warnings` replaces that flag in the check jobs; the release workflow does not set `RUSTFLAGS`. Its third-party notices are in the zip and in `docs/`.

Dry run without publishing: `gh workflow run release.yml`; download the `release-dry-run` artifact.

Artifacts are unsigned and unnotarized until signing secrets exist.

Windows x64 archives also include `windivert/` for the optional packet helper:
the pinned upstream DLL and signed driver, full license terms, matching source
archive, provenance and rebuild instructions. The executable also embeds the
same DLL and driver: without an adjacent `windivert/` folder (the standalone
`.exe`) it writes them to `%LOCALAPPDATA%\fastcord\windivert-2.2.2-A`
and loads them only after the SHA-256 check. The client remains unprivileged; only an explicitly started helper
requests UAC. Other release platforms do not bundle or load WinDivert.

The adjacent `zapret2/` directory contains the MIT-licensed UDP profile data,
complete upstream notice and pinned provenance. The 1200-byte packet is
embedded in the helper; standalone winws2 and Cygwin/Lua binaries are not
included. Keep the license/provenance directory in distributed Windows archives.

Application artwork is committed in `assets/icons/` (sources and regeneration
instructions in its README). Windows builds embed a multi-size ICO with the
Windows SDK resource compiler; set `RC` if the SDK is installed outside the
usual location. macOS archives include `fastcord.app/Contents/Resources/fastcord.icns`.
Linux archives include `share/applications/fastcord.desktop` and hicolor PNG/SVG
icons. For a user-local Linux installation, place the executable in
`~/.local/bin/fastcord` (on PATH), copy `share/applications/fastcord.desktop` into
`~/.local/share/applications/` and merge `share/icons/hicolor/` into
`~/.local/share/icons/hicolor/`. The archive does not modify desktop settings.

The Linux executable links the system PipeWire client library
(`libpipewire-0.3.so.0`, present on current GNOME and
KDE Plasma distributions). Screen sharing additionally needs a running PipeWire
daemon and xdg-desktop-portal with a ScreenCast backend for the desktop
(`xdg-desktop-portal-gnome`, `-kde` or `-wlr`); without them the share picker
explains what is missing and everything else works. The build host needs
`libpipewire-0.3-dev` and `clang` (bindgen).
