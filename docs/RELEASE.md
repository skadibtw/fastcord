# Releasing

1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and move `CHANGELOG.md` entries under the new version.
2. Commit, then tag and push:
   ```sh
   git tag v0.2.0 && git push origin v0.2.0
   ```
   Tags containing `-` (e.g. `v0.2.0-alpha.1`) publish as prereleases.
3. `.github/workflows/release.yml` checks that the tag matches the Cargo version, builds four targets, packages them with `scripts/package.sh`, and publishes one GitHub Release with `SHA256SUMS` only after every build succeeds.

Dry run without publishing: `gh workflow run release.yml`; download the `release-dry-run` artifact.

Artifacts are unsigned and unnotarized until signing secrets exist.

Application artwork is committed in `assets/icons/` (sources and regeneration
instructions in its README). Windows builds embed a multi-size ICO with the
Windows SDK resource compiler; set `RC` if the SDK is installed outside the
usual location. macOS archives include `fastcord.app/Contents/Resources/fastcord.icns`.
Linux archives include `share/applications/fastcord.desktop` and hicolor PNG/SVG
icons. For a user-local Linux installation, place the executable in
`~/.local/bin/fastcord` (on PATH), copy `share/applications/fastcord.desktop` into
`~/.local/share/applications/` and merge `share/icons/hicolor/` into
`~/.local/share/icons/hicolor/`. The archive does not modify desktop settings.
