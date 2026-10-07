# Changelog

## Unreleased

## 0.1.0-alpha.1 — 2026-10-07

Release-pipeline test build: an empty window, no Discord functionality yet.

### Added
- Cargo workspace (`fastcord-model`, `fastcord-app`) with an empty iced window.
- `Snowflake` ID type.
- CI: rustfmt, clippy, and tests on Windows, Linux, and macOS.
- Tag-triggered release workflow publishing Windows, Linux, and macOS (arm64, x86_64) builds with `SHA256SUMS`; manual dry run via `workflow_dispatch`.
