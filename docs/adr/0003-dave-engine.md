# ADR 0003: davey for DAVE (E2EE)

Status: accepted (2026-10-07)

## Context
DAVE (MLS-based audio/video E2EE) is mandatory for Discord voice, DM calls, and Go Live since 2026-03-01 (SPEC §6.4). Candidates: `davey` (Rust, OpenMLS, MIT) and Discord's official `libdave` (C++, MLS++, OpenSSL/BoringSSL).

## Decision
Use `davey`, pinned to an exact version, behind an internal `DaveSession` adapter in `fastcord-media`. `libdave` is the reference/test oracle only; its vectors validate our integration.

## Rationale
- Production use: `@discordjs/voice` ships `@snazzah/davey` as its only supported DAVE library (~1.5M weekly npm downloads); discord.py-self uses it too. It runs against live Discord servers every day.
- Pure Rust: builds on all three OSes with Cargo alone; no `unsafe` FFI in our crates (`unsafe_code = "deny"` stays).
- Supports Opus, H.264, VP8, VP9, AV1 frame transforms and passthrough mode for the transition period.

## Consequences
- Bus-factor risk: 0.1.x, one primary maintainer. Mitigation: pin, test against libdave vectors, be ready to patch/fork.
- Never ship two DAVE engines; never fall back to unencrypted media.

## Rejected
- **libdave via FFI:** hand-written unsafe C-ABI wrapper, C++/MLS++/BoringSSL native builds (CMake, Perl, Go, NASM on Windows) on three OSes, and a second crypto/TLS stack in the binary.
