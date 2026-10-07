# ADR 0005: Identify as the Discord web client

Status: accepted (2026-10-07)

## Context
Every Gateway Identify carries `properties` (OS, browser, client build). Options: mimic the official desktop client, mimic the web client, or identify honestly as "fastcord". An explicit third-party identity raises ban risk; desktop mimicry requires tracking desktop-only fingerprints.

## Decision
Use a stable web-client profile: `browser: "Chrome"` with a matching user agent, truthful OS/locale, `release_channel: "stable"`, and a current `client_build_number` fetched from discord.com web assets at startup (bundled fallback). The profile lives in one versioned module in `fastcord-discord`.

## Consequences
- Fewer native-only fields to imitate than the desktop profile; still needs build-number upkeep.
- Open risk U1b: the web client uses WebRTC for voice, fastcord uses native UDP voice. Verify acceptance on the alt account in milestone 17; if flagged, revisit this ADR (desktop profile).
- Supersedes the original SPEC §4.3 wording against fingerprint imitation. Still no CAPTCHA bypass or per-connection randomization.

## Rejected
- **Desktop profile:** more native fingerprints (native build number, `client_version`, OS-specific fields) to keep current.
- **Honest "fastcord" identity:** explicit third-party signal, higher ban risk for the user's account.

## Implementation notes (milestone 6)
- The profile is `fastcord-discord::gateway::ClientProperties::web`, built once per Gateway start and reused by every Identify; the exact payload is recorded sanitized in `fixtures/gateway/identify.sanitized.json` and documented in `docs/PROTOCOL.md`.
- `client_build_number` comes from the `"BUILD_NUMBER"` entry of `window.GLOBAL_ENV` in the HTML of `https://discord.com/app` (unauthenticated, 1 MiB read cap, plausibility-checked), resolved once at Gateway start and timestamped; the bundled fallback is `631730` (observed 2026-10-07). Chrome 155 is the version in the UA and `browser_version` (stable channel on 2026-10-07, from Google's version-history API); update it, the fallback, and the fixture together when the profile is reviewed (`PROFILE_VERSION`).
- Not sent, on purpose: per-launch UUIDs (`client_launch_id`, `launch_signature`, `client_heartbeat_session_id`) and `native_build_number` (desktop only).
- Evidence so far: a real Identify with a bogus token over the real compressed Gateway is answered with close 4004 (structure accepted), not 4002. Acceptance with a valid alt-account token (U1) is still pending the live check in `docs/TESTING.md`; if READY does not arrive without a challenge, the Gateway stops (`Stopped`/`AuthenticationRequired`) and this ADR must be revisited rather than worked around.
