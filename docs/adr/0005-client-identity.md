# ADR 0005: Identify as the official Discord client

Status: accepted (2026-10-07), revised (2026-10-25)

## Context

Every Gateway Identify carries `properties` (OS, browser, client build). The
options are the official desktop client's identity, the web client's identity,
or an explicit "fastcord" identity. An explicit third-party identity raises ban
risk; every official profile needs its version fields kept current.

On 2026-10-07 fastcord chose the web/Chrome profile. On 2026-10-25 the
maintainer changed the target to the **official desktop client (Electron)**:
fastcord is a native desktop application, a native app that claims to be a
browser is a contradiction, the `web` presence and session platform are wrong
for it, and the browser-only surface cannot be reproduced from Rust. The web
profile is superseded; its 2026-10-07 record is kept under "History".

The 2026-10-24 live session ended with the alt account restricted
(`docs/TESTING.md`). The identity audit in `docs/IDENTITY_AUDIT.md` found that
the web profile had the right shape but not its fields: a random
`launch_signature` that claims client mods in 4095 of 4096 launches while
`has_client_mods` is false, no `client_heartbeat_session_id`, no installation
id, no pre-auth fingerprint, API/Gateway v10 where the documented client
version is v9, no browser header or cookie surface, and nonces with random low
bits. The switch to desktop does not repair those by itself; they are separate
P0 requirements below. This is a deliberate ToS-risk decision: it does not make
an account safe and it does not authorize any challenge bypass.

## Decision (2026-10-25)

The identity target is the **official Discord desktop client (Electron)** on
the stable release channel for the host platform. Every field, header, version
and wire behaviour the official client sends that fastcord can produce
truthfully must match, and no field may contradict another.

### Profile

- `browser: "Discord Client"`.
- `client_version` is the Discord host version (for example `1.0.9015`).
- `browser_user_agent` is the Electron user agent with the same values:
  `Mozilla/5.0 (<platform>) AppleWebKit/537.36 (KHTML, like Gecko)
  discord/<client_version> Chrome/<chromium_version>
  Electron/<electron_version> Safari/537.36`.
- `browser_version` is the Electron version (the official desktop value, not
  the Chromium version).
- `client_build_number` is the web build from `https://discord.com/app`; the
  desktop client loads the same web app.
- `native_build_number` is the native metadata version from the update
  manifest (see "Version chain").
- `os`, `os_version` (full, for example `10.0.26100`), `os_arch` and
  `app_arch` are the host's real values; Windows also sends `os_sdk_version`;
  Linux sends `window_manager`, `distro` and, when the official capture has
  it, `runtime_environment`.
- `system_locale`, `has_client_mods: false`, `release_channel: "stable"`,
  `client_event_source: null`. The web-only `device` and `referrer*` fields
  are optional; the published desktop example omits them, so a sanitized
  capture decides whether to send them.
- Launch identity: a fresh `client_launch_id` (UUIDv4) per launch;
  `launch_signature` generated with the documented client-mod detection bits
  cleared, never a bare UUIDv4; `client_heartbeat_session_id` (fresh UUIDv4,
  regenerated every 30 minutes, with the REST header carrying the current
  value); `client_app_state` only if a sanitized official capture shows it.
- Installation and experiments: the installation id from READY
  `apex_experiments` (or `GET /apex/experiments?surface=2`) is persisted and
  sent as `X-Installation-ID` and `installation_id`; `X-Fingerprint` is
  generated through `GET /experiments` for the unauthenticated login flow, and
  a captured value is never reused across installations or accounts.
- Transport: API and Gateway version 9 (the documented client default); the
  Electron/Chromium header set (`sec-ch-ua*` as Electron sends it,
  `sec-fetch-*`, `accept-language`, `accept-encoding`, `origin`/`referer`,
  `x-discord-timezone`); a cookie jar for `discord.com`; the same headers on
  the Gateway, voice and remote-auth WebSocket handshakes.
- Behaviour: message nonces in the official shape (timestamp in the high bits,
  zero low 22 bits); opcode 4 without `self_stream`, which the server
  sets when opcode 18 creates a stream (corrected 2026-10-25); the persisted
  guild-version cache and `capabilities` follow the official capture once their
  payloads are handled.

The profile is built once at login and is stable across reconnects. The
30-minute heartbeat and the installation id learned after READY are explicit
exceptions. The exact wire shape is `docs/PROTOCOL.md`; the evidence and
per-item corrections are `docs/IDENTITY_AUDIT.md`.

### Version chain

The desktop profile needs four independent version values; three come from
public endpoints and the Electron/Chromium pair is pinned:

- `client_version` and `native_build_number`: from
  `https://updates.discord.com/distributions/app/manifests/latest` with
  `channel=stable`, the host `platform`, `arch`, and a client-generated
  `install_id`. `full.host_version` gives `client_version`;
  `metadata_version` gives `native_build_number`. The bundled values are the
  fallback when the fetch fails.
- `client_build_number`: from `https://discord.com/app`, as the web profile
  already does.
- Electron and Chromium versions: **not** in the manifest. They are a pinned
  pair in the profile, refreshed when the host version changes. A scheduled or
  CI check fetches the manifest and `/app`, compares them with the pinned
  values, and fails when the pin needs review; it never downloads Discord.
  Reading the values from the official `package.json` is an optional manual
  step, not a CI dependency.
- The OS build, architecture, SDK version and Linux desktop fields come from
  the host.

A consistency test asserts that the user agent contains
`discord/<client_version>` and `Electron/<browser_version>`, that
`native_build_number` is the manifest `metadata_version`, and that
`client_build_number` is the current web build. No release may ship with one
value updated and another stale.

### Residual differences

Header work cannot close the following, and the policy does not pretend it
does:

- the TLS/HTTP2 fingerprint (rustls/ring against Chromium's BoringSSL) and the
  HTTP/2 settings and header order;
- the voice transport (native UDP/RTP/AEAD against WebRTC/ICE/DTLS/SRTP);
- the absence of client telemetry (`/science`, `/track`) and of a real Electron
  runtime.

The first two need a separate transport project (a Chromium-compatible TLS
stack and WebRTC), not another header. They are tracked as known residuals; if
a future decision requires full indistinguishability, they become blocking
milestones rather than accepted risk.

### Consequences

- Four version values to keep current instead of two: the Discord host
  version, the native metadata version, the web build, and the Electron +
  Chromium pair.
- The pin check and the consistency test are mandatory; a stale Electron or
  Chromium version is a contradiction even when the host version is right.
- The client is still detectable by the residual differences above; the policy
  improves consistency, not immunity, and does not guarantee that an account
  survives.
- No challenge, CAPTCHA or verification is solved or bypassed; a demanded
  action still stops the affected work and points to the official client.

## Rejected

- **An explicit "fastcord" identity:** a third-party signal Discord can act on,
  and a higher risk for the user's account.
- **The web/Chrome profile (2026-10-07, superseded 2026-10-25):** it claims a
  browser surface and a `web` presence that a native application cannot honour,
  and it does not remove the TLS or voice differences either.

## Current implementation

`fastcord-discord::gateway::ClientProperties::desktop` builds the desktop
profile above (2026-10-25, `PROFILE_VERSION` 3): the desktop field set and
order, the host's `os_version`/`os_sdk_version` and Linux
`window_manager`/`distro` from `fastcord_platform::os_release`, the version
chain (`resolve_versions`: manifest and `/app` once per process, bundled
fallbacks), the Electron 42.11.15 / Chromium 148.0.7778.280 pair read from the
installed stable Windows host 1.0.9265, a launch signature with the 12
client-mod bits clear, a `client_heartbeat_session_id` regenerated every 30
minutes, and the installation id READY `apex_experiments` issues, kept in the
non-secret `installation-id` file of the data folder and sent as
`installation_id` and `X-Installation-ID` (`gateway::Identity`, shared by the
REST client and the Gateway). The pin check is
`live_version_chain_matches_the_pin`, run daily by
`.github/workflows/identity-pin.yml`. Password login sends the `X-Fingerprint`
of `GET /experiments`. API and Gateway use v9. REST sends the Chromium
same-origin header set with `x-discord-timezone` and a bounded `discord.com`
cookie jar; the WebSocket handshakes carry the user agent, language and
no-cache headers. Message nonces have the official shape (zero low 22 bits,
one process-wide sequence). Opcode 4 stays without `self_stream`, as the
official client sends it. The exact payload, the gaps and the per-item corrections are in
`docs/PROTOCOL.md` and `docs/IDENTITY_AUDIT.md`.
The accepted-shape evidence from 2026-10-07 (a bogus-token Identify over the
real compressed Gateway answered with close 4004, not 4002; build number
631730; Chrome 155) belongs to the web placeholder and is kept only as
history. The desktop target needs a fresh sanitized official capture and a new
live acceptance check (U1).

## History (2026-10-07)

The original decision was the web/Chrome profile: `browser: "Chrome"` with a
matching user agent, truthful OS/locale, `release_channel: "stable"`, a
per-launch `client_launch_id`/`launch_signature`, and a current
`client_build_number` fetched from discord.com (bundled fallback). The build
number came from `window.GLOBAL_ENV` in `https://discord.com/app`; Chrome 155
and build 631730 were observed on 2026-10-07. The 2026-10-22 amendment removed
a captured `x-fingerprint` shared across installations. That profile had fewer
native-only fields, but it also had the `web` presence, the browser surface and
the contradictions listed above. Its implementation notes are preserved in
git history and in the 2026-10-07 section of `docs/PROTOCOL.md`.
