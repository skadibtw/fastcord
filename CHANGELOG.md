# Changelog

## Unreleased

### Added
- `fastcord-model`: typed Discord entities (`User`, `Role`, `Channel` with forward-compatible `ChannelKind`, `Message`, attachments, reactions, replies), serde wire decoding (decimal-string IDs/permissions), partial `MessageUpdate` merge that never clears omitted fields, and Discord's channel permission algorithm (owner/admin, `@everyone`/role/member overwrites, implicit view/send rules, timeouts).
- `fastcord-discord`: fixed-origin `/api/v10` REST over reqwest/rustls with sensitive raw-token authorization, zeroizing/redacted user credentials, bounded JSON bodies, typed permission/resource/authentication/retryable errors, and account-wide 401 cancellation. Its monotonic-clock scheduler learns shared buckets while retaining major IDs, reserves concurrent capacity, serializes unknown routes, honors fractional resets and route/global 429 deadlines, and prioritizes user writes over speculative reads.
- Token login: masked, bounded token entry with an alt-account/ToS acknowledgement; off-thread `GET /users/@me` validation; correct account identity; explicit memory-only sessions; saved-login restoration; and logout that stops authenticated work, purges account state, and reports native deletion failures without claiming success.
- `fastcord-platform`: `keyring-core` with exactly one target-selected native backend (Windows Credential Manager, macOS Keychain, Linux Secret Service), credentials under service `fastcord` and account ID, native account discovery without a plaintext index, serialized off-thread access, zeroizing/redacted loaded tokens, and categorical secret-safe errors.
- QR login (ADR 0006): `fastcord-discord::remote_auth` implements Discord's remote-auth protocol v2 over a rustls/ring WebSocket with a per-attempt RSA-2048/OAEP key pair, nonce proof, fingerprint verification, heartbeats, the server's session timeout, pending-user display, and the ticket-for-token exchange. The login screen shows the QR code (primary method) with explicit start, cancel, regenerate, expiry and phone-cancel handling; the resulting token follows the same `GET /users/@me` validation and native-store path as token paste. A CAPTCHA demand stops with a clear message pointing to token login or the official client. Keys, tickets, QR links, and tokens are memory-only, redacted in `Debug`, and dropped when an attempt ends.

## 0.1.0-alpha.1 — 2026-10-07

Release-pipeline test build: an empty window, no Discord functionality yet.

### Added
- Cargo workspace (`fastcord-model`, `fastcord-app`) with an empty iced window.
- `Snowflake` ID type.
- CI: rustfmt, clippy, and tests on Windows, Linux, and macOS.
- Tag-triggered release workflow publishing Windows, Linux, and macOS (arm64, x86_64) builds with `SHA256SUMS`; manual dry run via `workflow_dispatch`.
