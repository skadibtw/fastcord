# Protocol notes

Wire-level behavior fastcord implements or depends on, with its evidence status. Design decisions live in `docs/SPEC.md` and `docs/adr/`; this file records what the code actually sends and expects. Community-documented behavior (userdoccers, `discord.py-self`) is a compatibility dependency, not a guarantee.

## Main Gateway (milestone 6)

Implemented in `fastcord-discord::gateway`. One `Gateway` handle is one account's connection lifecycle; a Tokio task owns the socket and hands ordered `GatewayEvent`s to the single consumer (the reducer of milestone 7).

### Connection and compression
- URL from `GET /gateway` (cached; asked again only after a failed connect), then `?v=10&encoding=json&compress=zlib-stream`. Only `wss://` hosts on `discord.gg` are ever connected to or sent the token; anything else (including a hostile `resume_gateway_url`) is refused, a hostile resume URL falling back to the discovered one.
- One persistent zlib context per connection (`flate2`, pure Rust). WebSocket message payloads accumulate until the `00 00 ff ff` sync-flush suffix, which is looked for at the end of the accumulation, so a message boundary may fall anywhere (even inside the suffix). The dictionary is never reset on a live connection; a new connection starts a new one. Identify's `compress` stays `false`.
- Safety ceiling: 16 MiB per decompressed event, and 16 MiB of compressed bytes accumulated toward one. Exceeding it ends in `Stopped(EventTooLarge)` (a diagnosable startup error, never silent truncation). Decompression is incremental, so a bomb is stopped before it is allocated. Scratch above 256 KiB is released as soon as the event has been handled.
- zstd-stream is intentionally not implemented (SPEC §4.1).
- Outbound frames are capped at 15 KiB locally, and a sliding window counts the 120-frames-per-60-seconds send ceiling; Identify, Resume, and heartbeats are always sent but counted, and server heartbeat requests beyond the ceiling are ignored.

### State machine
`Connecting -> AwaitHello -> Identifying | Resuming -> Ready -> Reconnecting -> Connecting …`, plus two terminal states: `AuthenticationRequired` (back to login) and `Stopped(reason)` (conditions reconnecting cannot fix).

- HELLO's interval is clamped to 1 s–5 min; the first heartbeat is offset by `jitter * interval`; every heartbeat carries the last delivered dispatch sequence (or `null`); server heartbeat requests (op 1) are answered immediately. A heartbeat interval that passes without an ACK ends the connection (`Reconnecting(HeartbeatTimeout)`); it is never left displayed as connected. While events are waiting for a slow consumer the task is not reading, so missing ACKs are not held against the server.
- Resume (op 6) uses the READY `session_id`, the last delivered sequence, and the READY `resume_gateway_url`. A Resume is never followed by an Identify on the same connection. Replayed dispatches with a sequence at or below the last delivered one are dropped before they reach the consumer, so replay cannot duplicate a message, a reaction, or a deletion.
- Reconnect delay: a connection that stayed Ready for 30 s reconnects immediately; otherwise `min(60 s, 1 s·2^(n-1))` with jitter between half and all of it. Non-resumable Invalid Session waits a random 1–5 s. Connect failures keep the session (a network outage says nothing about it); only the server ends it.
- Op 7: reconnect and Resume. Op 9: `d = true` Resumes; `d = false` drops the session and Identifies after 1–5 s. Five consecutive Invalid Session answers to Identify end in `Stopped(IdentifyRejected)`: the profile or account is being refused without a close code and fastcord does not loop on it.

| Close code | Meaning here |
|---|---|
| 4004 | `AuthenticationRequired`: stops all attempts and all authenticated work of the account |
| 4003, 4007, 4009, 1000, 1001 | session is gone: reconnect and Identify |
| 4001, 4002, 4010–4014, 4016 | `Stopped(Rejected(code))` (our payload or configuration is refused; retrying cannot help) |
| 4015 | `Stopped(TooManySessions)` |
| anything else (4000, 4005, 4008, abnormal closure, no code) | reconnect and Resume |

A 401 on `GET /gateway`, or on any other request of the account (`RestClient::authentication_required`), also ends in `AuthenticationRequired`. READY with a non-empty `required_action` is delivered, then ends in `Stopped(ActionRequired(code))`. No challenge is ever completed or bypassed.

Dropping the `Gateway` handle closes the socket with code 1000, which ends the session on Discord's side.

### Capabilities
Identify's `capabilities` is derived from named flags, never a magic number: `LAZY_USER_NOTES` (1<<0), `NO_AFFINE_USER_IDS` (1<<1), `DEDUPE_USER_OBJECTS` (1<<4), `PRIORITIZED_READY_PAYLOAD` (1<<5, requires dedupe), `PASSIVE_GUILD_UPDATE_V2` (1<<14) = **16435**. No intents. Not enabled because their payloads have no handler: protobuf user settings, reaction debouncing, client-state v2, versioned read states and guild settings, token refresh, auto call/lobby connect, channel obfuscation. The empty `client_state.guild_versions` is sent (cold login; no versions are persisted).

### Identify profile (ADR 0005, risk U1)
Built once per Gateway start by `ClientProperties::web` and reused unchanged by every Identify. The accepted-shape payload, with the token replaced, is recorded in `fixtures/gateway/identify.sanitized.json` and asserted byte-for-value by a test:

- `properties`: `os` / `os_version` / UA platform token from the host (`Windows`/`10`, `Mac OS X`/`10.15.7`, `Linux`/`""`, which is what Chrome reports on each), `browser: "Chrome"`, `browser_user_agent` and `browser_version` for Chrome 155 (reduced UA, minor parts `0`), `device: ""`, `system_locale` from the OS, empty referrer fields, `release_channel: "stable"`, `has_client_mods: false`, `client_event_source: null`, and `client_build_number`.
- `client_build_number` is read from the `"BUILD_NUMBER"` entry of `window.GLOBAL_ENV` in `https://discord.com/app` at Gateway start (unauthenticated, 1 MiB read cap, plausibility range 100 000–9 999 999), timestamped when resolved, and falls back to the value bundled at release time (`631730`, observed 2026-10-07). It is resolved once per run and never changes between reconnects. Fields that are per-launch UUIDs (`client_launch_id`, `launch_signature`, …) are deliberately not sent.
- Also: `presence {status: "unknown", since: 0, activities: [], afk: false}`, `compress: false`.

Evidence (2026-10-07, Windows, unauthenticated): the real endpoints returned build number 631730; a real Identify with a deliberately bogus token reached the real Gateway over `compress=zlib-stream`, its compressed HELLO decoded, and Discord answered with close code **4004** (authentication failed), not 4002 (undecodable payload): the structure of the payload is accepted. **Not yet established:** acceptance of the profile together with a *valid* account token (READY/READY_SUPPLEMENTAL on the alt account, and no challenge). That check is `live_account_identifies_and_resumes_after_a_forced_reconnect` (see `docs/TESTING.md`); until it has run, U1 remains open and the profile above is "structurally accepted", not "accepted".

### READY normalization
Decoded straight from the borrowed event text into typed structs that name only the fields used, so unknown fields are skipped as they are read and no untyped JSON tree is built; READY's arrays stream element by element.

- `users` (deduplicated, each once, current user included) plus ID references: guild members carry `user_id`; DMs and group DMs carry `recipient_ids`. A payload that still embeds user objects (no dedupe) normalizes to the same shape.
- `merged_members` is parallel to `guilds` (an unavailable guild's slot is empty). A length mismatch is `Stopped(MalformedReady)`: attaching members by position would silently be wrong.
- Guild fields come from the guild object or from `properties` (client-state-v2 shape). Channels learn their `guild_id`. Unknown channel types are preserved. Null arrays mean empty. Partial user objects (missing `username`) are accepted.
- Unavailable (outage or geo-restricted) guilds are listed by ID only.
- READY_SUPPLEMENTAL: per guild `voice_states` and the remaining members (from its own `merged_members`, parallel to its own `guilds`), plus `lazy_private_channels`. Malformed ones stop like malformed READY.
- PASSIVE_UPDATE_V2 is decoded (channel unread markers, voice state changes/removals, members) for the reducer of milestone 7.
- Live dispatch events keep their wire shape (`MESSAGE_CREATE/UPDATE/DELETE`, `GUILD_CREATE/DELETE`, `CHANNEL_CREATE/UPDATE/DELETE`); a payload that does not decode is skipped (its sequence still counts), unknown event names are consumed. `Debug` of events names the event only, never content or URLs.

### Consumer queue
Events pass through a byte-bounded queue (2 MiB budget, 256 events): each reserves about twice its frame size and returns it when taken. A slow consumer stops the task from reading the socket instead of growing memory; nothing is dropped, ordering is kept, and heartbeats continue. An event larger than the whole budget (a big READY) is queued alone. Events already decoded are always delivered before a reconnect, so the Resume sequence equals what was delivered.
