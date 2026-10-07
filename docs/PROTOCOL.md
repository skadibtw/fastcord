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
- PASSIVE_UPDATE_V2 is decoded and applied by the bounded reducer (milestone 7): channel unread markers, voice state changes/removals, and normalized members.
- Live dispatch events keep their wire shape (`MESSAGE_CREATE/UPDATE/DELETE`, `GUILD_CREATE/DELETE`, `CHANNEL_CREATE/UPDATE/DELETE`); a payload that does not decode is skipped (its sequence still counts), unknown event names are consumed. `Debug` of events names the event only, never content or URLs.

### Consumer queue
Events pass through a byte-bounded queue (2 MiB budget, 256 events): each reserves about twice its frame size and returns it when taken. A slow consumer stops the task from reading the socket instead of growing memory; nothing is dropped, ordering is kept, and heartbeats continue. An event larger than the whole budget (a big READY) is queued alone. Events already decoded are always delivered before a reconnect, so the Resume sequence equals what was delivered.

## Lazy subscriptions and member lists (milestone 7)

Implemented in `fastcord-discord::gateway::subscription` (what is sent) and `fastcord-discord::state` (what is kept). Evidence: the field set and operation semantics below come from community documentation (`discord.py-self`'s `GuildSubscriptions`/`types/gateway.py`, Userdoccers' opcode table) and have **not** been captured from a live session; risk **U2** stays open until the checks in `docs/TESTING.md` have run. `fixtures/gateway/subscriptions_navigation.json` records exactly what fastcord sends for a two-guild navigation; the `member_list_update_*.json`, `voice_state_update.json`, and `guild_member_update.json` fixtures are shaped after the documented payloads with fabricated IDs.

### Opcode 37
`{"op": 37, "d": {"subscriptions": {"<guild_id>": {...}}}}`. Opcode 14, the deprecated predecessor, is never sent and there is no fallback to it; a payload that cannot fit stops with `SubscriptionTooLarge` rather than being silently dropped. Each guild entry always carries every field:

| Field | Sent | Meaning |
|---|---|---|
| `typing` | `true` for a wanted guild, `false` to release it | In the community documentation this flag is what subscribes the connection to the guild (it is how the guild's other fields start to take effect); fastcord sets it for that reason only. TYPING_START is never decoded or shown. |
| `threads`, `activities`, `member_updates` | always `false` | Not MVP features. |
| `members` | user IDs, at most 200 per guild | Individually requested members (visible authors, reply targets, voice participants, permission computation). |
| `channels` | `{channel_id: [[start, end], ...]}` | Member-list ranges of the visible member sidebar only. Empty for voice-only subscriptions and milestone 8 navigation (no member sidebar is displayed). |
| `thread_member_lists` | always `[]` | Not MVP. |

Because it is not established whether an omitted field means "unchanged" or "cleared", every field of every changed guild is sent explicitly: the result is the same under either reading. A guild is released by sending it with `typing: false` and everything else empty, never by leaving it out.

What is subscribed: the selected guild and the guild of the active voice channel, nothing else. When a member sidebar is visible, ranges are the 100-row blocks touched by its viewport, at most three in total; `select_channel` initially requests `[0, 99]`, and scrolling releases obsolete blocks. Milestone 8 clears the member viewport and only requests the current user's member when needed for permissions. Navigating changes only what differs, in one frame, and releases the old guild explicitly. Rapid changes are coalesced: the first change opens a 250 ms window and everything changed within it is sent as one frame; setting what is already sent sends nothing.

Desired state (`SubscriptionTarget`, set through `Gateway::subscriptions()`) is kept apart from the last transmitted session state. A new READY clears that ledger; a RESUMED retains it, explicitly releases guilds removed while disconnected, and resends complete wanted entries because server-side retention is not established. Nothing is sent before READY/RESUMED. Subscription frames count against the 120-per-60 s ceiling; scheduled heartbeats have priority and optional traffic reserves enough slots for HELLO's heartbeat interval plus 20 other control frames. When only the reserve is left, sending is deferred (retried every 5 s) and the latest target wins. Frames are split before the 15 KiB outbound limit; one guild at its caps is about 5 KiB.

### GUILD_MEMBER_LIST_UPDATE
`{id, guild_id, member_count, online_count, groups: [{id: "online"|"offline"|role_id, count}], ops: [...]}`. `id` is `everyone` or a hash of the channel's permission overwrites; the event does not name a channel, so lists are keyed by `id`. Operations address positions in the **whole flattened list** (section headers count as rows):

- `SYNC {range: [s, e], items}`: the rows of `s..=e` (fewer items than the span means the list ends there).
- `INSERT {index, item}` / `DELETE {index}`: insert or remove one row; every later row, known or not, shifts by one.
- `UPDATE {index, item}`: replace one row.
- `INVALIDATE {range}`: the server no longer maintains `s..=e`; those rows are forgotten, nothing shifts.

An item is `{group: {...}}` or `{member: {user, roles, nick, ...}}` (presence is ignored). Since only subscribed ranges are described, rows are held sparsely as segments of known rows; an INSERT or DELETE moves later segments, touching segments join, and nothing outside what the server told us is invented. A row that cannot be read keeps its index (`Unreadable`); an operation name this client does not know empties the list's rows (counts kept) because it can no longer be trusted. Positions past `u32::MAX` are dropped, and a list never holds more than 1,000 known rows.

Member lists are accepted only for the selected guild and the selected channel's expected list ID; navigation releases old rows and late updates for another permission list are ignored. The expected ID follows [discord.py-self's `GuildChannel.member_list_id`](https://github.com/dolfies/discord.py-self/blob/master/discord/abc.py): `everyone` if the guild's default role grants VIEW_CHANNEL and no overwrite denies it, otherwise the unsigned MurmurHash3 x86-32 (seed zero) of lexically sorted `allow:<id>`/`deny:<id>` view overwrites joined by commas. An incremental event cannot introduce a list before its first SYNC. Rows outside the current subscription ranges are released on viewport changes.

### Events the reducer consumes
READY and READY_SUPPLEMENTAL (replace/extend state; a READY drops everything the session derived), GUILD_CREATE (a guild already held keeps its members, voice, and unread markers), GUILD_DELETE (outage keeps only the ID), CHANNEL_CREATE/UPDATE/DELETE (DM recipients move to the user table), MESSAGE_CREATE (only the channel's last-message marker, never backwards), PASSIVE_UPDATE_V2 (unread markers, voice states, members of guilds not subscribed; ignored for guilds not held), GUILD_MEMBER_ADD/UPDATE/REMOVE, VOICE_STATE_UPDATE (guild voice only), GUILD_MEMBER_LIST_UPDATE. Missing member/user fields in GUILD_MEMBER_UPDATE preserve old values; explicit null/empty fields clear them. READY's deduplicated users remain available until supplemental members acquire their references. Gateway sequence filtering discards replayed duplicates before the reducer.

### Retention (SPEC §4.5)
One `Store` is the only writer. Sizes conservatively account for owned String/Vec capacities, hash-table allocation slack, and auxiliary identity/focus tables against a 12 MiB budget. Channel metadata uses compact sorted vectors rather than large hash-table capacity overhead. Above the budget the store sheds toward 80%: nonvisible lists, unreferenced users, then unreferenced members (oldest write first, a user goes with its last member); optional maps are compacted so eviction releases backing memory. Identity and permission data, the current user's own member, voice/requested members, and visible rows are not selectively shed. If required data alone cannot fit, the reducer releases the account store and latches `limit_exceeded()`; the app drops the Gateway and displays terminal `StateTooLarge`. It never displays a silently truncated account as connected.

### Not established (verify live, see docs/TESTING.md)
Whether `typing: true` is really the subscription flag and whether it is needed for `channels` to take effect; whether omitted fields clear anything (irrelevant to correctness, since none are omitted); which subscriptions the server applies implicitly before any opcode 37; whether subscriptions survive a Resume; whether a voice-guild-only subscription keeps voice states fresh while another guild is selected (the reducer handles VOICE_STATE_UPDATE for it and PASSIVE_UPDATE_V2 for the rest); the real member-list ids for channels with overwrites.

## Guild/channel navigation (milestone 8)

Navigation is a reducer-owned ID selection, not a new REST discovery path. READY and the existing guild/channel/member events provide the metadata; GUILD_ROLE_CREATE/UPDATE/DELETE also update the retained role table and effective permissions. Guild outages remain visible but disabled. Text, announcement, and voice channels with VIEW_CHANNEL appear in deterministic category/position/ID order; categories are labels, not openable channels. Voice rows without CONNECT are visible but disabled. Opening a voice row selects it without sending a voice join.

Permission checks use the model's owner/admin and everyone/role/member overwrite algorithm. Incomplete owner/member/role metadata fails closed (a positively identified guild owner can bypass); an omitted or null member `roles` field is not proof of an empty role set. Active communication timeouts remove voice/send permissions. Changes reconcile the selected IDs and remembered channel per joined guild; hidden, removed, or newly forbidden channels cannot remain open. Selection is revalidated on the reducer even if a click originated in an older UI window.

The UI receives only two bounded row windows (at most 128 rows each, names capped to 256 UTF-8 bytes) and the selected channel's permission summary. Navigation controls are watch-coalesced; replaceable snapshots occupy one shared latest slot and at most one unconsumed notification. Fixed 32-pixel rows use exact top/bottom spacers and one viewport of overscan on each side, never one widget per cached channel. No full Store clone reaches the UI, no periodic redraw is subscribed, and no navigation action fetches message history (history paging is milestone 9).
