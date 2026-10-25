# Identity audit implementation handoff

Prepared: 2026-10-25. Code baseline: `identity/official-client` at `55f4892`.
This is an execution plan for other agents, not a record of work completed.

## 1. Objective and branch boundary

Finish the actionable remainder of `docs/IDENTITY_AUDIT.md`, with evidence for
each wire change and an explicit list of accepted residual differences. Keep
the client functional, byte-bounded, and compatible with embedded byedpi.

- `feat/byedpi` at `d9b31d7` is published as `origin/feat/byedpi`.
- Current local `main` at the handoff is `daa4215`; it already contains byedpi.
- Merge `56d0d9a` brought that main into identity and preserved all bypass hooks.
- Commit `55f4892` contains the review corrections. Identity has not been pushed
  by this work. Integrate subsequent work into this branch, not into main.
- Do not reset main, rewrite the published byedpi branch, publish identity,
  create releases, or access a main Discord account as part of this plan.
- The main checkout has an unrelated untracked `MANUAL_ACCEPTANCE.md`; leave it
  alone. Use the identity checkout or isolated worktrees based on the baseline.

Read the actual current files before starting; hashes and test counts here are
a handoff snapshot, not permission to overwrite later changes.

## 2. Required reading and interpretation

Read repository `AGENTS.md`, SPEC sections 2.3, 3, 4.1, 4.3, 5.4 and the relevant
section 16 acceptance rules, ADR 0005, the identity sections of PROTOCOL and
TESTING, then IDENTITY_AUDIT section F before its historical item descriptions.

The audit's opening table and many A/B descriptions are historical. For example,
they still describe API v10, missing heartbeat fields and no cookies. Those are
not the current implementation. Do not mechanically implement that table.

Use these statuses consistently:

| Status | Meaning |
| --- | --- |
| Implemented/offline tested | The code and deterministic tests exist |
| Compared with capture | The specific field/behavior matches a recorded official build and scenario |
| Live checked | The named scenario ran with the designated alt, with recorded outcome |
| Partial | Some requirements/evidence are still missing |
| Blocked on evidence | A specific capture, host or credential is unavailable |
| Accepted residual | Deliberately outside this identity layer; remains observable |
| Withdrawn | The original correction was wrong; do not implement it |

Never equate these statuses. A bogus-token Identify ending in 4004 does not
establish a valid-account READY, account safety, or client indistinguishability.

## 3. Preserve the existing corrections

These are regression obligations, not greenfield tasks:

| Area | Baseline behavior |
| --- | --- |
| A1 | Launch signature clears the documented client-mod bits |
| A2 | Shared Identity rotates its heartbeat session every 30 minutes |
| A3/A4 | READY installation is persisted; Identify includes it; REST sends X-Installation-ID |
| REST P1 | Explicit RestProperties allowlist excludes installation_id, is_fast_connect and version |
| A5 | Pre-token password login obtains a fingerprint through GET /experiments |
| A6 | REST and main Gateway use v9 |
| A7 | REST has the implemented Chromium header subset, four content decoders, timezone and current-location Referer |
| A8 | A bounded in-memory discord.com cookie jar exists; CDN stays credential-free |
| A9 | Main/remote-auth and voice WS have UA, language, HTTP encoding and no-cache headers; no WS compression extension |
| B1 | Nonces have zero low 22 bits and are reused on explicit retries |
| B2 | Withdrawn: do not add self_stream to opcode 4 |
| B6 | client_app_state is already omitted; initial presence is still unknown |
| C1 | Nullable native metadata, macOS-specific fallback and Darwin SDK major; five-target manifest pin check |
| Bypass | Version fetch, REST/auth, CDN/storage and both WS transports retain their existing byedpi routes |

Baseline validation: Windows workspace clippy passed; 2,129 tests passed, 52
live/hardware tests were ignored; fmt, license freshness and ARM64 macOS platform
cross-check passed. Windows test linking still emits the existing byedpi LNK4217
signal-import warning. Do not describe that run as warning-free.

## 4. Coordination and dependencies

Use one coordinator and at most three worker agents at a time. This document
does not itself launch agents. Suggested worktree branches are below; create
them only when executing the work, starting from the coordinator's integration
baseline rather than an old main.

| Packet | Suggested branch | Dependencies | Parallel work allowed |
| --- | --- | --- | --- |
| I0: evidence and current status | codex/identity-evidence | Baseline | With I1 implementation and I2 investigation |
| I1: WS extension support | codex/identity-ws | Baseline; final offer values need I0 | With I0 and I2 |
| I2: platform runtime evidence | codex/identity-platforms | Baseline; host access | With I0 and I1 |
| I3: REST/cookies/CDN conformance | codex/identity-http | I0; coordinate I2 profile changes | With I5 handler work, if files do not overlap |
| I4: account locale and initial presence | codex/identity-account-state | I0; shared Identity contract | Serialize against I3 and final I5 wire edits |
| I5: capability handlers | codex/identity-capabilities | I0 target mask; I4 settings contract | Research/isolated decoders may start earlier |
| I6: warm guild state | codex/identity-warm-state | I0 semantics; required I5 handlers | After their state/wire contracts stabilize |
| I7: integration and acceptance | Coordinator | Required packets integrated | Final gate |

Suggested waves: I0/I1/I2, then I3 plus isolated I5 handler work, then I4 and
completion of I5, then I6, then I7. If capture is unavailable, continue transport
tests, bounded storage design and platform investigation; do not invent target
values to unblock dependent wire changes.

File ownership rules:

- I1 owns both WS transports and any approved shared extension implementation.
- I2 owns platform facts and the version-pin changes in profile.rs/workflow.
- I3 owns HTTP header policy and cookies; I4 takes the shared Identity/header
  fields only after I3 hands them off.
- I5 owns capability declarations and corresponding decoders; I6 takes the
  Identify client_state and persistent guild-state integration afterward.
- profile.rs, rest.rs, gateway/wire.rs, gateway/decode.rs, app/login.rs and
  app/gateway.rs are shared hot spots. One writer at a time; agree interfaces
  before branching implementations that touch them.
- Workers report documentation changes; the coordinator owns shared audit,
  protocol, testing, SPEC, ADR and changelog integration to avoid conflicts.
- One coherent implementation packet per commit, including its tests and docs.
  Keep packet order explicit; do not invent new milestone numbers.

## 5. I0: evidence, fixture contract and status ledger

Deliver a current status table mapping every audit ID to code, evidence,
remaining work and its owner. Preserve historical audit text but clearly mark
it as historical; remove misleading statements that old defects still exist.

Obtain a sanitized capture from the official stable desktop client on the
designated alt, recording OS/architecture, client host/native/web versions,
Electron/Chromium versions, capture date and scenario. Windows x64 is the first
target. Other targets belong to I2 and must not inherit Windows runtime facts.

Capture the following separately:

1. A cold start and a warm start: initial REST requests, Identify, the relevant
   READY/settings/version shapes, and reconnect/Resume versus fresh Identify.
2. REST from home, a guild channel and a DM; at least one explicit write.
3. Main Gateway, remote-auth and voice WS handshakes: actual headers, negotiated
   extensions and compression parameters. Do not assume the hosts behave alike.
4. Account locale different from OS locale; initial presence choices where the
   alt can exercise them. Establish whether Accept-Language follows either.
5. Public image/avatar and attachment/media fetches, including redirect behavior.

Use existing DevTools or another suitable capture method. A TLS-intercepting
proxy is not evidence of the official client's original TLS fingerprint.
If a session or device is unavailable, record exactly which observation is
missing; preparing a capture procedure is not completion of the capture gate.

Do not commit raw HARs or authenticated dumps. Build minimal fixtures with
synthetic tokens, cookies, fingerprints, installation IDs, users/guilds and URLs.
Retain useful types, null/omission distinctions, header spelling and extension
parameters. Record transformation/provenance, not actual account identifiers.
Inspect decoded x-super-properties as well as its base64 form when sanitizing.

Acceptance: each target fact has source/date/platform/scenario, discrepancies
are listed explicitly, and no dependent implementation uses an unsupported guess.

## 6. I1: real permessage-deflate support

Primary files: fastcord-discord/src/ws.rs and
fastcord-media/src/gateway/transport.rs; follow their callers and test sockets.

1. Inspect the locked tokio-tungstenite/tungstenite APIs. The baseline stack has
   no configured permessage-deflate support. Propose a maintained extension-aware
   implementation, a narrow reviewed adapter, or a transport replacement before
   coding. Merely adding Sec-WebSocket-Extensions is unacceptable.
2. Preserve the existing Sink/Stream behavior, TLS trust checks, timeouts,
   cancellation, clean close, fake-discord loopback path and bypass connect hook.
   A shared implementation must obey the crate DAG; do not make media depend on
   discord or create an empty placeholder crate.
3. Implement negotiation and frame semantics: agreed parameters/window bits,
   optional context takeover, RSV1 rules, fragmented compressed messages,
   interleaved control frames and reset on a new connection.
4. Bound compressed input, inflated output, retained contexts and any queues by
   bytes. Respect each caller's existing message limits. A compression bomb,
   malformed stream or unsolicited extension must produce a typed failure.
5. Keep WS permessage compression separate from Gateway zlib-stream payload
   compression and from HTTP Accept-Encoding. Test the combinations the target
   actually negotiates; do not accidentally enable Identify payload compression.
6. Once the implementation works, offer only parameters the client supports,
   matching I0 evidence for each connection class.

Tests: independent RFC 7692 vectors; a local WS peer with extension accepted and
declined; plain/compressed text and binary; fragmentation and ping/pong/close;
context takeover on/off; invalid RSV bits and parameters; output-size bombs;
fresh state after reconnect; fake mode; main, QR and voice integration.

Acceptance: negotiated compressed frames work within existing limits on all
three WS paths, no unsupported offer is sent, bypass remains present, and any
dependency/license changes are accounted for. If replacing transport, include
a resource/idle regression check rather than relying only on payload tests.

## 7. I2: platform/runtime version consistency

Primary files: gateway/profile.rs, platform/src/host.rs, login host assembly,
fixtures/rest/client-identity-profiles.json and identity-pin.yml.

The live public manifest values at the baseline are:

| Platform | Host | Native metadata |
| --- | --- | --- |
| Windows x64 | 1.0.9265 | 96160 |
| Windows ARM64 | 1.0.57 | 96160 |
| macOS x64 | 0.0.419 | 96312 |
| macOS ARM64 | 0.0.419 | 96312 |
| Linux x64 | 1.0.166 | 96160 |

The web build is 649389. Recheck public values when executing. The manifest's
Windows ARM64 host number is verified; its Electron/Chromium pair is not.
Only Windows x64 has baseline runtime evidence for 42.11.15/148.0.7778.280.

1. Obtain runtime-version evidence from official installations or trustworthy
   platform-specific release artifacts for each supported target. Record exact
   provenance; no CI package download is required by the current policy.
2. If pairs differ, introduce a single target-keyed version/pin model that feeds
   UA, browser_version and UA-CH. Do not scatter per-platform literals.
3. Establish native_build_number null versus integer from current host captures.
   Nullable support already exists; neither old null examples nor a manifest
   response alone establish what that host actually sends in client properties.
4. Verify full OS release, SDK field, application/OS architecture including any
   supported emulation, and reduced UA platform tokens. Keep OS queries in platform.
5. Keep the five-target host/native drift check. Distinguish manifest coverage
   from runtime-pair verification in its output and documentation. Define how
   a runtime pin is reviewed when a live host moves; avoid an undocumented
   host/runtime mismatch or silent claim that a fetched host proves the pair.

Tests: pure target matrix independent of runner architecture, nullable wire
serialization, correct fallback pairing, stale/malformed manifest handling,
once-per-process resolution, native host tests and macOS/Linux build checks.

Acceptance: every supported target is either evidence-backed or explicitly
unverified; no Windows pair is labeled verified on another target by inference.

## 8. I3: REST, cookie and CDN conformance

Primary files: discord/rest.rs, cookies.rs, password_auth.rs, fingerprint.rs,
remote_auth/exchange.rs; app/attachments.rs and other actual CDN client builders.

1. Compare the actual request builders with I0 fixtures: GREASE/brand order,
   Accept, language, client hints, timezone, Origin, Referer and request class.
   Existing Referer already follows the visible location, including cross-channel
   background requests; verify the policy instead of deriving it from API route IDs.
2. Keep the explicit REST allowlist. Adding a Gateway field must not implicitly
   expand x-super-properties. Preserve heartbeat updates and installation header.
3. Do not copy REST Sec-Fetch headers to WS by assumption. Resolve the captured
   Electron behavior per host/path, including the possibility that absence is
   correct. Tests must follow capture, not a generic browser-header checklist.
4. Review cookie behavior against actual issued domains/paths and expiry. The
   baseline jar keys only by name and does not model normal expiry/domain/path
   attributes. If these affect correctness, implement bounded scope/expiry with
   an injected clock and document it. Never send discord.com cookies to
   discord.gg, discord.media, CDN/storage, or unrelated redirects by assumption.
5. For CDN/assets, add only the public profile-derived headers established by
   capture. Choose Accept by fetch purpose without claiming unsupported codecs.
   Account for reqwest's automatic encoding headers; no explicit default map does
   not imply that the wire request has no headers.
6. Preserve URL validation, redirect revalidation, signed-URL redaction, the
   credential-free storage uploader, byte budgets and byedpi routing.

Tests: recorded/synthetic request matrix including pre-auth/authenticated calls;
gzip/deflate/br/zstd decoded responses and response limits; cookie isolation,
replacement/expiry/scopes; CDN redirect destinations; no Authorization, Cookie,
fingerprint, installation ID or super-properties on public/storage requests.

Acceptance: observed differences are corrected or separately justified, and
public fetches cannot inherit account credentials through shared header helpers.

## 9. I4: account locale and initial presence

Primary files: Gateway decode/event/connection/wire and state settings modules,
profile Identity/header state, app/login.rs and account-switch teardown.

1. Establish the actual locale source and update shape from I0. Support the
   legacy/protobuf setting forms that the enabled capability set requires.
2. Separate system_locale from account client locale. Keep OS locale in the
   properties; use account locale for x-discord-locale. Determine Accept-Language
   independently from capture; do not assume it has the same source.
3. Use one bounded current settings value shared by requests, not a cloned full
   store. Validate locale strings; define pre-READY fallback and dynamic updates.
4. Clear/account-scope settings during teardown and switch so account B cannot
   inherit account A's headers. Do not invalidate launch identity or installation
   semantics merely to change a locale.
5. Compare initial presence unknown with capture. If a stored status is required,
   obtain it from the authoritative setting/cache and define cold-start behavior.
   Preserve invisible/DND and user intent across reconnects. Do not introduce
   unsolicited presence writes or client_app_state fields.

Tests: ru-RU OS with en-US account and the reverse; missing/malformed settings;
live setting updates; reconnect versus fresh Identify; account A-to-B switching;
initial invisible/DND handling and continued absence of uncaptured fields.

Acceptance: field sources are explicit, no stale account locale leaks, and any
presence change is based on observed semantics rather than a more plausible guess.

## 10. I5: capabilities and their handlers

Primary files: gateway/capabilities.rs, wire.rs, decode.rs, event types,
user_settings.rs, reducer state and all consumers of changed READY shapes.

1. Compute a named bit-by-bit diff between current 16435 and I0's target. Map
   each additional flag to payload changes, dependencies, affected consumers,
   budgets and tests. The target is tied to a build, not a permanent integer.
2. Implement each needed decoder/reducer path before enabling its flag. Audit
   existing partial protobuf support: a guild-folder parser alone does not mean
   all protobuf settings or versioned settings semantics are supported.
3. Handle relevant versioned read states/guild settings, client-state v2 and
   other changed shapes only where evidenced. Unknown fields must remain safe;
   malformed required state must stop cleanly rather than leave a false READY.
4. Integrate flags in small ordered commits with named prerequisites. Do not
   advertise token-refresh, automatic call/lobby behavior, or any other feature
   whose consequences are unimplemented or conflict with explicit-action scope.
5. If a captured flag requires a separate feature project, record the remaining
   difference and prerequisite. A claim of exact capability parity stays open.

Tests: old/new READY and READY_SUPPLEMENTAL shapes, deduplication and alignment,
unavailable guilds, unknown flags/fields, malformed/oversized settings, partial
updates, reconnects and downstream read/unread/member behavior.

Acceptance: every enabled bit has complete bounded handling and consumers; the
actual mask is derived from named flags. No magic-integer replacement of 16435.

## 11. I6: correct warm guild-state persistence

Primary files: gateway client_state/READY decode, guild reducer, the app's
account-scoped persistence boundary and startup/teardown code.

Persisting a version map alone is not sufficient. If Discord omits unchanged
guild data, a new reducer must already have the corresponding baseline to use it.

1. Establish version sources, update/invalidation rules and partial READY
   semantics from I0 and I5. Distinguish Resume (existing in-memory session)
   from a new Identify after restart or an invalid session.
2. Specify a byte-bounded account-scoped snapshot containing each advertised
   guild version and every retained guild-state dependency needed to reconstruct
   that version. Reuse an existing complete persistence mechanism if one exists;
   do not serialize the entire mutable store without a budget/design review.
3. Define a schema/capability version, atomic publication, corruption/oversize
   rejection and migration policy. Publish versions and their state together.
   File I/O stays off the UI and real-time threads.
4. Advertise a version only after its baseline is restored successfully. Cold
   start, incompatible schema, lost entries and failed restore use an honest
   empty/partial version set. A resource limit must not silently drop required
   guild/permission state while continuing to claim its version.
   Budget the final serialized Identify against the existing 15 KiB ceiling,
   including token, properties and all other fields; a bounded disk map alone
   does not guarantee a valid outbound payload. If only a subset fits, claim
   only versions whose baselines can be retained and merge full data for the rest.
5. Invalidate on account switch, guild removal, relevant version changes and
   unsupported state transitions. Do not persist tokens, keys, cookies or signed
   attachment URLs inside this cache.
6. Extend fake/local Gateway tests to omit unchanged guilds and deliver changed
   ones, so tests prove reconstruction rather than merely outgoing map contents.

Tests: cold/warm equivalence; unchanged guild omission; changed/unavailable/deleted
guilds; crash during write; corrupt, stale and oversized snapshots; eviction;
account separation; lost permissions; fresh Identify versus Resume.

Acceptance: warm and cold sessions converge to equivalent required navigation
and permission state, with bounded disk/RAM use and no invented versions. Until
this is achieved, keep the version set empty and B4 explicitly partial.

## 12. I7: integration, regression and live acceptance

The coordinator integrates in dependency order into identity. Recheck main
before any eventual merge; preserve both the identity and current byedpi work.
Do not publish identity merely because local tests pass.

Required offline checks, after integration:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo clippy -p fastcord-fake -p fastcord-discord -p fastcord-app --features fastcord-app/fake-discord --all-targets --locked -- -D warnings
cargo test -p fastcord-fake --locked
python scripts/gen_third_party_licenses.py --check
git diff --check
```

After Cargo.lock changes, fetch locked sources and regenerate the license
document before running its freshness check. Use the repository's pinned Rust,
native toolchain and CMAKE instructions. On this Windows host cargo is at
`C:/Users/user/.cargo/bin/cargo.exe`; Python is available at
`C:/Users/user/AppData/Local/Programs/Python/Python312/python.exe`.

Run relevant macOS/Linux native builds and platform tests. Cross-compilation
only proves type/build compatibility; record actual host runs separately. UI
changes require launching fastcord; fake-mode launch is appropriate for offline
layout/flow checks and cannot prove Discord interoperability.

Recheck bypass hooks and exercise public manifest resolution through embedded
byedpi, using the ignored version-chain test and FASTCORD_IDENTITY_BYEDPI when
needed. Local request/handshake tests must exercise the production builders;
searching for a function name alone does not prove correct routing.

With the designated alt available, perform a controlled acceptance run:

1. Valid-token login to READY and READY_SUPPLEMENTAL, no required_action/challenge.
2. One DM and guild history read; one explicit send/reaction if required by the
   acceptance scenario; verify nonce reconciliation and acknowledgements.
3. Reconnect/Resume; new Identify and warm-start state only where those packets
   have been implemented. Verify the expected settings and capability shapes.
4. Main/QR/voice handshake negotiation as applicable; voice/media checks retain
   the existing DAVE and screen-share acceptance requirements.
5. Account switch/logout, worker cleanup and no continuing authenticated work.

Use credentials only in the single process environment, never command text,
fixtures or output. Respect existing Identify/retry limits. On a challenge,
restriction or required action, stop the affected session and record the
outcome; do not retry around it. If no valid alt is available, U1 stays pending.

Review the existing ignored live helpers before using them: some construct a
bundled diagnostic profile or a direct discovery request. A diagnostic helper's
success must not be labeled a production login/profile/bypass check unless it
actually exercises the final production initialization path.

Acceptance report: exact revision/platform/scenario, commands and outcomes,
test counts, ignored/manual gates, minimal redacted evidence, remaining
discrepancies, and measured resource changes for new transport/cache work.
Update audit F, PROTOCOL, TESTING, SPEC/ADR where decisions changed, and Unreleased.

## 13. Residual tracks and scope limits

A10 TLS/h2, A11 UDP/WebRTC, B7 telemetry and the lack of a real Electron runtime
are accepted residuals in the baseline policy. They are not closed by I0-I7.
Do not quietly start a transport rewrite to improve the status table.

If a later project explicitly changes that policy, prepare separate work:

- TLS/h2: platform coverage and maintenance assessment, origin-TLS trust and
  credential boundaries, HTTP and WS integration, byedpi compatibility,
  independent handshake evidence and idle/resource measurements. Choosing
  BoringSSL alone does not prove a Chromium-equivalent network fingerprint.
- Voice/WebRTC: architecture/ADR first; ICE/DTLS/SRTP and signaling mapping,
  mandatory DAVE preservation, independent Go Live sessions, packetization,
  congestion/retransmission, capture/audio-thread constraints and cross-platform
  interoperability/resource gates. Never relabel existing UDP as WebRTC.
- Telemetry: keep the accepted absence of fabricated /science or /track events.
  Existing genuine experiment/fingerprint calls do not authorize fake analytics.
- Runtime/product: do not add an Electron shell, fake runtime proofs, Discord
  branding, or remove the fastcord name to satisfy this audit.

## 14. Worker assignment and completion templates

Use this assignment text with the appropriate packet substituted:

> Start from the coordinator's current identity integration HEAD, including
> this plan (code baseline 55f4892). Read AGENTS.md, the relevant SPEC sections, IDENTITY_AUDIT F and
> IDENTITY_IMPLEMENTATION_PLAN.md. Implement packet I<N> within its ownership
> boundaries. Inspect the current code before editing. Preserve the explicit
> REST properties view, existing identity lifetimes, byte budgets, crate DAG,
> byedpi routing and fake mode. Agree changes to shared files with the coordinator.
> Do not publish identity or change main. Do not invent capture values or claim
> live acceptance from offline/bogus-token checks. Deliver one coherent commit,
> relevant tests and proposed documentation updates. If evidence is unavailable,
> state the missing observation and complete independent work first.

Every worker's return must include:

1. Packet, branch and commit; changed files and interfaces.
2. Concrete before/after behavior and source of target values.
3. Tests run with outcomes; unrun platform/live gates separately.
4. Budgets, dependency/license changes and byedpi/fake-mode impact.
5. Evidence still missing, accepted residuals and remaining integration needs.

The coordinator closes a packet only against its acceptance criteria. The final
identity result may be implemented and tested while still explicitly awaiting
capture or valid-alt acceptance; it must never be labeled indistinguishable or
account-safe on the strength of these changes.
