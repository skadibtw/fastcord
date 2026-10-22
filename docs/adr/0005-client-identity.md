# ADR 0005: Identify as the Discord web client

Status: accepted (2026-10-07)

## Context
Every Gateway Identify carries `properties` (OS, browser, client build). Options: present the official desktop client's identity, the web client's identity, or an explicit "fastcord" identity. An explicit third-party identity raises ban risk; the desktop profile requires tracking desktop-only fingerprints.

## Decision
Present the Discord web client's identity: `browser: "Chrome"` with a matching user agent, truthful OS/locale, `release_channel: "stable"`, a per-launch identity (`client_launch_id`, `launch_signature`), and a current `client_build_number` fetched from discord.com web assets at login (bundled fallback). The profile lives in one versioned module in `fastcord-discord` and is the single identity of both the Gateway Identify and the REST client.

## Consequences
- Fewer native-only fields to keep current than the desktop profile; still needs build-number upkeep.
- Open risk U1b: the web client uses WebRTC for voice, fastcord uses native UDP voice. Verify acceptance on the alt account in milestone 17; if flagged, revisit this ADR (desktop profile).
- Supersedes the original SPEC §4.3 wording against fingerprint imitation. The identity is generated once per launch and never changes between reconnects; a server challenge is still never worked around.

## Rejected
- **Desktop profile:** more native fingerprints (native build number, `client_version`, OS-specific fields) to keep current.
- **An explicit "fastcord" identity:** a third-party signal Discord can act on, higher ban risk for the user's account.

## Implementation notes (milestone 6)
- The profile is `fastcord-discord::gateway::ClientProperties::web`, built once per launch (login) and reused by every Identify and every REST request; the exact payload is recorded sanitized in `fixtures/gateway/identify.sanitized.json` and documented in `docs/PROTOCOL.md`.
- `client_build_number` comes from the `"BUILD_NUMBER"` entry of `window.GLOBAL_ENV` in the HTML of `https://discord.com/app` (unauthenticated, 1 MiB read cap, plausibility-checked), resolved once per launch and timestamped; the bundled fallback is `631730` (observed 2026-10-07). Chrome 155 is the version in the UA and `browser_version` (stable channel on 2026-10-07, from Google's version-history API); update it, the fallback, and the fixture together when the profile is reviewed (`PROFILE_VERSION`).
- `client_launch_id` and `launch_signature` are two fresh UUIDv4s generated once per launch (`ClientProperties::with_launch_identity`) and fixed for every Identify that follows, as the web client does; `client_app_state` is `"focused"`. Not sent: the desktop-only `native_build_number`.
- The same `ClientProperties` is the identity of the REST client: `x-super-properties` carries the identical JSON base64-encoded, with the profile's user agent and `x-discord-locale`. Amendment 2026-10-22, profile version 2: omit `x-fingerprint`; a captured fingerprint must not be reused across installations/accounts or replaced by an invented value. This fixes shared identifier reuse but does not establish compatibility or protection from account restrictions.
- Evidence so far: a real Identify with a bogus token over the real compressed Gateway is answered with close 4004 (structure accepted), not 4002. Acceptance with a valid alt-account token (U1) is still pending the live check in `docs/TESTING.md`; if READY does not arrive without a challenge, the Gateway stops (`Stopped`/`AuthenticationRequired`) and this ADR must be revisited rather than worked around.
