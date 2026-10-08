# Repository Guidelines

## Project Overview
fastcord is a native, lightweight third-party Discord desktop client in Rust. Goals: no ads/upsells, idle RAM < 150 MB, near-zero idle CPU. MVP scope (nothing more): text channels, DMs/group DMs, voice with device selection and noise suppression, screen share send/watch, emoji and reactions. Login is by user-token paste (ToS risk accepted; test on an alt account). No selfbot automation.

`docs/SPEC.md` is the source of truth for design decisions and the **ordered milestone list (§16)**. Read the relevant section before implementing anything.

## Architecture & Data Flow
Cargo workspace under `crates/`. Dependency direction is strict and acyclic (SPEC §3):

```
model  <-  discord
model  <-  media              (voice WS, UDP/RTP, AEAD, DAVE; no cpal/iced)
model, media, platform <- audio   (cpal, Opus, nnnoiseless, jitter, mixing)
model, media, platform <- video   (capture, codecs, packetization)
all    <-  app                (binary `fastcord`, iced UI, orchestration)
```

- `fastcord-model`, `fastcord-discord`, `fastcord-platform`, `fastcord-audio`, and `fastcord-app` exist so far; create other crates in the milestone that first needs them, never as empty placeholders.
- One reducer task owns mutable Discord state; UI receives deltas/read models, never clones of full state.
- Networking, decoding, and audio never run on the UI thread. Audio callbacks: no locks, allocation, I/O, or inference.
- Every queue/cache is **byte-bounded**.
- OS-specific code lives in `fastcord-platform` modules, not per-OS crates. iced/wgpu interop stays in `app`.

## Key Directories
| Path | Purpose |
|---|---|
| `crates/fastcord-model` | Pure domain types (`Snowflake`, entities); no I/O, runtime, or GUI deps |
| `crates/fastcord-discord` | Authenticated REST transport, rate-limit scheduler, QR-login remote-auth client (`remote_auth`), main user Gateway lifecycle and opcode-37 subscriptions (`gateway`), single-writer bounded normalized account state (`state`), typed channel-history reads (`history`), byte-bounded message bodies with focus-based retention and in-flight page reconciliation (`message_store`), message creation with nonce, explicit mention policy, and not-sent/ambiguous failure classification (`send`), own-message ownership, content-only edits, deletion, and edit-answer ordering (`edit`) |
| `crates/fastcord-platform` | Native credential storage; one target-selected keyring backend |
| `crates/fastcord-app` | iced application; binary name `fastcord`; history coordinator with send and edit/delete reconciliation (`history`), unconfirmed-send outbox (`outbox`), unfinished edits/deletions of own messages (`changes`), message composer with edit mode (`composer`), variable-height virtual list (`variable_list`), and message timeline with own-message actions (`timeline`) |
| `crates/fastcord-audio` | Audio engine: cpal device enumeration/selection by stable ID, owner thread per call, lock-free preallocated SPSC rings, format/rate conversion (rubato), Opus encode/decode with FEC/PLC (`opus2`, bundled libopus), callback allocation audit marker (`audit`) |
| `docs/` | `SPEC.md` (design + milestones), `PROTOCOL.md` (what is actually sent/expected on the wire), `TESTING.md`, `RELEASE.md` |
| `scripts/package.sh` | Per-target release packaging used by CI |
| `packaging/macos/Info.plist` | `.app` bundle template (`@VERSION@` substituted) |
| `.github/workflows/` | `ci.yml` (fmt/clippy/test, 3 OS), `release.yml` (tag → GitHub Release) |

## Development Commands
```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo run -p fastcord-app
cargo build --release -p fastcord-app
gh workflow run release.yml        # release dry run, no publish
```
On the maintainer's Windows box, agent shells may not have `cargo` on `PATH`; call `C:/Users/user/.cargo/bin/cargo.exe` directly. Building `fastcord-audio` compiles the bundled libopus with CMake; if `cmake` is not on `PATH`, set `CMAKE` to the Visual Studio Build Tools copy (`C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe`). Linux builds need `libasound2-dev` (ALSA headers for cpal).

## Code Conventions & Common Patterns
- Edition 2024, resolver 3, toolchain pinned to 1.99.0 (`rust-toolchain.toml`). Workspace lints: `unsafe_code = "deny"`, clippy `all` warn; CI denies warnings.
- Shared dependency versions go in root `[workspace.dependencies]`; crates use `dep.workspace = true` and `[lints] workspace = true`.
- IDs are `Snowflake(u64)` internally, decimal strings on the wire.
- Errors: typed error enums per crate implementing `std::error::Error`; no panics on network input.
- Async: Tokio for control-plane I/O; real-time audio/video on dedicated threads fed by bounded channels.
- UI: iced 0.14 `application(boot, update, view)`; typed `Message` enum; long-lived workers via `Subscription`, never started from `view`.
- Secrets (user token, voice tokens, DAVE keys, signed CDN URLs) never reach logs, fixtures, or test output.
- Files are LF (`.gitattributes`).

## Important Files
- `crates/fastcord-app/src/main.rs`: entry point (`windows_subsystem = "windows"` in release).
- `Cargo.toml`: workspace, shared deps, release profile (thin LTO, stripped).
- `CHANGELOG.md`: update the `Unreleased` section in every feature commit.

## Runtime/Tooling Preferences
- Rust via rustup; Windows needs VS Build Tools with the C++ workload (MSVC linker).
- GUI: iced with wgpu (tiny-skia fallback). Audio: `cpal`, `opus2` (bundled libopus, needs CMake), `rubato`. Planned: `nnnoiseless`, `davey` (DAVE). Video: OS-native codecs (Media Foundation / VideoToolbox / VA-API) + `dav1d`; **no FFmpeg** (`docs/adr/`).
- GitHub Actions are pinned by commit SHA; keep it that way when adding steps.

## Workflow
- **One milestone (feature) = one commit** on `main`, including tests, docs, and the changelog entry. Follow SPEC §16 order.
- Documentation and commit messages in English.
- Primary manual test target: Windows. Live Discord tests use the alt account only; never commit tokens.
- Release: bump `[workspace.package].version`, tag `vX.Y.Z`, push the tag (see `docs/RELEASE.md`).

## Testing & QA
- Unit tests live next to code (`#[cfg(test)] mod tests`). Tests are deterministic and offline: protocol/REST fixtures in `fixtures/`, injected clocks for rate limits, crypto/media test vectors.
- No live-credential tests in CI. Hardware-dependent checks (audio devices, capture, GPU codecs) are manual and recorded per milestone.
- Each milestone's acceptance criteria in SPEC §16 must be exercised before committing; UI changes need a real launch of `fastcord`.
