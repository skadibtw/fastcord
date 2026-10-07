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
