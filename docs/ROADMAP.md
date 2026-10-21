# Delivery roadmap

Approved 2026-10-15. [GitHub tracker #67](https://github.com/skadibtw/fastcord/issues/67) collects the existing issues and the ordered post-MVP milestones. SPEC §16 remains the source of truth for feature order and scope. This roadmap is not a claim that unfinished native or live checks have passed.

## Readiness and order

Track four separate forms of evidence: implementation, deterministic/offline checks, native-device checks, and official-client interoperability. A checked-in implementation or closed historical issue does not establish the latter two. Preserve dated results in TESTING.md and record new results with the tested commit/tree, platform and backend. Close an issue only when its own acceptance criteria pass.

1. Address macOS regressions #51 (CoreAudio callback retirement), #52 (CoreVideo attachment ownership), and #53 (renderer dependency compatibility). Native Mac acceptance is still required. [#68](https://github.com/skadibtw/fastcord/issues/68) tracks the separate dependency ownership leak on failed callback registration discovered during review; repair it before M38 acceptance.
2. Audit the pending checks of earlier/closed milestones, starting with login/QR, Gateway, text/DMs and voice/DAVE. Complete open acceptance work in SPEC order: #14, #15, #22, #23, #25, #26, #28 and #29. These features already have implementations; reproduce first and fix observed failures.
3. Complete #30; verify existing M31/#31 acceptance; then complete #32 through #38 in SPEC order. Linux VA-API needs real GPU hardware beyond the Hyper-V software path. macOS acceptance is run by the maintainer on their Mac; architecture cross-compilation is not native runtime evidence.
4. Implement M39-M51 below in order, after M38. One feature per commit, with tests, docs and changelog. Do not add placeholder crates.

## Approved everyday-client stage

| Milestone | Issue | Capability |
|---|---|---|
| M39 | [#54](https://github.com/skadibtw/fastcord/issues/54) | Message formatting, embeds and mentions |
| M40 | [#55](https://github.com/skadibtw/fastcord/issues/55) | Attachment uploads |
| M41 | [#56](https://github.com/skadibtw/fastcord/issues/56) | Read state, unread markers and mentions |
| M42 | [#57](https://github.com/skadibtw/fastcord/issues/57) | Native desktop notifications |
| M43 | [#58](https://github.com/skadibtw/fastcord/issues/58) | Server-backed message search |
| M44 | [#59](https://github.com/skadibtw/fastcord/issues/59) | Pinned-message browser |
| M45 | [#60](https://github.com/skadibtw/fastcord/issues/60) | Friends, relationship actions and profiles |
| M46 | [#61](https://github.com/skadibtw/fastcord/issues/61) | Presence and own status |
| M47 | [#62](https://github.com/skadibtw/fastcord/issues/62) | Direct and group calls |
| M48 | [#63](https://github.com/skadibtw/fastcord/issues/63) | Text thread participation |
| M49 | [#64](https://github.com/skadibtw/fastcord/issues/64) | Forum posts and replies |
| M50 | [#65](https://github.com/skadibtw/fastcord/issues/65) | Everyday keyboard navigation and session drafts |
| M51 | [#66](https://github.com/skadibtw/fastcord/issues/66) | Everyday-client release acceptance |

New functions reuse the single-writer reducer, small UI read models, existing nonce-aware outbox, byte-bounded caches and native media engines. Search uses server pagination, uploads stream from disk, drafts stay in session memory, and no persistent plaintext message database is added. Every new authenticated protocol action needs sanitized fixtures and live alt-account evidence rather than assuming a bot endpoint proves user-client compatibility.

These are the boundaries of the M39-M51 stage: camera, server administration/moderation and inline audio/video attachment playback were not included. On 2026-10-16 the maintainer approved the later [full free communication stage](COMMUNICATION_COVERAGE.md), tracked in [#109](https://github.com/skadibtw/fastcord/issues/109). It adds these functions and communication apps/community tools through #69-#108. Paid/Nitro features, game library/game activities, store/ads and selfbot automation remain excluded. M51 is not the final parity gate; #108 is.

## Release gates

Run the full SPEC §12 matrix at M38 and again at M51: matched RAM/CPU/GPU measurements against the official client (hard 150 MB RAM limit removed on 2026-10-21) and mean idle CPU below 0.5% of one logical core over ten minutes; thirty-minute voice; twenty share cycles; return within 10 MB of the warmed baseline two minutes after media teardown. Record GPU memory separately and validate all four packaged targets. Linux/macOS report their native memory metrics. Missing account access, devices or a supported native host leaves the corresponding gate pending.
