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
