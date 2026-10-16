# Prompt for native macOS acceptance

Paste the following into the agent running on the maintainer's Mac:

```text
Resolve and natively verify fastcord issues #51, #52, #53 and #68. This is an
implementation and acceptance task, not permission to close issues without
evidence. Follow AGENTS.md, docs/SPEC.md and docs/TESTING.md. Use the current
working tree containing commit 2e8dd47 (macOS ownership fixes) and preferably
1b04dd0 or later. The Windows acceptance pass also changes the declaration of
ZONE_IDENTIFIER in crates/fastcord-platform/src/lib.rs to
#[cfg(target_os = "windows")]; include this one-line CI fix. If the remote
checkout lacks these changes, obtain the matching source from the maintainer
before testing; do not silently validate the older remote revision.

Work only on the four macOS regressions; preserve unrelated working changes.
Record commit/tree, macOS version, architecture, Rust/SDK versions and actual
audio/GPU devices. Start with formatting, workspace clippy with -D warnings,
and workspace tests, all --locked. Fix real failures without suppressing
warnings, weakening assertions, adding arbitrary sleeps or editing the Cargo
registry. Keep callbacks allocation/lock free and preserve metadata/CF ownership.

#51: with headphones and microphone permission, run the following serially in
debug and release:
cargo test -p fastcord-audio --locked live_default_devices_loopback_round_trip -- --ignored --nocapture --test-threads=1
cargo test -p fastcord-audio --locked live_switches_available_devices_without_fallback -- --ignored --nocapture --test-threads=1
Repeat both commands with --release. Exercise repeated start/stop and device
replacement, missing-output/no-fallback and recovery. Establish that callback
owners retire at the teardown boundary and that no callbacks continue after
stop. Do not call a delayed release a permanent leak without evidence.

#68: read vendor/coreaudio-rs/FASTCORD_PATCH.md and the patched registration
functions. Add/run a narrowly scoped native regression that deterministically
forces render/input registration failure after callback/buffer construction,
proves captured owners and backing buffers are released on the error path,
and then exercises successful creation/replacement/play/stop. Do not leave
failure injection enabled in production, loosen unsafe policy, or claim that
a compile check proves native failure handling. Repeat #51 device tests after
the regression. If there is no safe native injection seam, report the exact
remaining gap and leave #68 open.

#52: run all fastcord-video VideoToolbox software fixture, VUI/metadata and
session-restart tests; run the ignored hardware_encode_decode_720p30 test on
the actual supported hardware. Check repeated teardown for retained copied
CoreVideo attachments/IOSurfaces. Capture a fresh native build log: the
CVBufferGetAttachment deprecation must be absent. A single Apple Silicon run
does not prove Intel runtime acceptance.

#53: follow vendor/block/FASTCORD_PATCH.md exactly to compile block with
warnings denied and run vendor/block/tests/runtime.rs against the real Blocks
runtime. Check stack/heap invocation, copying/cloning and exactly-once capture
release. Check the build's future-incompatibility report for block. Launch the
real packaged fastcord app, verify Metal login rendering and clean shutdown,
and exercise the supported tiny-skia fallback without introducing a new
production override solely for tests. Keep Intel coverage explicitly pending
if this host cannot run it.

Do not automate password, CAPTCHA or OS privacy dialogs; hand those to the
maintainer if encountered. Discord credentials are unnecessary for these
regressions. Never record credentials in logs or fixtures.

Update docs/TESTING.md with dated native results and remaining gaps. Update
CHANGELOG if code changes. Commit only coherent fixes and their evidence,
preserving unrelated work. Close each issue only when its own criteria pass;
otherwise report the failing command, concise failure/root cause and what
still prevents closure. Return an English handoff with commits, exact test
totals, native runtime evidence and the status of each of #51/#52/#53/#68.
```
