# Repository Guidelines

## Project Overview
fastcord is a native, lightweight third-party Discord desktop client in Rust. Goals: no ads/upsells, idle RAM < 150 MB, near-zero idle CPU. MVP scope (nothing more): text channels, DMs/group DMs, voice with device selection and noise suppression, screen share send/watch, emoji and reactions. Login is by user-token paste (ToS risk accepted; test on an alt account). No selfbot automation.

`docs/SPEC.md` is the source of truth for design decisions and the **ordered milestone list (§16)**. Read the relevant section before implementing anything.

## Architecture & Data Flow
Cargo workspace under `crates/`. Dependency direction is strict and acyclic (SPEC §3):

```
model  <-  discord
model  <-  media              (voice WS, UDP/RTP, AEAD, DAVE; no cpal/iced)
model, media, platform <- audio   (cpal, Opus, nnnoiseless, DeepFilterNet, jitter, mixing)
model, media, platform <- video   (capture, codecs, packetization)
all    <-  app                (binary `fastcord`, iced UI, orchestration)
```

- `fastcord-model`, `fastcord-discord`, `fastcord-media`, `fastcord-platform`, `fastcord-audio`, `fastcord-video`, and `fastcord-app` exist so far; create other crates in the milestone that first needs them, never as empty placeholders.
- One reducer task owns mutable Discord state; UI receives deltas/read models, never clones of full state.
- Networking, decoding, and audio never run on the UI thread. Audio callbacks: no locks, allocation, I/O, or inference.
- Every queue/cache is **byte-bounded**.
- OS-specific code lives in `fastcord-platform` modules, not per-OS crates. iced/wgpu interop stays in `app`.

## Key Directories
| Path | Purpose |
|---|---|
| `crates/fastcord-model` | Pure domain types (`Snowflake`, entities); no I/O, runtime, or GUI deps |
| `crates/fastcord-discord` | Authenticated REST transport/rate limits, QR remote auth, main Gateway and opcode-37 subscriptions plus opcode-4 voice state (`gateway`), bounded normalized state (`state`), history and message retention (`history`, `message_store`), nonce-aware sends (`send`), own-message edits/deletes (`edit`), attachment URL refresh wire types (`attachment`), reaction add/typed-remove/who-reacted requests (`reaction`) |
| `crates/fastcord-media` | Voice join correlation (`session`), v8 voice Gateway + UDP task (`gateway`), rtpsize AES-GCM/XChaCha20 AEAD (`crypto`), RTP/RTCP parsing, IP discovery/ping/demux and SSRC-to-user mapping, mandatory DAVE adapter (`dave`), and video RTP (`video`: RFC 6184 H.264 packetization and the AOM AV1 depacketizer, `video/av1.rs`, which only receives; per-SSRC reassembly with NACK scheduling and frame deadlines, bounded pacer, retransmission ring and congestion defaults; the driver glue, with per-SSRC codec dispatch and the DAVE boundary, is `gateway/driver/video.rs`, opened with `connect_with_video`); no decoders or rendering; depends on `model` only |
| `crates/fastcord-platform` | Native credential storage; one target-selected keyring backend; `open_local_file` (OS default handler, never for programs/scripts) |
| `crates/fastcord-app` | iced application; binary name `fastcord`; history coordinator with send, edit/delete, and reaction reconciliation (`history`), unconfirmed-send outbox (`outbox`), unfinished edits/deletions of own messages (`changes`), unanswered reaction requests (`reactions`), message composer with edit mode (`composer`), variable-height virtual list (`variable_list`), message timeline with own-message actions and reaction chips (`timeline`), credential-free CDN fetch plus byte-bounded disk/decoded attachment caches (`attachments`), attachment preview/larger-view/download/open orchestration (`attachment_ui`), source-preserving visible emoji runs and bounded animation handles (`emoji`, `emoji_index`), worker-paged search/recent/variant/custom emoji picker, also used to choose a reaction (`emoji_picker`), and Go Live viewer sessions (`gateway/stream_audio.rs`) with a dedicated, bounded, lazily started AV1 decoding thread per stream (`gateway/stream_video.rs`; pictures are discarded until the M28 renderer) |
| `crates/fastcord-audio` | Audio engine: cpal device enumeration/selection by stable ID, owner thread per call, lock-free preallocated SPSC rings, format/rate conversion (rubato), microphone noise suppression behind one `Denoiser` trait (`denoise`: Off, Light = `nnnoiseless` inline on the owner, High quality = DeepFilterNet 3 built, run and dropped on one dedicated thread over bounded 480-sample channels; engine switches never leave a gap), Opus encode/decode with FEC/PLC (`opus2`, bundled libopus), callback allocation audit marker (`audit`) |
| `crates/fastcord-video` | Video codecs behind the platform-neutral `VideoEncoder`/`VideoDecoder` traits and borrowed NV12 frames (`codec`, `frame`): Windows Media Foundation H.264 (`media_foundation`, hardware first, software fallback), macOS VideoToolbox H.264 (`videotoolbox`), deterministic backend selection (`select`), H.264 Annex B/SPS helpers (`h264`), and AV1 receive decoding with statically bundled dav1d 1.5.4 (`av1`; C bridge `src/dav1d.c`; the pinned source archive, its SHA-256 and provenance in `native/`; `build.rs` compiles it with the target C compiler, portable C with no assembler and no network; any decode failure discards the native context; no AV1 encoder, no FFmpeg) |
| `assets/emoji/` | Versioned, compressed Twemoji PNG bundle, Unicode-index source attribution, and separate MIT / CC-BY-4.0 licenses; decode only visible assets |
| `docs/` | `SPEC.md` (design + milestones), `PROTOCOL.md` (what is actually sent/expected on the wire), `TESTING.md`, `RELEASE.md`, `THIRD_PARTY_NOTICES.md` (shipped in every archive by `scripts/package.sh`) |
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
- GUI: iced with wgpu (tiny-skia fallback). Audio: `cpal`, `opus2` (bundled libopus, needs CMake), `rubato`, `nnnoiseless` (Light noise suppression), DeepFilterNet via the `deep_filter` crate pinned to a git revision (High quality; tract runtime and bundled model, so the first fetch needs `git` and network). Planned: `davey` (DAVE). Video: OS-native H.264 codecs (Media Foundation / VideoToolbox / VA-API) and AV1 through statically bundled `dav1d` 1.5.4 (compiled by `fastcord-video`'s `build.rs` from a pinned source archive in the repository, so builds need only the target C compiler: MSVC on Windows, GCC or Clang on Linux, Xcode tools on macOS); **no FFmpeg** (`docs/adr/`).
- GitHub Actions are pinned by commit SHA; keep it that way when adding steps.

## Workflow
- **One milestone (feature) = one commit** on `main`, including tests, docs, and the changelog entry. Follow SPEC §16 order.
- Documentation and commit messages in English.
- Primary manual test target: Windows. Live Discord tests use the alt account only; never commit tokens.
- Release: bump `[workspace.package].version`, tag `vX.Y.Z`, push the tag (see `docs/RELEASE.md`).

## Testing & QA
- Unit tests live next to code (`#[cfg(test)] mod tests`). Tests are deterministic and offline: protocol/REST fixtures in `fixtures/`, injected clocks for rate limits, crypto/media test vectors. Noise-suppression quality is checked on synthesized WAV fixtures in `fixtures/audio/` (generator, manifest and regeneration command in `denoise/quality.rs`; no third-party audio). H.264 RTP wire behavior is checked against independent vectors in `fixtures/voice/h264-rtp-vectors.json` (generator `fixtures/voice/gen_h264_rtp_vectors.py`, pycryptodome; regenerate with `python fixtures/voice/gen_h264_rtp_vectors.py`) and, end to end, against real MLS/DAVE sessions in `fastcord-media/src/gateway/tests/video.rs`. AV1 follows the same pattern: `fixtures/voice/av1-rtp-vectors.json` (generator `fixtures/voice/gen_av1_rtp_vectors.py`, pycryptodome), real MLS/DAVE driver tests in `fastcord-media/src/gateway/tests/video/av1.rs`, and the dav1d decoder against genuine libaom streams in `fixtures/video/av1-*` (generator `fixtures/video/gen_av1_fixture.py`, which needs libaom's `aomenc` and `aomdec`; the committed files are reproducible with the pinned build recorded in `av1-fixture.json`).
- No live-credential tests in CI. Hardware-dependent checks (audio devices, capture, GPU codecs) are manual and recorded per milestone.
- Each milestone's acceptance criteria in SPEC §16 must be exercised before committing; UI changes need a real launch of `fastcord`.
