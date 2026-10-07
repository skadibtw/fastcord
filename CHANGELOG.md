# Changelog

## Unreleased

### Added
- `fastcord-model`: typed Discord entities (`User`, `Role`, `Channel` with forward-compatible `ChannelKind`, `Message`, attachments, reactions, replies), serde wire decoding (decimal-string IDs/permissions), partial `MessageUpdate` merge that never clears omitted fields, and Discord's channel permission algorithm (owner/admin, `@everyone`/role/member overwrites, implicit view/send rules, timeouts).

## 0.1.0-alpha.1 — 2026-10-07

Release-pipeline test build: an empty window, no Discord functionality yet.

### Added
- Cargo workspace (`fastcord-model`, `fastcord-app`) with an empty iced window.
- `Snowflake` ID type.
- CI: rustfmt, clippy, and tests on Windows, Linux, and macOS.
- Tag-triggered release workflow publishing Windows, Linux, and macOS (arm64, x86_64) builds with `SHA256SUMS`; manual dry run via `workflow_dispatch`.
