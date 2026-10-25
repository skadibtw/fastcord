# Client identity audit: what still tells fastcord apart

Status: accepted as the gap list for ADR 0005 revision 2026-10-25
(official-client impersonation). Basis: the source tree and the public
user-client documentation at <https://docs.discord.food>; no live request was
made for this review and no official-client capture is available in the
repository.

This is the gap list behind the 2026-10-25 policy revision. Each item is a
requirement, written as a correction. It is not a claim that any single item
caused a specific account action. The 2026-10-24 live session ended in a
restriction (`docs/TESTING.md`), which is *consistent* with an anti-abuse
signal, but nothing here proves the cause.

## Why this matters

ADR 0005, as revised on 2026-10-25, makes fastcord present itself as the
official Discord client. The current profile has the web placeholder's *shape* but
several fields are missing or wrong, and some of those fields are documented as
part of Discord's anti-abuse and experiment tracking. Closing the
inconsistencies below is required by that policy, not optional polish. It does
not make the client indistinguishable: the TLS stack and the voice transport
alone prevent that (see "Residual differences").

Priorities: **P0** = likely to matter to anti-abuse, cheap; **P1** = clear
mismatch, moderate cost; **P2** = cosmetic or unverified; **—** = do not change.

## Summary

| ID | Signal in fastcord | Basis | Priority | Correction cost |
|---|---|---|---|---|
| A1 | `launch_signature` is a bare UUIDv4, so its mod-detection bits are random | code + docs | P0 | small |
| A2 | `client_heartbeat_session_id` is never sent | code + docs | P0 | medium |
| A3 | `X-Installation-ID` / `installation_id` are never sent | code + docs | P0 | medium |
| A4 | READY's `apex_experiments.installation` is ignored | code + docs | P0 | small |
| A5 | `X-Fingerprint` is omitted from the login flow | code + docs | P1 | small |
| A6 | API and Gateway use `v10`; the documented client version is `v9` | docs | P1 | small, wide |
| A7 | REST has no Chromium/Electron header surface (UA-CH, cookies, language, encoding, timezone) | code + docs | P1 | medium |
| A8 | No cookies at all on `discord.com` or `gateway.discord.gg` | code + docs | P1 | medium |
| A9 | WebSocket handshake carries only `Origin` | code | P1 | small |
| A10 | rustls/ring TLS and h2 fingerprints differ from Chromium/Electron | code | P1 | large |
| A11 | Voice says `protocol: "udp"` under a desktop identity | code + docs | P1 | very large |
| B1 | Nonces carry 22 random low bits; official-client nonces do not | code | P1 | small |
| B2 | Voice state never sends `self_stream` | code | P1 | small |
| B3 | `capabilities` is a deliberate subset | code + docs | P2 | large |
| B4 | `guild_versions` is always empty | code | P2 | medium |
| B5 | `x-discord-locale` is the OS locale, not the account locale | code | P2 | medium |
| B6 | `client_app_state` / initial presence are unverified guesses | code | P2 | small |
| B7 | No `/science`, `/track` or `/experiments` traffic | code + docs | P2 | — |
| C1 | Desktop version chain ages out (host, native, Electron, Chromium) | code | P2 | small |
| C2 | CDN requests have no default headers | code | P2 | small |
| D | Local naming is "fastcord" everywhere | code | — | keep |

## A. Fields that Discord's anti-abuse and experiments use

### A1. `launch_signature` is random where it must be a bit field (P0)

`ClientProperties::with_launch_identity` sets both identifiers from a fresh
UUIDv4 (`crates/fastcord-discord/src/gateway/profile.rs:232`). The docs say a
launch signature "may appear to be a random UUID" but certain bits flag
detected client mods (JQUERY, BetterDiscord, Vencord, ...): 12 documented bits,
at 119, 108, 100, 91, 84, 75, 61, 55, 48, 38, 24 and 11. The published default
generator clears those bits, so a clean
client sends all of them as zero
(<https://docs.discord.food/reference#launch-signature>).

A UUIDv4 has 122 random bits and none of the 12 positions overlap its fixed
version/variant bits, so a random signature has all 12 mod bits clear with
probability 1/4096: in 4095 of 4096 launches fastcord claims one or more client
mods while `has_client_mods` is `false` (`profile.rs:207`). That is both a
third-party signal and a self-contradiction inside the same properties object.

**Correction.** Generate the signature with the documented mask (copy the
published constant, or OR the 12 documented bits and verify they agree), keep
`has_client_mods: false`, and keep the per-launch lifetime. Add a test that 256
generated signatures have every documented bit clear and still parse as
RFC 4122 UUIDv4. Bump `PROFILE_VERSION` to 3 and update the fixture, ADR and
`docs/PROTOCOL.md`. If a capture shows the value is stable per installation
rather than per launch, persist it.

### A2. `client_heartbeat_session_id` is missing (P0)

The published client-properties examples carry
`client_heartbeat_session_id`: a client-generated UUID "representing the
current persisted analytics heartbeat, regenerated every 30 minutes"
(<https://docs.discord.food/reference#client-properties>). fastcord has no such
field anywhere.

**Correction.** Add `client_heartbeat_session_id: String` to
`ClientProperties`, generate a UUIDv4 at login, and rotate it every 30 minutes.
Identify sends the value current at connect time; the REST header must carry
the current value, which means the prebuilt `x-super-properties` default header
(`crates/fastcord-discord/src/rest.rs:555`) can no longer be immutable. Encode
the header once, keep it in a swappable slot, and replace it when the heartbeat
rotates (or build it per request and accept the allocation). Add a
paused-clock test for rotation and a header test.

### A3. No installation ID (P0)

Official clients send `X-Installation-ID` on API requests and
`installation_id` in Identify; the value persists after authentication. A
missing installation is valid (the server issues one), but a client that never
has one after the first session is unusual
(<https://docs.discord.food/topics/experiments#installations>).

**Correction.** Decode `apex_experiments` from READY (the app currently ignores
it), take `installation`, store it as a non-secret per-installation setting, and
send it as `X-Installation-ID` plus `installation_id` from then on. Before a
value exists, omit both. If a live capture shows the header is expected even on
the first requests, obtain one with `GET /apex/experiments?surface=2`
(unauthenticated) before login.

### A4. READY's experiment data is ignored (P0, same fix as A3)

`READY` may carry `apex_experiments`, including `installation`. The decoder
keeps only the fields it names, so the value is dropped. **Correction:** parse
the bounded field and hand the installation to the profile/settings layer; the
assignments themselves can stay unused.

### A5. `X-Fingerprint` is omitted (P1)

The docs say an unauthenticated client should obtain a fingerprint from
`GET /experiments` and send it in `X-Fingerprint` (and applicable `fingerprint`
body fields) until authentication
(<https://docs.discord.food/authentication#fingerprints>). fastcord sends none;
ADR 0005 removed a *captured* value because reusing it across installations was
wrong, which is correct, but the documented replacement is a generated value,
not omission (`rest.rs:578`).

**Correction.** Generate one fingerprint per installation through the
unauthenticated endpoint, cache it (the endpoint is limited to 3 per 2 minutes
per IP), and send it on the unauthenticated login requests. Do not reuse a
captured value across accounts. Update ADR 0005, SPEC §4.3 and
`docs/PROTOCOL.md`, which currently present omission as the final decision.

### A6. API and Gateway version (P1)

The client-version table marks API `v9` as the default for clients; `v10` is
available but is the version fastcord uses everywhere
(`crates/fastcord-discord/src/route.rs:11`) and forces in the Gateway URL. A
client that claims to be the desktop client but calls `/api/v10/...` and
`gateway.discord.gg/?v=10` does not look like the official client
(<https://docs.discord.food/reference#api-versions>).

**Correction.** Confirm with a live capture which version the current official
client uses (the docs say v9). If it is v9, switch `API_BASE` and the Gateway
`v=` parameter to 9, update the URL validation and fixtures, and keep the v10
routes that have no v9 equivalent only where a live check requires them. This
touches many tests but no protocol logic.

### A7. The REST surface is not the official client's (P1)

`identity_headers` sends only `Accept: application/json`,
`x-super-properties`, `x-discord-locale` and the user agent (`rest.rs:555`).
Missing relative to a Chromium/Electron request to `discord.com`:

- `sec-ch-ua`, `sec-ch-ua-mobile`, `sec-ch-ua-platform` (and the high-entropy
  hints only if the site asks for them);
- `sec-fetch-site`, `sec-fetch-mode`, `sec-fetch-dest`;
- `accept-language`, `accept-encoding` (no compression feature is enabled in
  `reqwest`, so none is advertised);
- `origin` and `referer` on writes;
- `x-discord-timezone`;
- `cookie` (see A8).

Also, the `Accept` value for a Chromium `fetch` is normally `*/*`, not
`application/json`; verify against a capture.

**Correction.** Build one Chromium/Electron header set from the same profile
(so the Chromium major in `sec-ch-ua` matches the user agent) and install it in
the REST client. Enable `gzip`/`brotli`/`deflate` in `reqwest` if the advertised
`accept-encoding` is to match Chromium, otherwise advertise only what the client
can decode and treat the difference as accepted. Add `x-discord-timezone` from
the OS time zone. Add a recorded-request test that compares the header names
and values with a sanitized capture.

### A8. No cookies (P1)

No `reqwest` client enables a cookie store (`Cargo.toml` has no `cookies`
feature), so fastcord never sends the Cloudflare cookies the official client
always has (`__dcfduid`, `__sdcfduid`, `_cfuvid`, and `cf_clearance` when
issued). A Chromium user agent with zero cookies is a bot signal even before
any Discord field is compared.

**Correction.** Add a bounded in-memory cookie jar for `discord.com` (and the
hosts the login flow uses), populated by the `/app` build-number fetch and the
login responses, and send it on REST and WebSocket connections. Do not send it
to the credential-free CDN client, which is deliberately cookie-free for signed
URLs; decide separately whether asset fetches should carry the user agent only
(C2). Never log cookies.

### A9. WebSocket handshakes (P1)

The main Gateway, voice Gateway and remote-auth sockets add only `Origin`
(`crates/fastcord-discord/src/ws.rs:157`,
`crates/fastcord-media/src/gateway/transport.rs:117`). The official client's
handshake also carries `User-Agent`, `Accept-Language`, `Cookie`, `Cache-Control: no-cache`,
`Pragma: no-cache` and `Sec-WebSocket-Extensions: permessage-deflate`.
fastcord offers no extension because it does not implement the extension.

**Correction.** Add the same profile user agent, `Accept-Language` and cookie
jar to the handshake. Do not offer `permessage-deflate` until the client can
decode it; an unoffered extension is a difference, an offered-but-unhandled one
is a bug.

### A10. TLS and HTTP/2 fingerprints (P1, unavoidable cheaply)

`reqwest` and `tokio-rustls` use rustls with `ring` (`Cargo.toml:76`,
`Cargo.toml:101`). Chromium/Electron uses BoringSSL with a different cipher order,
extension set, GREASE values and ALPN behaviour, and its HTTP/2 SETTINGS,
priority frames and header order also differ. Cloudflare terminates
`discord.com`, so JA3/JA4-style classification can separate the two regardless
of every header above.

**Correction.** Accept and document, or adopt a BoringSSL-backed HTTP client and
a Chromium-like h2 profile (a large dependency and an ongoing compatibility
burden). This is the main reason to be honest about the limit of identity
imitation: the header work is still worth doing, but it cannot make the
transport indistinguishable.

### A11. Voice uses UDP under a desktop identity (P1, architectural)

`select_protocol` sends `protocol: "udp"` with a native RTP/AEAD transport
(`crates/fastcord-media/src/gateway/wire.rs:519`). The official desktop client
uses WebRTC: ICE, DTLS and SRTP, from its Electron stack. No official client
sends UDP voice.

**Correction.** Either implement WebRTC (ICE/DTLS/SRTP, a separate transport
project) or accept that the desktop profile does not remove the contradiction.
There is no small fix; this is the known U1b risk and it applies to every voice
session, not only to text.

## B. Protocol and behaviour

### B1. Nonces (P1)

The nonce generator puts 22 random bits in the low bits, and the code comments
say the official client's nonces have no entropy there
(`crates/fastcord-discord/src/send.rs:37-85`). Every message fastcord sends is
therefore distinguishable by its nonce shape alone.

**Correction.** Generate nonces in the official shape: timestamp in the high
bits, low 22 bits clear. To keep per-process uniqueness when two sends land in
the same millisecond, advance the timestamp by one millisecond (or use the
official client's collision behaviour if a capture shows one). Keep reusing the
same nonce on an explicit retry, which is required for `enforce_nonce`. Add a
test for the zero low bits and for monotonicity.

### B2. `self_stream` (P1)

`VoiceStateRequest` has no `self_stream` field
(`crates/fastcord-model/src/voice.rs:89`), and starting a share does not update
the voice state (`crates/fastcord-app/src/gateway/stream_share.rs`). Official
clients set it when Go Live starts and clears it when it ends. fastcord itself
reads the field to build stream offers
(`crates/fastcord-app/src/gateway.rs:3270`), so its own streams are offered from
a field its own client never sets.

**Correction.** Add `self_stream: bool` to `VoiceStateRequest`, send `true` when
the stream is created and `false` on stream delete, stop or leave, and update
the reducer/tests. This is both a protocol-parity fix and a user-visible bug
fix.

### B3. Capabilities (P2)

Identify sends 16435, a named subset of five flags (`capabilities.rs:48`). The
official client at a given build enables a larger set; the exact set is not
captured here.

**Correction.** Capture a sanitized current desktop Identify and match the set,
implementing the payload handlers first. Do not copy bits whose shapes are not
handled: enabling them changes READY and breaks decoding. This is a larger
project (protobuf user settings, client-state v2, versioned read states).

### B4. `guild_versions` (P2)

Every Identify sends an empty cache (`crates/fastcord-discord/src/gateway/wire.rs:126`),
so every launch is a cold login and every guild is sent in full. The official
client persists versions and sends them on reconnect.

**Correction.** Persist a bounded per-account map of guild versions and send it;
or accept the cost (it is also a performance difference, not just a signal).

### B5. `x-discord-locale` (P2)

The header is the OS locale because it reuses `properties.system_locale`
(`rest.rs:569`). The official client sends the account's chosen client locale;
the OS locale belongs in `system_locale`. A Russian Windows account whose Discord
UI is English therefore sends `ru-RU` where the official client sends `en-US`.

**Correction.** Read the account locale from the login response or READY
`user_settings.locale`, use it for `x-discord-locale`, and keep the OS locale
for `system_locale`. Rebuilding the header after READY is the same mechanism as
A2.

### B6. `client_app_state` and initial presence (P2, unverified)

fastcord always sends `client_app_state: "focused"` and
`presence.status: "unknown"` (`profile.rs:223`,
`gateway/wire.rs:119`). The published web and desktop property examples do not
show `client_app_state` at all, and an initial `"unknown"` status is unusual;
both need a capture before changing.

**Correction.** Record a sanitized official Identify and compare the two fields;
remove `client_app_state` if the official client does not send it, and send the
stored status at Identify if it does. Compare `device` and the `referrer*`
fields too: the published desktop example omits them, while the current web
placeholder sends `device: ""` and empty referrers. Do not guess.

### B7. Analytics traffic (P2, do not fake)

The official client sends `/science` and `/track` events and calls the
experiment endpoints; fastcord sends none. Absence is a behavioural signal, but
fabricating analytics or experiment traffic would be worse than the signal.

**Correction.** None. Document it as an accepted difference.

## C. Upkeep

### C1. Desktop version chain (P2)

The desktop profile has four independent values: `client_version` and
`native_build_number` (host and native, from Discord's update manifest),
`client_build_number` (the web build), and the Electron/Chromium pair (pinned,
not in the manifest). The current build only knows the web build and Chrome
155; the desktop chain does not exist yet.

**Correction.** Fetch the manifest at login with the host platform, arch and an
`install_id`; take `full.host_version` and `metadata_version`, with bundled
fallbacks. Keep the Electron/Chromium pair in the profile and refresh it when
the host version changes. Add a CI or scheduled check that fetches the manifest
and `/app`, compares them with the pinned/bundled values, asserts the
user-agent/native/build consistency, and fails for review; it never downloads
Discord. Keep `PROFILE_VERSION` review in the same check.

### C2. CDN requests (P2)

The attachment/CDN client deliberately has no default headers
(`crates/fastcord-app/src/attachments.rs:188`). The official client sends a user
agent, `Accept`, `Accept-Language` and a `Referer`. The CDN is public and signed-URL
based, so the risk is lower than on the API, but it is still a difference.

**Correction.** Send the profile user agent and an image/video `Accept` on
asset fetches; leave cookies and authorization off. Do not add signed-URL
logging.

## D. User-visible differences (not server-visible)

These are not defects and should not be "fixed" by imitation: the app is
`fastcord` in its window title, tray tooltip, notifications, credential service
and autostart entry (`main.rs:172`, `tray.rs:48`, `platform/lib.rs:24`,
`autostart.rs:12`). The English-only UI, the absence of Nitro/shop/quests/
Activities, the native renderer and the explicit ToS warning on login are
visible signs to the user. Keeping the fastcord name and not copying Discord's
name, logo or icon is the legally and ethically correct choice; the audit above
is about the wire identity, not about hiding the product.

## E. Residual differences (accepted on 2026-10-25)

Even after every correction above, these remain and are stated in the ADR
rather than papered over:

1. **TLS/h2 fingerprint** (A10): rustls is not Chromium's BoringSSL.
2. **Voice transport** (A11): UDP/RTP is not WebRTC.
3. **Telemetry** (B7): no `/science` traffic.
4. **No real Electron runtime**: no Chromium JS engine, no native modules, no
   canvas/audio fingerprint, no CDP, no real cookie history.
5. **Feature set**: a native client does not behave like the official client
   in every request pattern.

The 2026-10-25 decision accepts these residuals for now and tracks them; they
are not claimed solved. If a future decision requires full indistinguishability,
they become blocking milestones (a Chromium-compatible TLS stack and WebRTC),
not another round of header work.

## F. Specification status

This branch applies the policy: ADR 0005 carries the 2026-10-25 revision,
SPEC §4.3 states the requirements and residuals, `docs/PROTOCOL.md` marks the
target desktop profile, and `README.md` describes the impersonation. The remaining
work is implementation and verification: the checks in G, plus the per-item
tests listed under each correction. `docs/TESTING.md` gains the live capture
task when the identity milestone starts.

## G. Verification plan

- Unit tests: signature bits clear; heartbeat rotation with a paused clock;
  installation parsed from READY and persisted; nonce low bits zero; header set
  matches the profile; `self_stream` transitions; locale selection.
- Fake server: assert the outgoing Identify and `x-super-properties` field set,
  the REST header names and the voice state payloads. The fake records requests
  without secrets.
- Live, alt account only: capture one sanitized Identify and one
  `x-super-properties` from the official desktop client (its DevTools or a
  local proxy on the maintainer's machine), compare field by field, then run
  the existing live gates. Never commit tokens, cookies, fingerprints or
  installation IDs.
- If a challenge or restriction occurs, stop; do not retry around it.

## Appendix: if the observer is DPI, not Discord

The audit above is about Discord's servers. A network filter sees a different
set of signals, and the embedded byedpi (`crates/fastcord-bypass`) changes only
some of them:

| Signal | fastcord | Official client | Effect of byedpi |
|---|---|---|---|
| TLS ClientHello | rustls/ring cipher order and extensions | Chromium/BoringSSL | Splitting changes TCP segmentation, not the ClientHello bytes; a fingerprint-based block still sees rustls |
| HTTP/2 | reqwest/h2 settings and header order | Chromium h2 | Unchanged |
| QUIC/HTTP3 | not used | Chromium/Electron may use it to Cloudflare | Unchanged (TCP only) |
| Voice | UDP RTP + custom AEAD | WebRTC (ICE/DTLS/SRTP) | Voice is not routed through byedpi; a filter can separate the two transports |
| DNS | system resolver | the official client may use DoH | Unchanged |
| SNI/hosts | `discord.com`, `gateway.discord.gg` | same | Same; SNI blocking affects both |

If the question is "why is fastcord blocked while the official client works",
the answer is the first and fourth rows: a rustls ClientHello and raw UDP voice
are distinguishable from Chromium and WebRTC even when the TCP stream is
desynchronized. Fixing that means adopting a Chromium-compatible TLS stack and
WebRTC, which is the same work as A10 and A11, not a byedpi strategy change.

## H. Open questions

1. Which API and Gateway versions does the current official client use? (Docs
   say v9; confirm live.)
2. Is `launch_signature` per launch or per installation? (Docs call it a launch
   signature and publish a per-call generator.)
3. Does the official client send `client_app_state` and which initial presence
   status? (The published examples omit the former and show no presence.)
4. Which capabilities does the current official client send?
5. Does the official client keep sending `X-Fingerprint` after authentication?
6. Does `X-Installation-ID` appear on the very first requests, or only after
   READY supplies it?
