# fastcord Technical Specification

## 1. Decision

Build fastcord as a single-process, native Rust desktop application using Rust 1.99.0 stable, the MSVC target on Windows, and iced 0.14 with its wgpu renderer. Separate Discord protocol/state, real-time audio, video/capture, platform integration, and presentation into a small acyclic Cargo workspace. Use a direct user-account Gateway/REST implementation rather than adapting a bot framework; a bounded, single-writer state store; native UDP/RTP media with both required transport AEAD and DAVE; `davey` for the DAVE/MLS implementation; `opus2` with bundled libopus; `cpal` for microphone/playback; and `nnnoiseless` for microphone noise suppression. Use OS screen-capture APIs, H.264 as the interoperable send baseline, OS-native video codecs (Media Foundation, VideoToolbox, VA-API) plus `dav1d` for AV1 decode (no FFmpeg; see `docs/adr/0001-video-codecs.md`), and iced shader rendering for video. All networking, decoding, and audio processing stay outside the UI thread. Ship Windows, Linux, and macOS artifacts through GitHub Actions, with bounded caches and event-driven rendering designed to use fewer resources than the official Discord client under equivalent workloads (the hard 150 MB RAM limit was removed on 2026-10-21).

## 2. Scope, evidence, and invariants

### 2.1 Required MVP

- Guild and text-channel navigation; history; manually sending messages; editing/deleting one's own messages; replies; viewing attachments.
- DMs and group DMs, including listing/opening conversations and the same message operations.
- Voice channels: join, leave, microphone mute, deafen, per-user local volume, and speaking indicators.
- Input/output audio-device selection.
- Open-source microphone noise suppression.
- Screen-share sending and watching, including optional captured application/system audio.
- Unicode and custom emoji, an emoji picker, and reactions.

No selfbot automation, commands, scheduled messaging, scraping, mass-DM functionality, plugins, camera capture, moderation tools, guild administration, or attachment uploading. Viewing an unsupported attachment type means showing its metadata and providing an explicit open/download action, not claiming an inline decoder exists. Existing accessible text threads can reuse the message view and REST routes; creating/managing forums or threads is not an MVP feature.

Only one account is active at a time. A user session has one parent voice connection; Go Live connections are separate. Starting a share and watching a share must not replace or disconnect the parent voice connection. Resource-pressure handling must be visible rather than silently dropping active participants or pretending a feature works.

### 2.2 Evidence status

At drafting, the repository inspection found only `LICENSE`, whose opening identifies the GNU GPL version 3. There was no existing application architecture to preserve, and every path in this specification other than `LICENSE` described a proposed implementation. That is historical design evidence; current implementation and verification status lives in `docs/TESTING.md` (per-milestone offline and manual results, and pending live and hardware gates), `docs/PROTOCOL.md` (wire behavior actually implemented), and `CHANGELOG.md`.

Protocol facts cited from official Discord documentation are stronger evidence than user-client behavior documented by Discord Userdoccers or observed in `discord.py-self`. Community-documented behavior is explicitly a compatibility dependency, not a public API guarantee. Design budgets and acceptance thresholds below are requirements, not measured results. At drafting, no implementation benchmark or live-account interoperability check had been performed for this specification. Results recorded since then, and the live or hardware checks still pending, are in `docs/TESTING.md`; the budgets and thresholds remain requirements whatever those results show.

### 2.3 Non-negotiable invariants

1. Every outbound user action originates from an explicit UI action, except necessary connection maintenance, subscriptions, and media transport.
2. Authentication tokens, voice tokens, DAVE keys, and signed attachment URLs never enter normal logs, fixtures, crash reports, or URLs sent to unrelated services.
3. Never send unencrypted media in a session requiring DAVE; transport encryption does not replace E2EE.
4. Callback threads never wait on locks, allocate, perform network/file operations, or run neural inference.
5. Queues and caches are byte-bounded. A message count alone is not a memory budget.
6. Socket reconnection must not duplicate messages, react twice, reuse a transport nonce, or apply stale media state to a new session.
7. Permission checks inform the UI, but server responses remain authoritative.
8. Idle means logged in, no call/share, no input, and no ongoing animation. That state must have no periodic UI redraw loop, audio callback streams, or capture/codec workers.

## 3. Cargo workspace layout

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
LICENSE
README.md
CHANGELOG.md
crates/
  fastcord-model/src/       # IDs, normalized entities, commands/events, permissions
  fastcord-discord/src/     # REST, main Gateway, reducers, bounded state
  fastcord-media/src/       # voice WS, UDP, RTP/RTCP, transport AEAD, DAVE adapter
  fastcord-audio/src/       # cpal, resampling, Opus, denoise, jitter, mixing
  fastcord-video/src/       # capture orchestration, codecs, packetization, frame pools
  fastcord-video/native/    # pinned dav1d source archive + provenance; built by build.rs
  fastcord-platform/src/    # credential store, OS capture/audio, native handles
  fastcord-app/src/         # binary, iced UI, orchestration, settings, image cache
assets/emoji/              # versioned index and attributed assets
packaging/                 # platform launchers, icons, Info.plist, bundle layouts
scripts/                   # maintained CI/package/performance entry points
fixtures/                  # sanitized protocol/media test vectors
.github/workflows/
  ci.yml
  release.yml
docs/
  SPEC.md
  PROTOCOL.md
  PERFORMANCE.md
  TESTING.md
  RELEASE.md
  THIRD_PARTY_NOTICES.md
```

Dependency direction:

- `model` has no application-crate dependencies and no GUI/runtime dependency.
- `discord -> model`.
- `media -> model`; contains no cpal, iced, capture, or video decoder dependency.
- `platform -> model` where common IDs/settings are needed; never depends on iced.
- `audio -> model, media, platform`.
- `video -> model, media, platform`; it does not depend on `audio`.
- `app -> discord, audio, video, platform, model` and coordinates stream-audio routing through bounded channels.

Keep OS-specific modules inside `platform`, rather than creating a crate per operating system. `media` exposes session handles and owned encoded-frame buffers, not UI widgets. `video` exposes decoded-frame leases and capture status, not a renderer instance. iced/wgpu interop belongs to `app` so codec workers never acquire the UI's device directly.

Use Tokio for control-plane I/O, `reqwest` with rustls for REST/CDN, and a maintained Tokio WebSocket implementation. Use `serde` with typed payloads and explicit handling of unknown fields/events. Pin Rust and iced in manifests; commit `Cargo.lock`; pin native sources, build configuration, and hashes. Exact transitive versions are recorded by the lockfile, not floating documentation claims.

Use Rust edition 2024 and Cargo resolver 3. Release builds use thin LTO and stripped application symbols, with separate symbols retained as CI artifacts. Do not select panic-abort merely to hide thread cleanup problems. Target-specific dependency features must prevent unrelated backends from loading or initializing.

## 4. Main Gateway and state cache

### 4.1 Transport and compression

Use main Gateway API version 10, JSON encoding, and `compress=zlib-stream`. This is separate from the voice Gateway version. Discover the URL through `GET /gateway`; retain `resume_gateway_url` from READY for resuming. A main connection owns its own persistent zlib decompressor. Reassemble WebSocket fragments before processing, accumulate compressed data until the `00 00 ff ff` flush suffix, and retain the dictionary across events. Reset it only when opening a new connection. Transport compression means Identify's payload-compression flag is false; do not enable both compression schemes. [S1, S3]

`zstd-stream` is documented, but is not selected for the MVP: it needs a persistent zstd stream, and a WebSocket message does not terminate a zstd frame. Do not treat each message as an independent compressed frame. Keep the compression enum internal with only the shipped zlib variant; do not add an unused second implementation. [S1]

Process READY without retaining a duplicate untyped JSON tree. Use a streaming/visitor path for large arrays. Start with a 16 MiB uncompressed event safety ceiling, reuse decompression scratch, and release unusually large scratch after startup. This is a client safety limit, not a claimed Discord inbound limit. Exceeding it must produce a diagnosable startup error, not silently truncate account state. Outbound Gateway payloads must respect the documented 15 KiB limit. [S1]

### 4.2 Lifecycle

Use an explicit connection state machine: `Disconnected -> Connecting -> AwaitHello -> Identifying/Resuming -> Ready -> Reconnecting`, plus terminal `AuthenticationRequired` and terminal `Stopped(reason)` for conditions reconnecting cannot fix (payload rejected, too many sessions, an account action Discord requires, repeated Invalid Session, an event over the safety ceiling, undecodable READY). `Disconnected` is the absence of a running Gateway handle; the handle's first event is `Connecting`. Implemented in `fastcord-discord::gateway`; wire details and evidence status are in `docs/PROTOCOL.md`.

- Start heartbeat scheduling from HELLO's interval; jitter the first heartbeat, send the latest dispatch sequence (or null), answer server heartbeat requests immediately, and require ACK progress.
- On a missed ACK, close and reconnect; never keep displaying the connection as healthy.
- Store sequence, session ID, and resume URL in memory. Resume with opcode 6, not another Identify.
- Handle opcode 7 Reconnect and opcode 9 Invalid Session according to their semantics and close codes; use bounded exponential reconnect delay with jitter to avoid an outage loop.
- Authentication failure stops automatic attempts and returns to login. A fresh Identify replaces session-owned state; a successful Resume applies replay to existing state.
- The reducer applies events in sequence and makes duplicate creates/deletes harmless. It merges partial MESSAGE_UPDATE payloads rather than replacing missing fields with defaults.
- Respect the documented Gateway send ceiling of 120 commands per 60 seconds; budget control traffic first, coalesce subscriptions, and never let UI churn starve heartbeats. [S1]

### 4.3 User Identify, intents, and capabilities

Use the raw user token, without `Bot ` or `Bearer ` prefixes. User accounts do not require bot intents; omit `intents`. Never request privileged bot intents as a substitute for implementing user-client state. Capabilities are a separate field. [S1]

Select named flags, not a copied magic integer:

- `LAZY_USER_NOTES`.
- `NO_AFFINE_USER_IDS`.
- `DEDUPE_USER_OBJECTS`.
- `PRIORITIZED_READY_PAYLOAD`, together with its deduplication prerequisite.
- `PASSIVE_GUILD_UPDATE_V2`.

Implement the corresponding normalized `users`, `merged_members`, READY_SUPPLEMENTAL, and passive-update shapes before enabling them. Do not enable protobuf user settings, reaction debouncing, client-state-v2, or token-refresh capabilities without their handlers. Send an empty guild-version cache on a cold login rather than claiming cached versions that are not persisted. [S1, S3]

Identify properties use a centralized, versioned **web-client profile** (ADR 0005): `browser: "Chrome"`, a matching `browser_user_agent`/`browser_version`, truthful `os`/`os_version`/`system_locale`, `release_channel: "stable"`, a per-launch `client_launch_id`/`launch_signature`/`client_app_state`, and a current `client_build_number`. The build number is fetched from discord.com web assets at login (cached with a timestamp) with the value bundled at release time as fallback. The profile is built once per launch and stays stable across reconnects. The same profile is the REST client's identity: it is sent base64-encoded as `x-super-properties` together with its user agent and locale, so the Gateway and REST halves never disagree. Profile version 2 omits `x-fingerprint` instead of reusing a captured identifier across all accounts/installations; no replacement is fabricated. **UNKNOWN U1:** server acceptance of this profile with the selected capability set is not established. Milestone 6 must record a sanitized accepted payload and stop on a server challenge; this spec does not authorize CAPTCHA bypass. **UNKNOWN U1b:** the official web client negotiates voice through WebRTC, whereas fastcord uses the native UDP voice protocol; whether a web profile plus `protocol: "udp"` is accepted or flagged must be verified in milestone 17 on the alt account.

### 4.4 Lazy subscriptions

Use opcode 37, `{"subscriptions": {guild_id: subscription}}`; opcode 14 is the deprecated predecessor and is not a permanent fallback. Track desired subscriptions separately from the last transmitted set. [S2, S3]

- Subscribe to the selected guild and the guild containing the active voice channel.
- Request member-list ranges only for the visible channel/sidebar viewport, rounded to protocol range blocks; begin with one 100-entry range and no more than three live ranges per visible list.
- Request individual members only when required for visible authors, reply targets, voice participants, or permission computation; evict unreferenced members later.
- Do not enable activities or thread subscriptions, and never decode or show typing indicators: none is an MVP requirement. Per the cited community documentation (S3) the wire `typing` flag is what subscribes the connection to a guild, so it is `true` for a subscribed guild (and `false` only to release one); it grants no typing UI.
- Remove unused ranges on navigation; coalesce rapid selection changes.
- Implement `SYNC`, `INSERT`, `UPDATE`, `DELETE`, and `INVALIDATE` member-list operations with index-correct behavior.
- Consume passive guild updates so unsubscribed guild metadata does not become permanently stale.

**UNKNOWN U2:** exact unsubscribe/partial-update semantics for the current server must be captured in the opcode-37 fixture and verified with two guilds, including keeping voice membership fresh while browsing another guild. Do not assume an omitted field clears a subscription: milestone 7 therefore sends every field explicitly for every changed guild. The recorded fixture is what fastcord sends; the live capture of the server's behavior is still outstanding (docs/TESTING.md). [S2, S3]

### 4.5 State ownership and budgets

One reducer task owns mutable normalized state. IDs are `u64` internally and serialized as strings as required. Maps store entities once; channel/message/member references use IDs, not copied nested objects. UI snapshots are small immutable deltas or shared read models, not clones of all guild state.

Initial steady-state caps:

| Store | Cap and retention rule |
|---|---|
| Guild/channel/role metadata and normalized users | 12 MiB; preserve navigable guild/channel identity and required permission data, shed optional detail first |
| Message bodies and parsed presentation data | 16 MiB; max 500 messages per hot channel, max 2,000 globally, byte cap wins |
| Cold-channel markers and bounded pending state | 4 MiB; IDs/cursors, not full histories |
| Current visible member ranges | Included above; release after navigation unless pinned by voice/visible messages |
| Queued Gateway/UI deltas | 2 MiB; coalesce replaceable changes, never silently lose ordered mutations |

The metadata reducer charges owned allocation capacities, not just element counts. If required identity/permission or pinned state alone exceeds the safety ceiling after optional eviction, stop the account connection with a visible diagnostic and release the store; never silently truncate a connected account or let mandatory data grow without a cap.

A cache miss refetches the required history or entity. No persistent plaintext message database in MVP. Local settings may store device IDs, volume overrides, recent emoji, and layout, not tokens. Purge account-specific state and cache namespaces on logout.

## 5. REST and CDN

### 5.1 Rate-limit scheduler

Use `/api/v10` and one authenticated REST client per active account. Route keys are `(HTTP method, normalized route, major parameter)`. Learn `X-RateLimit-Bucket`, but retain major resource IDs in bucket identity because the bucket hash excludes them. Read limit, remaining, and fractional reset-after headers; schedule against monotonic time. On 429, honor `retry_after`/`Retry-After` and the global flag/scope. A global pause applies to all authenticated routes. Do not hard-code the documented bot 50-requests/second value as a user entitlement. [S4]

Reserve capacity before dispatching concurrent requests. Unknown routes start conservatively with one in-flight request per route key. Prioritize user writes above speculative reads. A 401 stops authenticated work, a 403 updates the affected permission/error state, and a deleted-resource 404 invalidates that resource. Do not retry these statuses blindly.

Message send uses a stable local operation ID and supported nonce/enforce-nonce fields, reconciled against REST responses and MESSAGE_CREATE. An ambiguous network failure must not trigger an unconditional duplicate POST; show an uncertain/pending state, reconcile, and let the user explicitly retry where necessary. [S5]

### 5.2 Endpoint-to-feature mapping

| Feature | REST/Gateway operation |
|---|---|
| Validate login | `GET /users/@me` |
| Gateway discovery | `GET /gateway` |
| Guild list | READY/GUILD_CREATE; `GET /users/@me/guilds` only when needed for reconciliation |
| Guild channels | READY and channel events; `GET /guilds/{guild}/channels` for explicit refresh |
| Channel metadata | `GET /channels/{channel}` |
| Message history | `GET /channels/{channel}/messages?limit=50&before={id}`; use `around` for a reply jump |
| Reply target | `GET /channels/{channel}/messages/{message}` if not already cached |
| Send/reply | `POST /channels/{channel}/messages`; replies carry `message_reference` and explicit mention policy |
| Edit own message | `PATCH /channels/{channel}/messages/{message}`; preserve existing attachments when required |
| Delete own message | `DELETE /channels/{channel}/messages/{message}` |
| DM/group-DM list | READY private channels and `GET /users/@me/channels` |
| Open/create DM or group DM | `POST /users/@me/channels` with explicit `recipients` selected by the user |
| Guild custom emoji | READY/guild emoji updates and `GET /guilds/{guild}/emojis` when needed |
| Add normal reaction | `PUT /channels/{channel}/messages/{message}/reactions/{emoji}/@me` |
| Remove own normal reaction | User-client route `DELETE /channels/{channel}/messages/{message}/reactions/{emoji}/0/@me`; pin a compatibility fixture for this typed route |
| Reaction detail | `GET /channels/{channel}/messages/{message}/reactions/{emoji}` on demand |
| Friends and blocks | READY `relationships` and `RELATIONSHIP_ADD/UPDATE/REMOVE`; `PUT /users/@me/relationships/{id}` (accept, send by ID, block with `{"type":2}`), `DELETE` of it (decline, cancel, remove, unblock), `POST /users/@me/relationships` (send by username); one explicit request per click |
| Presence | READY `presences`, READY_SUPPLEMENTAL `merged_presences.friends`, `PRESENCE_UPDATE`; own status via `PATCH /users/@me/settings` (`status`, `custom_status`); admitted only for referenced users |
| User profile | `GET /users/{id}/profile` on demand when a name or avatar is clicked |
| Attachment refresh | `POST /attachments/refresh-urls`, or refetch owning message to obtain a current URL |
| Voice and Go Live | Main Gateway signaling, not REST polling |

The user-client typed delete-reaction route differs from the commonly shown bot-compatible untyped route; implement one tested current route, not speculative fallback requests. Percent-encode Unicode reaction strings and custom `name:id` strings. Group-DM creation is one manual action, never an account-discovery or bulk-creation loop. [S5, S6, S7]

### 5.3 CDN/media cache

Use a separate unauthenticated HTTP client for CDN resources. Do not forward `Authorization` on cross-origin redirects or to CDN hosts. Preserve signed attachment query parameters for actual fetches; store a stable logical attachment key independently. Refresh expired URLs through the authenticated API, not by modifying signatures. [S7]

- Disk LRU: 256 MiB total, account-scoped for private attachments; compressed content only, atomic writes, no cache of arbitrary external embeds without an explicit request.
- Decoded image/emoji CPU cache: 12 MiB; key includes asset identity, decode dimensions, and animation frame.
- UI texture/glyph budget: target 16 MiB resident allocation, measured separately from process RSS where drivers keep separate GPU memory.
- At most four concurrent image fetches and two decodes; decode off the UI thread and cancel off-screen speculative work.
- Clamp decoded dimensions/pixel count before allocating. Decode thumbnails to display size, not full camera resolution.
- Avatars and server icons (`cdn.discordapp.com/avatars|icons/{id}/{hash}.png?size=64`, default avatars `embed/avatars/{(id>>22)%6}.png`; `a_` hashes use the static PNG) share the fetcher, disk LRU and decode slots; decoded results up to 128 KiB are also kept as raw sized RGBA in a separate persistent per-account store (`images/<account>` beside the attachment cache; name = kind, id, CDN hash, box; 64 MiB, LRU by last use, trimmed every 32 writes; purged with the account), so a later launch or server switch is one small file read with no network, decode or resize; the UI keeps at most 256 decoded handles (24 MiB of pixels in all, which also holds embed pictures), requested only for the rows on screen, with the letter circle as fallback.
- In-app attachment view supports images and common media formats included in the media build; other files expose type, name, size, and user-driven open/download. Inline playback (#74, #116) covers Ogg Opus audio (mono/stereo, 20 ms packets) through `fastcord-audio`'s `ogg` demuxer and `file_player`, and MP4/WebM files with H.264 or AV1 video and Opus audio through the bounded demuxers in `fastcord-video::container` (ADR 0007), the existing video decoders and the latest-frame playback path, with the audio clock as master. AAC-LC audio (mono/stereo) is decoded by the pure-Rust `symphonia-codec-aac` and re-encoded to 20 ms Opus packets for the same engine path (ADR 0008); HE-AAC and multichannel AAC play the video silently with a note, and VP8/VP9/HEVC video, MP3, FLAC and every other format remain Download/Open-only. Voice messages (#75): the player draws the attachment's `waveform`/`duration_secs` and recognizes the `1 << 13` message flag; recording (`fastcord-audio::recorder`, an input-only engine plus the Ogg Opus writer, preview through the file player, upload through the ordinary cloud-upload path with the voice flag) is an off-by-default compatibility-gated extension because official recording is mobile-only; see PROTOCOL.md "Voice messages (#75)".
- Animated assets run only while visible and the window is active. Their timers stop when not needed.

## 6. Voice transport and DAVE

### 6.1 Session setup

Use voice Gateway **version 8**, explicitly in the URL. Official documentation recommends it and defines buffered resume. Do not conflate main Gateway v10 with voice Gateway v8. [S8]

1. Main Gateway opcode 4 requests the selected channel and mute/deaf state.
2. Correlate this join attempt with both the current user's VOICE_STATE_UPDATE/session ID and VOICE_SERVER_UPDATE/endpoint+token; either event may arrive first.
3. Open a distinct secure voice WebSocket; Identify with server ID, user ID, session ID, voice token, and the highest DAVE version actually supported by the pinned implementation.
4. Heartbeat from voice HELLO. In v8 include `t` and `seq_ack`; track the last numbered server message, including binary messages.
5. READY supplies the UDP endpoint, SSRCs, and offered transport modes. Open a UDP socket and perform IP discovery.
6. Send Select Protocol with the discovered external address/port, selected mode, and actual codec capabilities.
7. Wait for Session Description, establish transport key and DAVE state, announce speaking state before audible RTP, and only then release queued media.

A join attempt owns a generation ID so late events from a previous join cannot overwrite it. Leaving sends channel-null state, stops local capture/output as appropriate, drops sockets and keys, and cancels workers. Moving channel is a new handshake even if the endpoint string is unchanged; never reuse the old voice token or crypto context. [S8]

### 6.2 UDP, RTP, RTCP, and transport AEAD

IP discovery is a 74-byte packet: big-endian type 1 request/type 2 response, length 70 excluding the four-byte prefix, SSRC, 64-byte address field, and a two-byte port. Validate response source and SSRC. A blocked UDP route is a visible connection failure; this MVP does not promise a TCP/WebRTC relay fallback. [S9]

Audio uses Opus at 48 kHz. Use stereo wire configuration as documented, with mono microphone denoising mapped into the encoder's channel arrangement. Default to 20 ms packets: 960 RTP timestamp ticks at 48 kHz. Maintain wrapping 16-bit sequence numbers and 32-bit timestamps; parse CSRC count, padding, extension preamble, extensions, and negotiated payload types instead of assuming every RTP header is 12 bytes. [S8, S9]

Support:

- `aead_aes256_gcm_rtpsize`, preferred when offered and efficient on the host.
- `aead_xchacha20_poly1305_rtpsize`, mandatory fallback when AES mode is not offered.

Do not implement deprecated XSalsa modes. Transport keys are 32 bytes. Build the clear authenticated header according to rtpsize semantics: fixed header, CSRCs, and the extension preamble; extension elements belong to the encrypted body. Append the four-byte incremental nonce suffix after encryption, reconstruct the mode-specific nonce on receipt, and strip the suffix before AEAD verification. Use established RustCrypto AEAD implementations and cross-implementation vectors, including extended RTP headers. Stop/rekey before a transport nonce would repeat; RTP sequence wrap is not a crypto-nonce reset. RTCP needs its own correct header/body boundary, not an RTP parser reused blindly. [S8, S9]

Demultiplex discovery, UDP ping, RTP, and RTCP before media decode. Track user-to-SSRC mappings from Speaking/Video events. Unknown SSRC buffering is short and bounded (100 ms); never route it to an arbitrary user. Parse sender/receiver reports; audio loss informs jitter/FEC and video additionally uses feedback/RTX described below.

### 6.3 Opus implementation

Use `opus2` 0.4 with `backend-libopus`, `static`, and `bundled`; its documented native build requires a C compiler and CMake or a supplied libopus package. Do not select its optional pure-Rust codec backend without separate quality/interoperability acceptance. Store one encoder per outgoing audio source and one decoder per active incoming audio SSRC. Preallocate encode/decode buffers. Begin with 64 kbit/s microphone audio, constrained by channel/server limits; enable in-band FEC and adapt packet-loss expectations from observed loss. [S10]

### 6.4 DAVE is mandatory

Official Discord documentation says E2EE is required from March 1, 2026 for DM/GDM calls, voice channels, and Go Live. DAVE is not an optional later milestone and a transport-only voice demo does not satisfy voice acceptance. [S8]

Select **`davey` (latest 0.1.x, pinned) as a direct Rust DAVE implementation built on OpenMLS** (ADR 0003). It is not a binding to official libdave. Production evidence: `@discordjs/voice` ships `@snazzah/davey` as its only supported DAVE library, and discord.py-self uses it. Use it through a narrow internal `DaveSession` adapter, without optional Python or Node bindings. Its documented codec enum includes Opus, H.264, VP8, VP9, and AV1. Pin the exact version and run official-protocol/reference interoperability vectors. [S11]

Official **libdave** is the reference implementation and test oracle. It is C++ with MLS++ and OpenSSL/BoringSSL dependencies; a Rust integration would require a C-ABI wrapper, ownership-safe handles, and pinned native build recipes. Do not claim `davey` provides that wrapper. Do not ship two DAVE engines in parallel. If the chosen engine fails an acceptance vector, fix/update it or make an explicit one-engine cutover; do not silently fall back to unencrypted transport. [S12]

The media session owner handles JSON and binary voice opcodes. Implement the DAVE transition/epoch and MLS external-sender/key-package/proposal/commit/welcome sequence (opcodes 21–31), including invalid-commit/welcome recovery. Server binary messages have a two-byte sequence prefix plus opcode; client binary messages do not copy that server prefix. Prepare keys before transition-ready, switch send state only on execute-transition, and retain only the protocol-permitted receive grace state. Membership changes, reconnects, and departed-user key removal must be tested. [S8, S13]

Correct transform order:

```text
send: PCM/video pixels -> codec frame -> DAVE frame encryption
      -> codec RTP packetization -> per-packet transport AEAD -> UDP
receive: UDP -> transport AEAD verification -> RTP repair/depacketization
         -> DAVE frame verification/decryption -> codec -> playback/render
```

DAVE operates on a complete encoded video frame, not on arbitrary UDP fragments. Never use the voice transport secret as an MLS key. Do not log or persist MLS state. If keys are not ready, pause local media with bounded queues and visible connecting/rekeying status.

## 7. Audio processing, devices, and suppression

### 7.1 Threading

`cpal` owns OS callback streams; callbacks run on dedicated/high-priority threads on modern platforms. They are not Tokio tasks. A separate audio engine thread owns resamplers, the denoiser, Opus state, receive decoders, and mixing. Network tasks move pooled encoded packets through bounded channels. [S14]

Pipeline:

```text
cpal input callback -> preallocated SPSC PCM ring
 -> resample/downmix to 48 kHz mono
 -> 480-sample denoise blocks -> accumulate 960-sample Opus frame
 -> encode -> media session

media session -> per-SSRC encoded jitter queue
 -> Opus/FEC/PLC decode -> local per-user gain -> stereo float mix/limiter
 -> output resampler -> preallocated SPSC ring -> cpal output callback
```

Use preallocated rings with approximately 40 ms normal buffering and a hard 100 ms cap. On an output underrun emit silence and count it; on capture overrun discard the oldest stale input rather than increasing latency indefinitely. Session teardown and device replacement happen on the owner thread. The owner and the High quality denoise worker request real-time scheduling (`fastcord-platform::realtime`: MMCSS on Windows, Mach time-constraint policy on macOS, `SCHED_RR` on Linux) and cpal promotes the device callback threads (`realtime` feature); a refusal is never fatal and shows in `EngineStats` (`realtime_threads`, `callback_realtime_denied`). The owner ticks every 5 ms on a high-resolution sleep with fixed deadlines (not `recv_timeout`, which waits at the 15.6 ms system tick on Windows) and counts wake-ups more than 10 ms late (`late_ticks`); the engine, and so the tick, exists only during a call or a microphone test.
On a fatal device error, close only the failed direction and its endpoint; discard queued playback packets and terminate the capture channel while leaving the healthy direction running. Bound playback packet attempts per owner pass so malformed packets cannot stall teardown. Resampling uses a 256-tap Blackman-Harris sinc filter that preserves the 20 kHz passband between 44.1 and 48 kHz, and integer device output saturates at its representable limits.

### 7.2 Jitter, mixing, and controls

- Per-SSRC adaptive jitter target: 40 ms initial, 20–120 ms range; estimate interarrival jitter from timestamps.
- Reorder packets and discard duplicates/too-late packets. Use Opus in-band FEC when the next frame is available; otherwise use PLC.
- Keep encoded queues bounded, including unknown SSRCs; release inactive decoder state after a short inactivity interval while retaining required DAVE member state.
- Mix in f32 without clipping intermediate sums; apply a final limiter: a 0.25 s peak follower sets the gain target (0.9), the gain falls with a 3 ms time constant and recovers over 0.25 s, and a soft clip bounds what the attack lets through at 0.97, so loud speakers summed do not pump. Per-user volume is a local 0–200% gain, persisted by account/user ID, with short ramps to avoid clicks.
- Speaking indicators combine authoritative SSRC/user state with received voice activity/audio level; use a short release delay and no idle animation timer.
- Microphone mute stops microphone capture/processing and audible transmission; send the protocol silence tail as required when ending a talkspurt, then speaking=false.
- Deafen prevents playback and also mutes microphone transmission. Preserve the previous self-mute preference for undeafen. Exclude stream playback too; do not confuse deafen with stopping the other participant's stream.
- If the server mutes/deafens the user, reflect it distinctly and do not allow a local toggle to claim it overrode server state.

### 7.3 Device selection

Enumerate `cpal` devices and supported formats off the UI thread. Offer independent input/output selectors and explicit `System default`. Persist a stable backend ID where available, with a human label for display, not label-only equality. Convert supported sample formats and rates to/from the internal 48 kHz f32 representation.

On hot-unplug or device error, close the old stream and show the affected device state; use the default device only if the saved choice is `System default` or the user chooses it. Never silently switch a selected private headset output to speakers. Probe devices lazily when settings or a call requires them. [S14]

### 7.4 Noise suppression

Offer a setting with three modes (ADR 0004): **Off**, **Light** (default) — `nnnoiseless` 0.5.2, a Rust RNNoise implementation — and **High quality** — DeepFilterNet 3 via its Rust crate. Apply to microphone audio only. Both engines sit behind one `Denoiser` trait in `fastcord-audio`. The DeepFilterNet model and runtime load only when High quality is selected and are dropped when the user switches away, so Light/Off users pay no memory for it. `nnnoiseless`'s low-level API minimizes copies, maintains state across calls, and expects f32 samples scaled to the i16 numeric range: use 480-sample/10 ms blocks, handle the documented initial delayed output, and rescale afterward. DeepFilterNet adds lookahead latency; account for it in the §12.2 local-latency budget and show it in the setting description. Bypass completely when Off or microphone-muted. Never apply speech suppression to music/game/stream audio. [S15]

CPU cost is an estimate, not a result: target **1–5% of one modern x86 core** for the mono denoiser and **p95 <0.5 ms per 10 ms block** in a release build on the recorded reference Windows machine. The original RNNoise paper reports roughly 40 MFLOPS and 1.3% of a Haswell i7-4800MQ core for its non-vectorized C implementation; that number is not a benchmark of this Rust crate. Measure the exact shipped model/build. [S16]

DeepFilterNet is not the default because its CPU/RAM cost (estimated 5–15% of one core, +10–20 MB) conflicts with the low-resource goal; it is opt-in for users who need Krisp-like suppression of non-stationary noise (keyboard, voices). Record measured CPU, RAM, and latency for both engines on the reference PC in milestones 23 and 23a. Noise suppression is not acoustic echo cancellation. Echo is handled separately by `fastcord-audio::echo` (#114): a pure-Rust partitioned-block frequency-domain adaptive filter (320 ms, 5 ms hop) against the audio the output device actually played, run on the owner thread before the denoiser, on by default and switchable in the audio dialog. It is a linear canceller with double-talk protection and no residual-echo suppressor, so it needs a few seconds of speech to converge and does not promise speakerphone-quality AEC; a headset remains the recommendation.

### 7.5 Microphone processing order, gain and diagnostics (#83)

The capture path runs, on the engine owner thread and never in a device callback: device samples, downmix to mono, resample to 48 kHz, **echo cancellation** (when on, a call, and unmuted), **noise suppression** (Off/Light/High quality), **gain** (manual -20 to +30 dB plus optional automatic gain, with a peak limiter at 0.97), the **level meter**, the **voice-activity threshold** gate and Opus. Gain follows suppression so it never amplifies what the suppressor removes; the meter measures exactly the signal the threshold compares with, so what the meter shows is what the threshold decides. With 0 dB and automatic gain off the stage is bypassed and samples are untouched. Automatic gain steers block RMS toward -21 dBFS within -12 to +24 dB, attacks fast, releases at 5 dB/s and holds while the block is quieter than -55 dBFS, so silence and room noise are not amplified.

The echo canceller adapts at a rate set by how much of the microphone error is echo the filter missed (a correlation with its own estimate, as in Speex MDF) rather than by a converged/frozen state machine, with a floor so it keeps tracking the room; a filter whose estimate does not even appear in the microphone (the reference moved by an underrun, an unmute or a device change) is detected and re-adapted, and an unmute or push-to-talk press restarts fast adaptation for a second because the microphone stream restarts with an arbitrary phase against the reference. It adds no algorithmic latency (the estimate is subtracted from the current block), costs about 0.4% of one core (measured, release) and about 0.27 MiB, and does no work while muted or off (`EngineStats::echo_blocks`).

Diagnostics are read-only and cost nothing while the audio dialog is closed: the engine measures level and echo-canceller state only after `EngineControl::Diagnostics(true)`, publishes to lock-free atomics, and the app polls them at 10 Hz on a timer that exists only while the dialog is open, coalescing unchanged values. Outside a call the dialog offers **Check level** (a live meter on an input-only engine) and **Record and play back** (5 s kept in memory as about 20 KB of Opus frames, then played to the selected output; recording first and playing second rules out acoustic feedback). Neither is connected to a voice session, so neither can transmit to a call. Device and permission failures are named by direction and cause with the remedy (privacy settings for permission, another application for exclusive use, unplugged), and a microphone that is open but silent for three seconds is reported as such.

## 8. Screen sharing: send, watch, and stream audio

### 8.1 Go Live signaling

A share is not ordinary camera video on the parent voice connection. Remain joined to its host voice instance. [S9, S17]

- Start: main Gateway opcode 18 with `type=guild` or `call`, channel ID, guild ID when applicable, and any supported region preference.
- Watch: opcode 20 with the selected advertised `stream_key`.
- Correlate STREAM_CREATE and STREAM_SERVER_UPDATE by key. STREAM_CREATE provides the stream RTC server/channel IDs; STREAM_SERVER_UPDATE provides endpoint/token.
- Open a **separate** voice-v8 WebSocket, UDP transport, SSRC mapping, nonce state, and DAVE group. Use stream RTC IDs for Identify, not parent-guild IDs.
- Stream keys have documented guild/call forms; prefer the server-advertised key over rebuilding it from strings.
- Handle stream ping opcode 21 according to the current protocol profile; pause/resume with 22; end/unwatch with 19. Handle STREAM_DELETE reasons and server failover without dropping the parent call.
- Announce video/RTX SSRCs from READY using voice opcode 12. Viewers request the desired primary SSRC using voice opcode 15 Media Sink Wants; set unwanted/off-screen video to zero.
- Only the share owner sends stream media; watchers do not start a microphone on the stream connection.

**UNKNOWN U3, high risk:** community documentation currently describes the stream DAVE group identifier as the numeric media-session ID equal to `rtc_server_id - 1`. This is distinct from the textual `media_session_id` field used elsewhere in voice signaling. Isolate the derivation in the stream identity adapter, validate it with an official-client interoperability trace and MLS reference behavior, and never reuse the parent voice channel ID. This is a release gate, not a detail to guess. [S9]

### 8.2 Codec policy

Send **H.264** by default, a single screen layer at up to 1280×720 and 30 fps, using a low-latency profile with no B-frames. Start around 2.5 Mbit/s and obey negotiated/server limits and network feedback. A software encoder must remain available when hardware is absent. Higher quality is not required to claim the baseline works; do not display a quality choice that is not implemented.

Use OS-native codec APIs per platform (decision record: `docs/adr/0001-video-codecs.md`):

| Codec | Windows | macOS | Linux |
|---|---|---|---|
| H.264 encode | Media Foundation: hardware MFT preferred, Microsoft software MFT fallback | VideoToolbox (hardware, Apple software fallback) | VA-API; `openh264` software fallback |
| H.264 decode | Media Foundation with D3D11VA; software MFT fallback | VideoToolbox | VA-API; `openh264` software fallback |
| AV1 decode | `dav1d`; Media Foundation hardware AV1 where the GPU/driver exposes it | `dav1d`; VideoToolbox hardware where available | `dav1d`; VA-API hardware where available |
| VP8/VP9 decode | `libvpx`, only if live tests (U4) show Discord senders actually emit them | same | same |

FFmpeg is not used. Do not spawn codec child processes or pipe raw frames between processes. Keep decoder contexts lazy and create them only for the negotiated codec. [S18, S19]

Receive H.264 and AV1; add VP8/VP9 only when U4 evidence requires them. Implement each advertised codec's RTP depacketizer and DAVE transform selection; merely having a decoder is insufficient. Advertise encode/decode support separately, using only working paths. H.265 is not selected or advertised for MVP, even though community documentation now lists it. The server-selected payload type, RTX payload type, and codec are authoritative. [S9, S11]

**UNKNOWN U4:** the current server's negotiation with official desktop/mobile viewers, especially AV1 senders and H.264 profile selection, requires live interoperability tests. Test actual emitted/received codecs rather than assuming the codec preference list guarantees a selection.

### 8.3 Video transport

Use a 90 kHz video RTP clock and codec-specific packetization. For H.264 support single NAL, aggregation reception, and FU-A fragmentation; for VP8/VP9/AV1 implement their actual payload descriptors, frame boundaries, and reassembly. Apply DAVE before fragmentation and decrypt after complete frame reassembly.

Target UDP datagrams at or below 1200 bytes including RTP/extensions/AEAD/suffix overhead. Use bounded pacing instead of bursting a full keyframe at once. Announce assigned primary and RTX SSRCs, negotiated RID/header extensions, and actual active state. Support playout-delay metadata where the server profile requires it. Do not hard-code browser SDP payload types into the native UDP transport. [S9]

Implement RTCP sender/receiver reports, NACK/RTX, PLI/keyframe requests, and negotiated transport feedback. Keep a retransmit ring bounded by both 500 ms and 2 MiB; reconstruct RTX correctly and encrypt each retransmission with a fresh transport nonce. Missing/incomplete video frames expire after a bounded deadline; request a keyframe rather than feeding damaged arbitrary fragments to a decoder.

Use a small pacer queue capped at 100 ms for ordinary frames. An empty queue may admit one keyframe with up to 180 ms of paced data (below the 200 ms receive deadline), with unchanged pacing rate and burst credit; the normal bound returns when it drains. This prevents desktop IDRs just over the ordinary 78,125-byte budget at 2.5 Mbit/s from starving recovery. Initial congestion policy: reduce bitrate by 20% when receiver loss exceeds 5%, a backed-up pacer exceeds its cap, or RTT grows more than 100 ms above the running minimum; never interpret a frame too large for an empty queue as network congestion and shrink its allowance again. Increase slowly (at most 5% per stable interval) after several seconds below 2% loss. Honor a lower server/receiver bitrate request immediately. Lower resolution/frame rate before queueing stale frames. These values are design defaults to validate, not claims about Discord's proprietary congestion controller.

### 8.4 Capture backends

| OS | Screen/window capture | Stream audio | Required behavior |
|---|---|---|---|
| Windows | Windows.Graphics.Capture through `windows` bindings, user-selected window/display, D3D11 frame surfaces | WASAPI application loopback where available; explicit system-loopback mode otherwise | Check support; handle source close/resize and permission denial; show active capture state |
| Linux | xdg-desktop-portal ScreenCast -> PipeWire (`ashpd`/PipeWire bindings) | Explicit PipeWire source or sink-monitor selection; audio is separate from the screen portal | CreateSession, SelectSources, Start, OpenPipeWireRemote; honor session close and portal backend limitations |
| macOS | ScreenCaptureKit via maintained Rust/Objective-C bindings | ScreenCaptureKit audio capture, excluding fastcord where supported | Request OS permission, handle denial/revocation, use timestamped native samples |

Windows.Graphics.Capture provides display/window frames and user-facing selection controls. Windows application-loopback capture has a documented build-20348 minimum; on older supported Windows versions, clearly label system audio as broader capture rather than pretending per-application capture works. [S20, S21]

Linux screen capture requires a functioning portal/PipeWire desktop. Feature-detect portal interface versions; newer version-6 streams prefer `pipewire-serial`/`PW_KEY_TARGET_OBJECT`, whereas older versions provide node IDs. Do not claim the ScreenCast portal itself authorizes or returns desktop audio. For X11, use the portal backend if available; absence is a visible missing system prerequisite rather than silent fake capture. [S22]

Target macOS 13+ for the full audio/video capture baseline. Check each optional native method at runtime instead of adopting documentation for newer OS releases unconditionally. Supply the required bundle usage descriptions; local permission grants and release signing are part of packaged-app testing. [S23]

### 8.5 Hardware encoding and decoding

Select capabilities at runtime:

- Windows: Windows.Graphics.Capture D3D11 textures feed the hardware Media Foundation H.264 encoder directly (zero-copy, shared D3D11 device); decode through Media Foundation with D3D11VA surfaces.

  **Implemented (M32, send side).** `fastcord-platform` capture writes the video processor's NV12 output into a ring of three render-target textures on its own multithread-protected D3D11 device and, when the handler `wants_gpu`, passes the texture itself as a `GpuFrame` (device, texture, size, timestamp; valid during the callback). `fastcord-video` opens a hardware Media Foundation encoder only on that device's adapter (LUID match), checks `MF_SA_D3D11_AWARE`, hands it the same device through `IMFDXGIDeviceManager` (`MFT_MESSAGE_SET_D3D_MANAGER` before the media types) and feeds `MFCreateDXGISurfaceBuffer` samples. Chosen automatically (`ShareConfig::zero_copy`, `FASTCORD_ZERO_COPY=0` forces the copy path); not used with the software preference, the camera, a preview or a background effect, which read the bytes. The share reports `ShareEvent::ZeroCopy(Active | Copy(reason))`, `ShareStats::gpu_pictures`, and `(zero-copy)` in the encoder name. Failure of the GPU encoder (cannot open, `ProcessInput` error such as device removed or reset) drops that encoder, answers the same picture on the copy path (which opens its own encoder) with an IDR, shows `copy path: <reason>`, and never ends the share; it is not retried within that share. Known limit: if the capture's own device is removed, Windows.Graphics.Capture stops too and the share ends with the existing capture error (the copy path shares that device); rebuilding the capture device in place is not implemented.

  **Receive side: kept on the CPU NV12 upload.** iced 0.14 pins wgpu 27.0.1. Showing a D3D11VA surface without readback would need the decoder's texture shared (NT handle or keyed mutex) and imported into wgpu through `wgpu-hal` (`create_texture_from_hal`) on DX12 only; iced may run on Vulkan or the tiny-skia fallback, the decoder device and wgpu's adapter can differ on multi-GPU machines, there is no cross-API fence, the import needs `unsafe` in `fastcord-app` (workspace `unsafe_code = "deny"`) and a direct `wgpu-hal` dependency tied to iced's exact wgpu build. That is not safe enough for "where safe"; the decoder keeps its staging copy and the existing bounded lease and shader upload (milestone 28). Revisit when iced exposes an external-texture API.
- Linux: VA-API decode/encode where present; `openh264` software otherwise.
- macOS: VideoToolbox decode/encode with IOSurface-backed frames.

The deterministic preference is platform-native hardware, then another tested hardware backend, then software. A configured explicit backend that fails shows its failure; automatic mode may choose the software path and report that choice. Hardware names are candidates, not a guarantee that the current runner/GPU/driver exposes them. [S19]

Keep a maximum of three reusable frames per active stage; queue the newest useful frame, not every capture callback. Preserve native surfaces through capture/conversion/encode when supported. Do not implement unsafe cross-API texture import without matching adapter/device ownership, synchronization fences, and lifetime rules.

### 8.6 iced rendering

Use iced's `shader::Program` and primitive prepare/render integration, with the exact wgpu version iced uses. Decoders deliver frame leases through a latest-frame slot; frame arrival requests redraw only while visible. Never serialize video through PNG/JPEG or recreate an image handle for every frame. [S24]

For CPU-decoded YUV/NV12, upload reusable Y/UV plane textures and convert to RGB in a shader; preserve strides, color matrix, range, and aspect ratio. For hardware-decoded frames, use platform interop only when it removes a proven transfer safely; otherwise one explicit hardware-to-CPU transfer plus reusable texture upload is the baseline. This unavoidable fallback transfer is accounted for in the video budget and must not grow into extra per-frame copies. Suspend decode requests or reduce requested quality when minimized; release decoder/surface pools on unwatch.

### 8.7 Stream audio and synchronization

Capture application/system audio separately from microphone input, resample to 48 kHz stereo, encode Opus, and transmit on the **stream** RTC connection with SOUNDSHARE speaking state. Never denoise this path. Receiving stream audio goes through the same output device/mixer with its own volume, and respects deafen. [S9]

Use source monotonic timestamps and RTCP sender mappings to align video presentation with audio playback. Keep a bounded presentation queue and target absolute audio/video offset below 80 ms in the reference test. Drift correction occurs through small resampler adjustments, not unbounded PCM accumulation.

**Windows implementation (M35).** `fastcord-platform::loopback` records 48 kHz stereo `f32` on one dedicated MTA thread, preferring WASAPI process loopback (`ActivateAudioInterfaceAsync`, `VAD\Process_Loopback`, Windows build 20348+): a window shares its process tree (`INCLUDE_TARGET_PROCESS_TREE`), a screen shares everything except fastcord's own tree (`EXCLUDE_TARGET_PROCESS_TREE`). Older builds fall back to endpoint loopback of the default output, labelled "broad capture" with a warning in the source picker (fastcord's call is part of it). `fastcord-audio::soundshare` moves the samples through a 200 ms bounded lock-free ring to an encoder thread (stereo, `Application::Audio`, 128 kbit/s, no denoising); 500 ms of digital silence ends the burst, so an idle share sends nothing. The frames leave on the stream connection (`MediaSender::send_soundshare`, SOUNDSHARE speaking flag, DAVE-sealed), never on the call connection. A viewer plays the stream's packets through an output-only engine on the call's output device, mixed and concealed by the same jitter buffer as voice, muted by deafen. The audio clock is the only clock: video is presented as decoded and is not yet delayed to the audio's playout latency (40-160 ms adaptive jitter target), so the 80 ms reference offset is measured on hardware, not enforced.

Loopback capture must not unknowingly retransmit the call itself. On Windows/macOS exclude fastcord when the selected native mode supports it. On Linux, require an explicit monitor/application routing choice and explain when whole-output capture includes call audio; provide separate-output routing instructions. Audio capture is opt-in and visibly indicated. A source without audio may share video normally, but a failed requested audio source is an error shown to the user, not silent success.

### 8.8 Camera in calls (#84)

A camera is ordinary `video` on the call's **own** voice connection, unlike a Go Live share (§8.1), which has a connection of its own. The call connection is opened with `connect_with_video` (`VideoOptions::camera`): Identify `video: true` and one `video` layer (rid `100`, up to 1280×720, 30 fps, 1.5 Mbit/s), H.264 encode and decode plus AV1 decode in Select Protocol. Platforms that cannot capture a camera yet (Linux, macOS) offer `VideoOptions::call_watch`: the same decode offer and no layer, so they still see other participants' cameras.

- **Capture.** `fastcord-platform::capture::start_camera` is a third capture source next to displays and windows and shares `CaptureSession`, `FrameHandler`, pacing and the owner thread with them. Windows opens the device through a Media Foundation source reader with video processing enabled, picks the largest mode within 1280×720 that runs at 24 fps or more (then the rate nearest to 30 without exceeding it, then NV12 over YUY2 over MJPEG) and receives NV12 at the camera's own size. Devices are named by their stable symbolic link, listed on request (`list_cameras`), at most 16; the saved choice is the ID, never the label, and a saved camera that is not connected is shown as such and never replaced by another. `mf.dll`, `mfplat.dll` and `mfreadwrite.dll` are resolved at run time so a Windows N edition without the Media Feature Pack still starts. macOS (AVFoundation) and Linux (V4L2) report `Unsupported` until a milestone implements them; the Camera button is not offered there.
- **Send path.** The camera reuses `fastcord-video::share` unchanged (`ShareWorker::start_camera`): capture → hardware-first H.264 (Media Foundation, Microsoft software MFT fallback) → the connection's frame sink on the capture thread, with keyframe, bitrate and pause feedback and encoder reopen on a size change. A `PreviewSink` sees each captured picture before encoding, so the local preview tile is the capture itself (at most 15 pictures a second, only while on screen) and never a decode of what was sent.
- **Announcement.** After Session Description the connection declares no video (opcode 12 with `video_ssrc: 0`, like a viewer). The first frame after the camera is turned on announces the layer active; turning it off announces it inactive and the driver refuses frames; turning it on again restarts at a keyframe. Opcode 4 `self_video` follows the camera being live (capturing and H.264 accepted by the call).
- **Receive path.** Opcode 12 from other participants maps SSRCs to users (`VoiceEvent::VideoSource`). Each user whose camera is announced gets a tile (at most six) with its own `StreamDecoder`: a visibility-gated, lazily started decoding thread (the one a stream viewer uses), a latest-frame lease slot and the NV12 wgpu pass or the tiny-skia fallback. The connection asks the server (opcode 15) only for users whose tile is on screen (`MediaSender::set_video_users`); a hidden window, an overlay or a left tile stops both the request and the decoding, and the decoders release their native contexts. Tiles beyond six are neither decoded nor requested.
- **Never automatic.** The camera is off at the start of every call, is turned on only by the Camera button and is released (device closed, layer announced inactive, tile closed) the moment it is turned off, the call ends, the capture fails or another device is chosen. A failure (permission denied, camera not connected, unplugged, taken by another application) turns the request off and states the cause and remedy; nothing retries by itself. If the call stops accepting H.264 from fastcord the camera pauses, not ends.
- **Free-only.** The 720p30 layer and its bitrate are the free baseline; a lower server or receiver limit (opcode 15, REMB, loss) is obeyed and nothing is hard-coded as a paid benefit. Camera backgrounds are §8.9.

### 8.9 Camera backgrounds (#85, ADR 0010)

Free blur and four bundled preset backgrounds, chosen in the call settings of the voice panel, saved with the camera device (never turning the camera on) and changeable at any time during a call without restarting the camera or the connection. Custom uploaded backgrounds and any other paid effect are not offered.

- **Stage.** `fastcord-video::effect::Stage` runs on the capture thread between capture and encoder. Its output is the picture that the local preview shows and the encoder sends: the self-view is the stream's picture, not the raw camera. Only the self-view can be mirrored (default on, a saved choice): the sent picture never is, and the preview tile's caption says it is a preview and whether it is mirrored.
- **Segmentation.** MediaPipe selfie segmentation (Apache-2.0, ADR 0010) run by tract on one worker thread at 256x256 and at most about 15 masks a second, handed one picture at a time (no queue), with a soft-edged, temporally smoothed result. The mask is stretched over the full picture and blended per pixel; the room never appears before the first mask exists.
- **Blur and presets.** The background is a small picture (about 320 pixels wide): the frame box-averaged and blurred with two 9-pixel box passes (blur) or a bundled 320x180 picture (preset), enlarged bilinearly while compositing at the camera's full rate on NV12, with no allocation after the first picture.
- **Lifetime.** Off: no worker, no model, no copy. First enabled: the worker starts and loads the model itself. Back to Off, camera off, failure, or end of call: the worker is joined and the model dropped. A model that cannot load or run turns the camera off with a message rather than sending the unprotected picture.
- **Budget** (`docs/TESTING.md` #85): about 4 ms of capture-thread time per 1280x720 picture, 28 ms of worker time per mask, 9.6 MB resident.

### 8.10 Multi-participant call layouts and stream controls (#86)

A call can hold at once: the account's camera on the call connection (§8.8), its own screen share on a stream connection of its own (§8.1), up to six other participants' cameras (opcode 15 interest on the call connection) and up to four watched Go Live streams, each a separate voice connection with its own DAVE identity. None replaces the parent voice session: every stream task is aborted on its own key, and leaving the call tears them all down without a stream delete.

- **Tiles.** `call_layout` lists the call's videos in a fixed order (watched streams, remote cameras, then the self-view while the camera is on) and `CallView::plan` decides which are drawn. At most **8** are drawn and decoded at once, the number of plane pairs the NV12 pass binds (`playback::MAX_VIDEOS`). Anything beyond that is neither decoded, requested nor drawn, and the view says so ("3 more videos are not shown: at most 8 …", with the way out: pin one or stop watching a stream). A fifth watched stream is refused with the reason and the four already watched are untouched. Nothing is dropped silently.
- **Layouts.** *Grid* (every video the same size; large up to four tiles, small beyond), *Focus* (one large tile, the rest in a strip; the first video unless one is pinned) and *Speaker* (Focus following whoever speaks on camera and keeping them through silence, so a speaker never flaps back; a pin overrides it). Pinning in a grid switches to Focus. A *full view* draws one video only; every other tile is hidden (suspended, not refused) until it ends. The window can additionally go full screen. Participants without video are listed beside the tiles (at most 24, then "and N more") and can be hidden. There is no compact strip outside the call view: the voice panel at the bottom of the sidebar (heading click, or the call settings for streams) opens it.
- **Hidden costs nothing.** A tile that is not drawn (call view closed, window minimized or covered, over the cap, behind a full view) has its session's visibility cleared: the picture is released, the decoder is suspended and its native context freed. A hidden camera is also left out of the opcode 15 request, and a hidden stream turns its connection's video off (`set_video_wanted(false)`, so the server stops forwarding it) and asks for a keyframe when drawn again. Streams are decoded only while the call view draws them.
- **Quality wants.** A camera drawn small asks for its lowest *active* layer (`MediaSender::set_video_thumbnails`; the driver picks the lowest `quality` among the layers the server announced in opcode 12 and re-requests a keyframe when the choice changes). A user who announced a single layer is unaffected, and the wants quality value stays 100: no other value, resolution or frame rate is requested. Whether Discord announces more than one camera layer to a user client is unverified (TESTING #86); a stream tile is not requested at a lower layer and decodes whatever the sharer sends. The 720p30 baseline is unchanged.
- **Stream audio controls (2026-10-21).** Each watched stream has independent local 0-200% volume and mute controls in its tile and in call settings, plus received-audio/output-failure status. They survive hiding the video and follow the call output device and deafen state (§8.7); voice volume is separate. A pop-out window would need a multi-window iced application and is not built; the full view and window full screen cover the need inside one window. The account's own share has no preview tile (a preview of the screen being shared would be a mirror of the call view).
- **Budget** (arithmetic on the caps, asserted by `call_layout` tests; measured hand-off cost in `docs/TESTING.md` #86). Each drawn tile holds the 6 MiB decode queue and a three-picture pool: 6 + 3 x 1.32 = 9.95 MiB at 720p, 14.9 MiB at 1080p, and never more than 6 + 48 MiB whatever the size. Eight 720p tiles are therefore under 80 MiB of video buffers (about 120 MiB at 1080p; 432 MiB is the absolute ceiling for pathological picture sizes), on top of the former 300 MB active-session planning reference of §12 for one watched stream: a call with eight videos adds those buffers, and decoder cost is per tile (one hardware or dav1d decoder thread each). Idle (no call, or nothing drawn) costs nothing extra.

### 8.11 Free server-local soundboard (#87)

The open server's soundboard, for the voice channel the account is in. Scope: the server's own sounds only; other servers' sounds (Nitro), Discord's default sounds, and entrance sounds are not offered here (managing the server's own sounds is the #99 assets panel).

- **Catalog.** `GET /guilds/{id}/soundboard-sounds` when the panel opens (at most 64 sounds, names cut to 32 characters) and again after `GUILD_SOUNDBOARD_SOUND_CREATE/UPDATE/DELETE` or `..._SOUNDS_UPDATE` for that server. A re-list drops the preview and a selection whose sound vanished. The list lives in the panel only, not in the reducer's store, and is dropped when the panel closes.
- **Preview** is local: CDN fetch (`cdn.discordapp.com/soundboard-sounds/{id}`, no credentials, through the shared disk LRU, at most 1 MiB), decode on a blocking worker (`fastcord-audio::soundboard`, ADR 0011) and playback on the file player's output-only engine. It sends nothing and stops on a second click, deafening, closing the panel or leaving the call.
- **Play** is one click on one sound: `POST /channels/{id}/send-soundboard-sound` (`{"sound_id"}`, no `source_guild_id`), re-checked at the click against the call (the voice target must be the panel's server and channel), the channel's `USE_SOUNDBOARD` and `SPEAK` permissions (from the sidebar row) and the sound's availability. One play at a time (the buttons are disabled while one is in flight); sent once at user-write priority, a 429 is rescheduled by the shared scheduler and nothing else is retried; an ambiguous failure says the sound may have played. A 401 stops the account like any other.
- **Hearing others.** The official client plays a sound locally when `VOICE_CHANNEL_EFFECT_SEND` carries a `sound_id`; the sound is not mixed into the voice stream, so this client does the same (the account's own play comes back through the same event, so there is exactly one path). The reducer worker keeps the newest 4 cues (`VoiceControls::sound_cues`, ignored and blocked users are dropped, events for another channel are ignored); the UI starts at most **3** players for others, a repeat of a sound that is already playing restarts it, an older cue (by sequence number) is never played twice. Emoji effects are not surfaced.
- **Controls.** Received soundboard volume (0-100, multiplied with the sound's own volume) and Mute soundboard are saved in `audio-devices.json`, independent of the per-person voice volumes (§6). Mute drops the sounds from the call; it does not affect Preview. Favourites (at most 64) are saved there too. Deafening (self or server) starts nothing and stops everything.
- **Budget.** 64 sounds, 64 favourites, 4 cues, 3 + 1 players, each a clip of at most 6 s (about 100 KiB of packets) from a file of at most 1 MiB; a player and its engine exist only while a sound plays, and the 250 ms tick runs only then. Release events: sound end, mute, deafen, leaving the call, closing the panel (preview), logout.
- **Limits.** The Soundboard button needs the call's channel row in the sidebar (its permissions come from that row), so it is hidden while another server is open. Sounds in Ogg Vorbis are not decodable. A play while the sound's server-reported volume is zero is silent by design.


Use a versioned Unicode emoji index with grapheme-aware parsing for variation selectors, skin-tone modifiers, flags, and ZWJ sequences. Preserve original Unicode text for copy/paste and sending. Render recognized emoji as consistently sized inline Twemoji assets; load only required assets rather than a full decoded atlas. Twemoji graphics require CC-BY-4.0 attribution, separate from its MIT code license. Unsupported/new sequences fall back to text/font rendering, not replacement of unrelated text. [S25]

Custom emoji tokens `<:name:id>` and `<a:name:id>` become inline assets loaded from the Discord emoji CDN with static PNG/WebP or animated GIF/WebP as supported. Keep a text fallback for unavailable/deleted assets. Animated emoji only advance while visible; cap simultaneous decoding and reuse frame buffers. Picker cells are virtualized and searchable by name/alias, with categories, recent items, Unicode variants, and available guild emoji.

The picker inserts Unicode or the correct custom token at the cursor; it does not upload or create emoji. Availability for sending outside the source guild and animated emoji may depend on account entitlement and permissions; reflect known restrictions and surface server rejection without fabricating Nitro capabilities.

Reactions store aggregate counts and the current user's state, not every reacting user. Handle add/remove/remove-all/remove-emoji and events for uncached messages without allocating their histories. Support normal reactions only; do not expose a nonfunctional paid burst-reaction UI. Clicking one's existing reaction removes it. Reaction-detail users load only on explicit inspection. [S5]

### 8.12 Text chat in voice and Stage channels (#88)

A guild voice (type 2) or Stage (type 13) channel carries a text conversation under the same channel ID, read and written through the ordinary message routes and dispatches. No new route or event.

- **Opening.** Each voice or Stage row in the sidebar has a Chat button; the row itself still joins the call (`Message::JoinVoice`). Opening a chat (`Message::SelectChannel`) is the same reducer selection as for a text channel and never sends a voice request; joining, moving or leaving the call never changes the selection (tests in `main.rs` and `gateway.rs`). Nothing is fetched for rows that are merely listed: history loads when the chat is opened, like a text channel.
- **Permissions** reuse the one computation (`PermissionContext`; the text-like implicit rules already covered voice). The chat opens with VIEW_CHANNEL alone; CONNECT gates only the join (`ChannelRow::enabled` now means "chat can be opened", `permissions.connect` "call can be joined"; a row without CONNECT is labelled "(no access)" and cannot be joined). History needs READ_MESSAGE_HISTORY, the composer SEND_MESSAGES, reactions ADD_REACTIONS. An overwrite or role change re-evaluates the open chat on the next snapshot: sendability follows at once, losing VIEW_CHANNEL closes it, and the voice connection is never touched.
- **Read state.** `MESSAGE_CREATE` marks the row unread and counts mentions exactly as for text channels, and the unified inbox lists voice chats. Notifications use the same path.
- **Not offered:** threads (Discord does not allow them in voice chats; the Threads button is hidden) (the Stage audience and speaker flows are 8.14, #89).
- **Limits.** Whether the official client also requires CONNECT, or a live Stage, to show the chat is unverified (see PROTOCOL).

### 8.13 Invitations and membership (#90)

A message that contains an invite link (`discord.gg/<code>`, `discord.com/invite/<code>`, `discordapp.com/invite/<code>`, with or without `http(s)://` and `www.`; the host must begin a word, so look-alike hosts and paths are ignored) shows an invite card below its text, at most two per message. A card is a fixed 76 px block (server icon, name, channel, online and member counts when Discord sent them, expiry date, a Join or Joined button) so the row never changes height when the invite resolves, and it reads "Invite invalid" for an expired, revoked or unknown code.

- **Lookup.** `GET /invites/{code}?with_counts=true` for codes of messages the timeline has built (newest first, at most 16 considered per pass), from a cache of at most 64 codes with a 10 minute TTL, at most 3 lookups in flight. A failed lookup is not repeated for 30 seconds; no lookup is ever retried by a loop. Icons use the existing public avatar store.
- **Join.** One explicit click, `POST /invites/{code}` with `{}`, one request, never retried by the client. A CAPTCHA or verification answer sets the existing write pause (`RestError::AccountVerificationRequired`): the card shows the paused-account text and no write is sent until the official client has verified the account and fastcord is reconnected. Any other refusal shows Discord's own message. A server already in the rail window reads "Joined"; clicking Joined opens it. The rail changes only on `GUILD_CREATE`.
- **Leave.** Right-click a server in the rail, then confirm: `DELETE /users/@me/guilds/{guild}` with `{"lurking":false}`. The owner (the reducer's `owned` flag, checked again at the confirming click) is told why it cannot leave and nothing is sent. The rail changes only on `GUILD_DELETE`.
- **Create.** The header of a text, announcement or voice channel where the account has CREATE_INSTANT_INVITE, MANAGE_GUILD or MANAGE_CHANNELS offers Invite, which opens a small panel: with CREATE_INSTANT_INVITE it picks a lifetime (30 minutes, 1, 6, 12 hours, 1, 7 days, never; default 7 days) and a use limit (none, 1, 5, 10, 25, 50, 100) and creates with `POST /channels/{id}/invites`, then shows the `discord.gg/<code>` link with Copy. Temporary invites are not offered.
- **Manage.** With MANAGE_GUILD the panel's Manage invites lists every invite of the server (`GET /guilds/{id}/invites`), with only MANAGE_CHANNELS the invites of that channel (`GET /channels/{id}/invites`): at most 100, each with code, channel, inviter, uses against the limit, time left (computed from the local clock) and a temporary tag; Copy link, and Revoke which asks for Confirm on that row and then sends `DELETE /invites/{code}` once. A row leaves the list on success or unknown invite; any other failure keeps it with a note and never retries by itself.
- **Discovery.** The Ex button at the top of the server rail opens a read-only list of public servers (`GET /discoverable-guilds`, unverified): icon, name, description and approximate counts, 24 per page, Show more up to 4 pages. It states that a server is joined with an invite link; no join route is invented.
- **Not implemented:** discovery search, joining straight from the discovery list, and a joined-server verification or onboarding flow beyond the pause above.

### 8.16 Scheduled community events (#92)

Implemented in `fastcord-model::scheduled_event` (the event, bounded text, recurrence description), `fastcord-discord::scheduled_event` (list and write requests, draft validation, `Store::event_permissions`/`event_allows`), `fastcord-discord` `state/events.rs` (the reducer part) and `fastcord-app::scheduled_events` (panel and banner). Routes and events are in `docs/PROTOCOL.md` and are all unverified.

- **State.** The reducer keeps at most 100 events per guild, counted in the guild's metadata bytes; under memory pressure every guild but the focused one gives its events up first (a list request brings them back). Gateway events settle on the last one seen and a late update of a deleted event is ignored. The list answer (an explicit request from the panel, handed to the reducer as a command) replaces the guild's events. The navigation snapshot carries the selected guild's events, soonest first with ended ones last, and the account's event permissions.
- **Panel.** The header's "Events" opens the list (name, status, local start with the relative time, kind, interested count), a detail view (description, end, place or channel, recurrence, creator), interest, Copy link and Join. People with MANAGE_EVENTS, or CREATE_EVENTS for their own events, also get New event, Edit, Start now, End event, Cancel event and Delete. Times are typed as `YYYY-MM-DD HH:MM` and shown in the local offset read at start-up.
- **Writes.** One at a time, one request each, rechecked by the reducer against permissions and the event's current status, never retried; cancel and delete ask first; the list and the detail change only when the Gateway confirms. A refused request shows Discord's 403/404/429 text; an ambiguous one reads "may have been applied".
- **Banner.** A voice or Stage event that is live, or due within 15 minutes (and not more than an hour late), shows "Starting soon" / "Live now" with Join, Details and Hide above the chat. It is evaluated when the view is drawn, with no timer.
- **Not implemented:** creating or changing recurrence, attendee lists (only the interested count), event cover images, and a desktop notification when an event the account is interested in starts (the toast path of #57 is driven by messages only, and whether the account is interested is known only for changes made in this session).

### 8.15 Rules screening, onboarding and server guide (#91)

The member side only (configuration is #97). Implemented in `fastcord-discord::onboarding` (bounded parsing, request bodies) and `fastcord-app::onboarding` (one panel at a time, banners). Routes are in `docs/PROTOCOL.md` and are all unverified.

- **Pending.** Guild members carry `pending` and `flags` (Gateway). While the own member is pending the navigation reducer reports no SEND_MESSAGES, ADD_REACTIONS, thread creation or attachments, so the composer and reactions are off with the note "Accept the server's rules to write or react"; a banner offers Review rules. The state ends only when the Gateway reports `pending: false`.
- **Rules.** Review rules loads `member-verification`; the rules (TERMS) are shown with one "I have read and agree" button, sent once. Other required questions are left to the official client. A submitted application reads "waiting for the server's moderators".
- **Onboarding.** A banner appears when the member flags say onboarding started and not completed; the channel header's "Channels & Roles" opens the questions for revisiting, preselected from held roles. Single/multi choice and required questions are enforced before one explicit Save. Roles and channels change only through Gateway role and channel updates.
- **Guide.** The header's "Guide" shows the welcome text, new-member actions and resource channels read-only; links open only channels the account can already see.
- **Not implemented:** Browse Channels, non-TERMS screening answers, configuration.

### 8.14 Stage audience, speakers and moderators (#89)

A Stage channel (type 13) is a voice channel with a role per person: speakers (`suppress: false`) and the audience (`suppress: true`), plus a live session with a topic (the stage instance). Everything is derived from the voice states and `STAGE_INSTANCE_*` events held by the reducer; nothing is cached outside it.

- **Transmit gate.** `VoiceControls::capture_enabled_with` is the only gate and now also requires `!stage.suppressed`. `StageControls` is derived from the store on every snapshot and set at once from the account's own `VOICE_STATE_UPDATE`; in a Stage the account counts as suppressed until its own voice state in that channel is known, so joining, moving and reconnecting (READY rebuilds voice states) never transmit before the server permits. Suppression and the raised hand are server state and survive a reconnect because they come back with the voice state.
- **Permissions** (`Store::stage_permissions`, the same role/overwrite computation as member actions, failing closed on unknown roles or a timeout): REQUEST_TO_SPEAK to raise a hand; MUTE_MEMBERS to invite, approve, move speakers to the audience and to speak without asking; the Stage moderator set (MANAGE_CHANNELS, MUTE_MEMBERS, MOVE_MEMBERS) to start, edit and end the session. `Store::stage_allows` also checks the state an action changes (a second request, inviting a speaker, starting a live Stage all send nothing). The worker runs it again immediately before the request. Role hierarchy is not applied to speaker changes.
- **UI.** Sidebar: speakers then audience under headings, hands first (earliest request first), counted in the existing 8-person cap. Seat controls above the voice panel; moderator actions in the member menu; the topic header and the hand list above the conversation. One write at a time; a late answer after leaving or changing Stage is ignored.
- **Not offered:** scheduled-event linkage, public Stages, the audience's own video or screen share rules beyond what the call already allows, and automatic acceptance of anything.

### 8.17 Server and channel management (#93)

Implemented in `fastcord-discord::guild_admin` (validated request bodies, presets), the reducer read model `state/navigation/admin.rs` (`guild_admin`, `GuildAdmin::plan_move`) and `fastcord-app::guild_admin` (one panel). Routes are in `docs/PROTOCOL.md` and are all unverified.

- **State.** `fastcord-model::GuildSettings` (description, Community flag, system and AFK channels, AFK timeout, default notifications, explicit-content filter) is read from `GUILD_CREATE` (top level or `properties`) and replaced by `GUILD_UPDATE` only when that event carries `features`; `Channel` gained topic, nsfw, slowmode, bitrate, user limit, region and video quality, with texts capped. Create, update (also a reorder) and delete of channels already flow through `CHANNEL_CREATE/UPDATE/DELETE` in the reducer; nothing is shown from a request's own answer.
- **Read model.** The panel asks the bridge for one server (`admin_guild`); the snapshot then carries its settings, whether the account owns it, guild-wide Manage Server / Manage Channels, each channel with its settings and its own Manage Channels, and up to 256 held non-bot members for a transfer. When the account's roles are not established there is no model, so no permission is guessed.
- **Writes.** One explicit click, re-checked against the current model (permission, owner, Community, exact typed name, the target is a held member), one request in flight, never retried. Delete server needs the typed name, transfer and channel delete a Confirm that names the target. A request body carries only changed fields.
- **Bitrate.** Discord does not report a server's maximum, and boosted values are not hard-coded: the choices run to 64 kbps or the channel's current value if higher.
- **Community.** Shown on or off. Announcement, Stage and forum creation is refused with the reason when it is off. Turning it on or off is not offered (rules and updates channels, verification level).
- **Reorder.** Move up/down swaps a channel with its sibling in the same category and list (categories, text-like, voice) and sends only the positions that change.
- **Not offered:** Community toggle, billing, boosts, vanity URL, monetization, paid media channels (roles and overwrites: 8.19).

### 8.19 Roles and channel access (#94)

Implemented in `fastcord-discord::role_admin` (validated bodies and requests), the reducer read model extension `state/navigation/admin.rs` (`GuildAdmin::roles`, `plan_role_move`, `can_grant`, per-channel overwrites and `synced`, and `admin_member` for one member) and `fastcord-app::guild_admin::access` (panels inside the #93 admin panel). Routes are in `docs/PROTOCOL.md`, all unverified.

- **Roles.** Server settings and the right-click menu open the roles list (Manage Roles), highest position first. The editor changes name, plain colour (presets and hex), hoist, mentionable and permissions as checkboxes grouped like the official client (`fastcord-model::PERMISSION_GROUPS`; paid, game-activity and monetization bits are not listed and unlisted bits are preserved). Controls the hierarchy forbids are shown disabled with the reason the reducer computed (`edit_lock`, `move_lock`, `assign_lock`).
- **Member roles.** The member right-click menu has "Roles\…": a checkbox per assignable role; each tick is one `PUT` or `DELETE` and the box follows `GUILD_MEMBER_UPDATE`, not the click.
- **Overwrites.** The #93 channel edit panel has a Permissions section (Manage Roles in the channel): overwrites listed per role or member, per-bit Allow / Neutral / Deny for the bits that exist for the channel kind, add a role or a held member, save (one `PUT`), remove (Confirm), and whether the channel is synced with its category, with "Sync with category" when it is not.
- **Inspection.** A read-only "What can a member do here?" list: the reducer computes the chosen held member's effective permissions in every listed channel with the one `channel_permissions` function, from the member's roles and the channel's overwrites. A member whose roles are not known shows nothing instead of a guess.
- **Immediate effect.** Navigation, composer and media already derive from the reducer's role and overwrite state, so a role, member or overwrite event changes access at once; a refused or ambiguous write changes nothing on screen.
- **Bounds.** At most 250 roles, 100 overwrites per channel, 500 channels, 256 held people; the member read model is rebuilt per request and kept nowhere.

### 8.20 Announcement channels (#98)

Implemented in `fastcord-discord::announce` (the four requests, the type-2 webhook parse that never reads a token), `state/navigation/admin.rs` (`follow_targets`: the reducer lists where a channel may be followed, with `MANAGE_WEBHOOKS` added to `Permissions`), `fastcord-model::Message` (`is_crossposted`, `crosspost_source`) and `fastcord-app::announce` (the Confirm, picker and list panel; the row tools and marker are in `timeline`). Routes are in `docs/PROTOCOL.md`, all unverified.

- **Publish.** A message tool in an announcement channel for messages not yet crossposted (own with Send Messages, any with Manage Messages), then a Confirm in the row. One request, re-checked against the loaded message and the channel's permissions at the click; the row shows "Published" only after `MESSAGE_UPDATE` sets the flag.
- **Received crossposts** show their source line; their later edits and deletes are ordinary events.
- **Follow.** The announcement channel header's Follow opens a picker of text channels where the reducer computes Manage Webhooks; the follow is sent once.
- **Followed destinations.** The text channel edit panel (Manage Webhooks) has "Followed announcements": the type-2 webhooks with their source and Unfollow after a Confirm (`DELETE /webhooks/{id}`).
- **Not offered:** any scheduler or automatic posting, other webhook types, paid features.

### 8.21 Community setup (#97)

Implemented in `fastcord-discord::community_admin` (editable documents that preserve unrelated fields, request bodies, bounds, permission checks, the five requests), `fastcord-model::GuildSettings` (rules/updates channel, verification level, features), `state/navigation/admin.rs` (`GuildAdmin` fields and per-channel `everyone_view`) and `fastcord-app::community` (the panel). The server settings' "Community setup" opens four tabs (Rules, Onboarding, Guide, Community), gated on Manage Server, with role grants gated on Manage Roles through the #94 reducer (`can_grant`).

- One save per tab, re-checked against current permissions just before sending; dirty indicator and Discard; one write in flight; stale answers ignored by a serial; 401 stops the account.
- The editor reloads from the answer (or the next read); Community changes show only after `GUILD_UPDATE`.
- Disabling Community needs a Confirm. Enabling needs a rules and an updates channel.
- **Left:** analytics and monetization dashboards, custom emoji, non-TERMS questions.

### 8.22 Server emoji, stickers and soundboard assets (#99)

Implemented in `fastcord-discord::assets` (the nine requests, the tier slots `expression_limits`, validated names and files), `image_data` (emoji and sound `data:` helpers), `rest.rs` (`RestRequest::raw` for the sticker multipart), `state/navigation/admin.rs` (`guild_assets`: Create/Manage Expressions and the emoji list from the reducer), the `GUILD_STICKERS_UPDATE` decode, and `fastcord-app::assets` (the panel).

- **Panel.** The server menu's "Emoji and sounds" (shown with Create or Manage Expressions) opens Emoji, Stickers and Soundboard tabs, each with its free-slot count, an upload form, and rows with Edit and Delete (after a Confirm).
- **One click, one request.** Permissions, the slot and the asset's existence are re-checked against the reducer's read model at the click; at most one write is in flight; nothing is retried. Discord's refusal is shown verbatim and an ambiguous failure reads "may have been applied".
- **Files** are judged by signature and size on a worker thread; a sound's length uses the #87 decoder; previews are the bounded emoji decoder and the #87 player. Nothing is written to disk.
- **Free only.** Slots come from `premium_tier` and are never bought; with no tier reported none is assumed.

### Apps: commands and components (#100)

A message's buttons and selects, the Components V2 layout pieces and slash commands of installed applications. Ownership: `fastcord-model` holds the bounded component tree (`component`); `fastcord-discord` holds the wire (`interaction`: request bodies, the command index, the modal and outcome events) and the byte-accounted store; the history worker (`history/interact.rs`, rules in `interactions.rs`) owns the session ID, the ledger of unsettled interactions, the open modal and the 15-second timers; the UI draws components (`component_view`) and the command list, option form and modal (`slash`). One click is one request, re-checked against the store, the open channel, the send permission and the session when it reaches the worker, sent once at user-write priority and never retried. The ledger allows one pending component per message and one pending command per channel (8 in all). Premium buttons are disabled with "not available in this client"; nothing here purchases, schedules or automates anything. Ephemeral messages (flag 64) are labelled, dismissable locally and purged with the conversation. Route and event details, and what is left, are in docs/PROTOCOL.md.

### 8.18 Accounts, sessions and security overview (#107)

- **Saved accounts.** Tokens live in the native store, one entry per user id (unchanged). A non-secret index file (`accounts.tsv`: user id, display name, avatar hash; at most 32 rows, 16 KiB) labels them and is reconciled with the store on every start, so accounts saved before the index need no migration. A missing or damaged index only loses labels.
- **Switching.** Exactly one active account. Switch runs the logout teardown (gateway, voice, every panel, drafts, attachment cache) without deleting the saved login, then restores the chosen account through the ordinary validate-then-enter path; the phase passes through Authenticating, never two sessions. A memory-only session cannot switch. Remove deletes another account's token after Confirm; logging out the active account still deletes its token.
- **Sessions.** `GET /auth/sessions` list and `POST /auth/sessions/logout` after Confirm, one request, never retried. A 60003/MFA demand is shown as Discord's refusal with "Confirm in the official client"; MFA re-authentication is not implemented and never bypassed.
- **Security overview.** Read-only MFA, email-verified and phone-present flags. Password, MFA, passkey and recovery changes are handed to the official client or `https://discord.com/settings/account`.
- **Communication settings.** Friend-request sources and the explicit-content filter were delivered with the Safety panel (#80); no duplicate was added.

## 10. iced UI architecture

Use iced's `Application`-style builder with `update`, `view`, `Task`, and `Subscription` concepts as provided by 0.14. UI messages are typed intents/results. Long-lived gateway/media workers feed stable subscriptions; creating a view must never reconnect a socket or recreate audio state.

`AppState` contains navigation, drafts, selection, presentation read models, pending operation IDs, and transient error state. The domain reducer remains authoritative for server entities. UI updates receive bounded/coalesced deltas; media frames bypass the general message queue through frame leases. A single UI redraw must not traverse all cached messages.

### Virtualization

Implement a reusable variable-height virtual list for message history and a fixed-height form for guild/channel/member/emoji rows. `scrollable` and `lazy` alone are not proof of virtualization.

- Keep measured heights in an indexed prefix-sum structure keyed by message ID and width bucket.
- Build widgets only for the visible range plus one viewport of overscan.
- Represent skipped items by top/bottom spacers.
- Anchor scrolling by message ID plus in-row offset. Preserve the anchor when prepending history or when an image changes height.
- Follow new messages only if already at the bottom; otherwise show a jump-to-latest control.
- Evict height/layout data with messages; avoid a hidden second unbounded history cache.
- Layout rich message text as grapheme-aware text/image runs with wrapping, links, replies, attachments, and emoji. Do not embed HTML or a WebView.

Caches are centralized with shared immutable assets; clones of lightweight handles must not clone decoded pixel arrays. Avatar/emoji/image decoding and disk access are asynchronous. Settings include token management, devices, suppression, and local media preferences. Connection and media errors are actionable inline state, not modal-error loops.

### Application and interface icons

Original fastcord artwork lives in `assets/icons/`. Its generator is the single
source for 90 SVG glyphs and the compiled iced canvas catalog, on a 20 px grid
with 1.7 px rounded strokes. Both wgpu and tiny-skia draw the same paths, without
an icon font, runtime SVG decoder, network access or animation timer. Use 14 px
for participant indicators, 16 px beside captions, 20 px for tools and 24 px in
the rail. Compact actions retain tooltips; confirmation and file controls keep
visible captions. Slash states remain distinguishable without color. The
original speech/bolt mark identifies the app, login and Direct Messages rail
entry. Windows embeds a multi-size ICO; macOS bundles declare an ICNS; Linux
archives carry a desktop entry and hicolor PNG/SVG icons. The 64 px window icon
is decoded once before the event loop starts. See `assets/icons/README.md` for
regeneration and the offline native gallery (`--icon-gallery`, debug builds).

No fixed 60 Hz subscription. Enable animation ticks only for currently visible animated emoji or ongoing media, and stop them on occlusion/inactivity. Baseline text navigation must remain usable while voice/video workers are busy.

## 11. Login and token storage

Login offers **QR code** (primary, shown first), **email/password** (Windows only; a CAPTCHA is shown to the person in a short-lived web view and never solved; TOTP second factor; ADR 0006 amendment) and **explicit token paste** (advanced), all with a clear account/ToS warning and password-style input. There is no embedded Discord login page, token extraction or browser-store scraping. Validate the resulting token once with `GET /users/@me` before establishing the Gateway.

Use the keyring ecosystem with one selected native backend per OS: Windows Credential Manager, macOS Keychain, and Linux Secret Service. Prefer `keyring-core` plus the exact backend crate, not the CLI feature that pulls every store. Native-store calls run off the UI thread. Store under application service `fastcord` and account ID after validation. [S26]

If a secure store is unavailable/locked, offer an explicitly nonpersistent session and explain that it will require login next launch; never silently write a plaintext token. A successful logout clears in-memory secrets, removes the saved credential, tears down workers, and purges account caches. Deleting a local credential is not a claim that every Discord session was revoked.

Use secret wrapper types, no `Debug` serialization of tokens, and zeroization of owned secret buffers where feasible. This cannot guarantee erasure of arbitrary copies inside third-party TLS/HTTP/GUI internals; do not promise that it can. Do not read the clipboard automatically or erase unrelated clipboard content.

QR login implements Discord's remote-auth protocol: a dedicated WebSocket (`wss://remote-auth-gateway.discord.gg`), an ephemeral RSA-OAEP key pair generated per attempt, the fingerprint rendered as a QR code (`https://discord.com/ra/{fingerprint}`), display of the pending user after the phone scans, and the encrypted ticket exchanged for the token through the documented REST call. Keys and tickets live in memory only and are dropped on success, cancel, or timeout; the QR expires with the server's timeout and can be regenerated explicitly. If the exchange requires a CAPTCHA, stop and offer token paste or the official client. Password login, MFA entry, CAPTCHA solving, and account verification are not implemented. If Discord requires a challenge, stop the affected action and direct the user to the official client; do not retry indefinitely. The accepted ToS risk does not guarantee the account will remain usable. **UNKNOWN U6:** current remote-auth opcode set and ticket-exchange endpoint must be captured from the community documentation and verified with the alt account in milestone 5a.

## 12. Performance budgets and measurement

All figures in this section are design targets until milestone 38 records results.

### 12.1 Idle target

**Maintainer revision, 2026-10-21:** the hard 150 MB RAM limit is removed. Stable simultaneous share send/watch with audio takes priority. The product target is lower measured RAM, CPU and GPU use than the official Discord client under equivalent workloads on the same machine. Report working set and private committed memory for both clients; never claim an improvement without matched measurements. Other OS measurements report RSS and their platform-specific private/footprint metric. Report dedicated/shared GPU memory separately; do not hide it in an RSS comparison.

**Renderer backend (ADR 0002):** iced uses wgpu pinned to the platform-native API — DX12 on Windows, Metal on macOS, Vulkan on Linux. OpenGL is excluded; wgpu's automatic selection is not used. Measured on the primary Windows PC with an empty window, release build (2026-10-07, working set): automatic selection 172 MB, DX12 77 MB, Vulkan 96 MB, GL 221 MB, tiny-skia 21 MB. **Risk:** DX12 alone exceeds the 45 MiB "runtime + idle renderer" allowance below; milestone 38 must either reduce renderer overhead (wgpu limits, staging-belt size, atlas sizes) or rebalance the envelope, and the gap must be tracked from the first UI milestone onward.

Milestone 8 pins these APIs through safe compositor configuration. Its Windows release unauthenticated login-screen surrogate averaged 78.147 MB working set over ten minutes (+1.147 MB versus the 77 MB empty-window baseline), with mean private committed memory 84.951 MB. Dedicated/shared GPU allocations are reported separately in `docs/PERFORMANCE.md`. This does not verify the authenticated idle target or close the 45 MiB allowance risk.

Historical planning envelope (MiB, not MB; retained as a reference, not a hard release gate):

| Component | Planning allowance |
|---|---:|
| Runtime, code/data, threads, HTTP/TLS, idle renderer | 45 |
| Guild/channel/user/message state | 32 |
| Decoded images and emoji | 12 |
| Glyph/texture and UI presentation allocation | 16 |
| Network scratch and queued events | 8 |
| Margin for allocator/driver variability | 20 |
| Total planning envelope | 133 MiB, approximately 139.5 MB |

GPU-resident allocations are not necessarily charged identically on each OS; the table is a planning envelope, not an accounting identity. Actual process and GPU measurements determine acceptance. Video and voice engines must be lazy so their active-session buffers are not paid at idle.

Near-zero idle CPU means mean **<0.5% of one logical core over ten minutes**, measured as process CPU-time delta divided by wall time, with no input or animation. Record heartbeat-related spikes separately. Do not divide by total system core count to make a busy worker appear idle.

### 12.2 Active targets

- Voice, microphone denoise enabled, four concurrent remote speakers: p95 processing work below 5 ms per 20 ms block; no callback allocation or engine-caused underruns in a 30-minute local test.
- Local audio processing latency target below 80 ms, excluding network propagation and remote jitter; report measured end-to-end latency separately.
- Screen share baseline: 720p30 encode and watch interoperability; <1% locally dropped frames on the reference hardware; p95 frame age <250 ms on a controlled LAN path; audio/video offset <80 ms.
- The former 300 MB active-session estimate is a planning reference, not a release ceiling. Measure voice plus simultaneous send/watch with audio against the official client; bounded queues, stable latency and resource teardown remain requirements.
- Message navigation/scroll: p95 frame time <16.7 ms on the reference display workload, with widget count proportional to viewport size.
- Memory after ending calls/shares and waiting two minutes must return within 10 MB of the warmed pre-call baseline; repeated cycles must plateau, not grow monotonically.

### 12.3 Measurement protocol

Use release builds from the same lockfile and record commit, OS, CPU, GPU, driver, display scale/resolution, account fixture size, active codec/backend, and power mode. No debugger or verbose logs during reported runs.

1. Cold launch and login: record peak startup memory separately from steady state.
2. Warm idle: 10 minutes after cache warmup, selected text channel, no media or animations.
3. Repeat idle minimized and with visible animated emoji, labeling the latter as active animation rather than idle.
4. Large-state deterministic fixture: 100 guilds, 2,000 channel records, 2,000 cached messages, and 500 user records; also report a real alt-account scenario separately.
5. Scroll 10,000 historical messages through paging without retaining all 10,000 in memory.
6. Run voice with four speakers, suppression on/off, device switching, 1–5% induced loss, jitter, and reconnect.
7. Send/watch 720p30 with stream audio, with and without hardware acceleration; cycle sessions 20 times.

Use Windows Performance Recorder/Analyzer and process memory counters on Windows; Instruments/Activity Monitor/vmmap on macOS; perf and `/proc`/smaps on Linux. Profile allocations in an explicit profiling build; ordinary shipped builds do not run a constant sampling profiler. Publish result tables and raw measurement instructions in `docs/PERFORMANCE.md`.

## 13. CI and release design

> Test-only fake: `crates/fastcord-fake` and the `fake-discord` features of `fastcord-discord` and `fastcord-app` (docs/TESTING.md) let the app run against a loopback server. They are off by default, inert until a loopback origin is set, never packaged, and `compile_error!` in any release build, so a release binary cannot contain them. The host allowlists (REST origin, `*.discord.gg` Gateway, upload/CDN hosts) are unchanged without the feature.

### 13.1 CI — milestone 1

On pull requests and main-branch pushes:

- Install exact Rust 1.99.0 with rustfmt/clippy.
- One formatting job: `cargo fmt --all -- --check`.
- Windows MSVC, Linux, and macOS jobs: `cargo clippy --workspace --all-targets --locked -- -D warnings` and `cargo test --workspace --all-targets --locked` for the platform's shipping feature set.
- Build all three platform backends, not just model-only tests. Feature-matrix checks must avoid enabling mutually exclusive native backends through an indiscriminate `--all-features`.
- Cache Cargo registry/git and target output using OS, architecture, toolchain, lockfile, native-source manifest, and feature hashes. Native build caches are separate and keyed by compiler/SDK/configuration.
- Pin Actions by commit SHA; default permissions are read-only; cancel superseded PR runs.
- Tests are deterministic and offline: serialized Gateway/REST fixtures, rate-limit clock tests, permission/reducer tests, crypto/media vectors, and image/parser tests. Live Discord credentials are never exposed to PRs.
- Display/audio/capture checks requiring hardware are explicitly manual integration checks; a hosted runner's unit-test success is not a screen-share acceptance claim.

Pin actual supported hosted-runner labels in the workflow when authored. Suggested families are windows-2025, ubuntu-24.04, macos-15 for arm64, and macos-15-intel for Intel if available to the repository. **UNKNOWN U5:** runner availability and preinstalled Rust/native tooling must be resolved in milestone 1. The user's local Rust installation is contextual evidence, not proof that a hosted image has it.

### 13.2 Tag releases — milestone 2

Trigger on `v*` tags and validate semantic version against Cargo package version. Build native artifacts for:

- `x86_64-pc-windows-msvc`.
- `x86_64-unknown-linux-gnu`.
- `aarch64-apple-darwin`.
- `x86_64-apple-darwin`.

Windows remains the primary live test target, not the only supported release target. macOS architectures ship as separate bundles, avoiding an untested universal-binary merge. Define Linux's supported glibc baseline from the build image and document it rather than claiming universal compatibility.

Artifacts:

```text
fastcord-vX.Y.Z-x86_64-pc-windows-msvc.zip
fastcord-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
fastcord-vX.Y.Z-aarch64-apple-darwin.tar.gz
fastcord-vX.Y.Z-x86_64-apple-darwin.tar.gz
fastcord-vX.Y.Z-source.tar.gz
SHA256SUMS
```

The publish job creates `fastcord-vX.Y.Z-source.tar.gz` with `git archive` of the checked-out commit, so it contains the pinned dav1d source archive and its provenance file in `crates/fastcord-video/native/`. libopus source is in neither archive: it is the `libopus_sys` 0.3.3 crate source pinned in `Cargo.lock` (used through `opus2` 0.4.0), which Cargo fetches from crates.io and builds with CMake. Its BSD-3-Clause notice is in `THIRD_PARTY_NOTICES.md`, which is included in the source archive and in every binary archive.

Windows ZIP contains the executable, required DLLs/runtime files, notices, and default assets. macOS archive contains a proper `.app` with Info.plist, framework/library paths, and permission descriptions. Linux archive contains the binary, selected redistributable libraries/assets, and a launcher only when needed for relative library paths.

Build/test/package jobs upload immutable artifacts. A final publication job gets `contents: write`, verifies all required artifacts/checksums, and creates the GitHub Release only after the matrix succeeds. Build jobs have no release write permission. Prerelease tags create prereleases. Release notes summarize changelog entries and known platform limitations honestly.

A dry-run workflow-dispatch path exercises the same packaging without publishing a real release. Signing/notarization credentials are owner-provided secrets: configure optional Windows signing and macOS Developer ID/notarization when available; otherwise explicitly label artifacts unsigned/unnotarized and test the documented local launch path. Missing signing keys must not produce a fake notarization claim.

Ship all required third-party notices (dav1d, libvpx if used, openh264 on Linux, Twemoji). H.264 patent licensing on Windows and macOS is provided by the OS codecs. On Linux, prefer a system VA-API driver; the `openh264` software fallback compiled from source does not inherit Cisco's binary patent coverage, so the Linux package must document this or load Cisco's prebuilt binary at runtime. [S18, S25]

## 14. Rejected alternatives

| Alternative | Concrete reason it loses |
|---|---|
| Electron/WebView shell | Introduces a browser runtime and does not satisfy the chosen native iced architecture; no evidence it can meet this project's idle envelope |
| Bot-oriented SDK as user-client core | Bot intents, readiness, authentication, and voice assumptions differ from lazy user subscriptions and Go Live; adapting it creates a second protocol layer |
| Raw MLS implementation written in fastcord | Duplicates cryptographic state-machine work already present in davey/libdave and expands security responsibility |
| Shipping both davey and libdave | Duplicates native/Rust crypto maintenance and makes interoperability failures ambiguous; choose one engine |
| DAVE later / transport encryption only | Officially incompatible with required post-March-2026 media sessions |
| zstd and zlib both in MVP | Adds another decompression path without a demonstrated product need; zlib-stream is the selected supported path |
| Full member/presence/history cache | Conflicts with byte-bounded memory and lazy user-client subscriptions |
| FFmpeg (library or subprocess) | Heavy MSVC/native build, 20–40 MB of DLLs, LGPL linking constraints, `ffmpeg-next` in maintenance mode, and zero-copy still needs per-OS interop; OS-native codecs plus `dav1d` cover the required codecs with smaller binaries (ADR 0001) |
| Browser-style WebRTC stack for the native baseline | Adds a second transport/signaling model and heavier integration; direct UDP matches the documented native route, with blocked-UDP limitations explicit |
| CPU RGBA conversion plus new iced image per video frame | Repeats conversion/upload/allocation and undermines memory/frame-time budgets |
| DeepFilterNet as the default | CPU/RAM cost conflicts with the low-resource goal; offered as opt-in High quality mode instead (ADR 0004) |
| OS emoji fonts as the sole renderer | Inconsistent asset/version/color support across targets; attributed Twemoji gives a deterministic baseline |
| Plaintext token configuration fallback | Turns a missing keyring into credential exposure; explicit memory-only login is safer and honest |

These are project design decisions, not benchmark claims about the alternatives.

## 15. Risks and explicit unknowns

| Risk | What breaks | Detection and resolution gate |
|---|---|---|
| U1: user Identify/profile/capabilities drift | Login rejected or READY parsed incorrectly | Milestone 6: sanitized live alt-account handshake plus READY/READY_SUPPLEMENTAL fixtures |
| U2: lazy subscription semantics | Stale channel/member/voice state or memory growth | Milestone 7: switch guilds while in voice; verify unsubscribe and reducer operations |
| U3: Go Live identity/DAVE group mapping | Stream opens but all frames fail E2EE | Milestone 24: official-client send/watch, independent stream MLS transcript, no parent-group fallback |
| U4: codec/profile/header-extension negotiation | Black video, incorrect RTX, or decoder failure | Milestones 25–31: codec-specific captures/vectors and official desktop/mobile compatibility |
| U5: hosted toolchain/runner/native build availability | CI or release never produces all targets | Milestones 1–2 and 27: pin actual toolchain/runners; prove native build recipes on all targets |
| U6: remote-auth protocol drift (QR login) | QR login handshake, ticket exchange, or token decryption stops working; Discord may demand a CAPTCHA | Milestone 5a: live handshake against the real gateway (hello, init, nonce proof, fingerprint, heartbeat acks) and a scan with the alt account's phone app; a CAPTCHA stops the flow with token paste as fallback, never a bypass |
| davey maturity and documentation gaps | Rekey, welcome, or frame transforms fail | Milestone 18: reference vectors and member-join/leave churn; block media completion on failure |
| Native decoder or malicious image input | Crash, excessive allocation, security exposure | Strict dimensions/lengths, vetted codec libraries, malformed fixtures, sanitizers in targeted development runs |
| UDP restricted by network | Voice/streams cannot establish | Timeout/IP-discovery diagnostics and explicit unsupported-route status; no fabricated relay support |
| Permissions/account entitlements | Channel reads, reactions, or stream creation forbidden | Local permission calculation plus surfaced REST/Gateway rejection, no retry storm |
| Capture/audio permissions, portals, old OS versions | Missing source, no frames/audio | Per-platform packaged-app tests with denial, revoke, resize, hotplug, and source-close cases |
| Loopback includes received call audio | Echo/rebroadcast of private conversation | Explicit source labels, platform exclusion or separate-output routing test, stream audio opt-in |
| GPU/driver/shared-memory variability | Idle target or stable video misses | Measure CPU and GPU residency on reference machines; lazy initialization and bounded pools |
| Account enforcement/ToS changes | Account suspension or permanent API incompatibility | Up-front risk warning and alt account; no technical guarantee or evasion mechanism |
| Signing keys and distribution permissions | OS warnings or blocked app launch | Release notes and packaged-app launch checks; owner must supply credentials for signed distribution |
| Codec/asset licensing | Release cannot legally redistribute a build | Notice audit before publication; OS codecs on Windows/macOS; Linux `openh264` policy per ADR 0001 |

A compatibility unknown is not permission to ship a stub. The relevant milestone stays incomplete until its acceptance criteria pass or the user explicitly approves a scope change.

### Interface sounds
Original synthesized tones (`ui_sound`, never copied from Discord) for mute/deafen, joining/leaving a call, others joining/leaving, push-to-talk, screen share and notification pings. Played by a short-lived output-only `FilePlayer` on the selected output device (soundboard path, at most 3 at once, nothing alive while idle); never sent to the call and not in the call engine's echo reference. Events come from snapshots of the call (`Watcher`), so shortcuts and buttons behave alike and the initial participant list is silent. Setting: "Interface sounds" (default on) and volume (default 40%) in the audio dialog.

## 16. Ordered milestones — exactly one commit per item

Each item is one coherent feature commit containing its implementation, deterministic tests, and the relevant README/protocol/testing/changelog update. Later acceptance does not retroactively excuse an earlier broken feature. Platform/media test fixtures are sanitized; live tests use the accepted alt account and a cooperating test participant. At drafting, none of the checks below had been run for this specification; an implementer performs them. Their results, and the live and hardware checks still pending for each milestone, are recorded in `docs/TESTING.md`.

Live-test platforms (see `docs/TESTING.md`): **Windows** — the maintainer's PC, run by the agent; **Linux** — a Hyper-V VM on the same PC (Wayland + PipeWire), run by the agent, with no GPU passthrough, so VA-API paths stay unverified until tested on real hardware; **macOS** — the maintainer's own Mac, run manually by the maintainer from a CI-built package. A macOS milestone is complete only after the maintainer reports its acceptance checks passing.

1. **Workspace skeleton + CI.** Add workspace, exact toolchain, minimal real iced window, model crate, lockfile, README, and three-OS CI. Acceptance: fmt/clippy/tests pass on all three OS families; the window opens/closes locally; no fake feature buttons.
2. **Tag-triggered release workflow.** Add real packaging, four target artifacts, checksum/source bundles, and gated publication. Acceptance: dry run packages the minimal app on all targets and packaged binaries launch without a Rust installation; a prerelease tag publishes the expected asset set atomically after successful builds.
3. **Typed protocol model and permissions.** Add snowflakes, entities, partial-message semantics, channel types, and overwrite calculation. Acceptance: owner/admin, role allow/deny, member overwrite, missing fields, and inaccessible channel fixtures pass.
4. **REST transport and rate-limit scheduler.** Implement route/bucket/global scheduling and error categories. Acceptance: deterministic clock tests cover shared buckets with different major IDs, fractional resets, global 429, concurrent reservation, and 401 stop.
5. **Token login and native credential storage.** Implement token-paste validation, saved login, memory-only mode, logout. Acceptance: correct account shown; invalid token stops; locked store is explained; restart restores only a saved token; logout removes it; secret-redaction tests pass.
5a. **QR login.** Remote-auth WebSocket, per-attempt RSA key pair, QR rendering in iced, pending-user confirmation, ticket exchange, then the milestone 5 storage path. Acceptance: scanning with the alt account's phone app logs in and stores the token; cancel/timeout/regenerate work; keys and tickets never reach logs; a CAPTCHA response stops with a clear message.
6. **Compressed user Gateway lifecycle.** Implement v10 zlib-stream, selected capabilities, READY normalization, heartbeat, Resume, reconnect, and auth stop. Acceptance: fragmented-compression fixtures, READY_SUPPLEMENTAL, replay/dedup, and a live reconnect work; accepted Identify profile is documented.
7. **Bounded state and lazy subscriptions.** Implement opcode 37, member-range operations, passive updates, and eviction. Acceptance: two-guild navigation emits only intended subscriptions; visible/member state remains correct; byte caps hold during large fixtures; no opcode-14 fallback.
8. **Guild/channel navigation.** Render virtualized guild/channel lists and permissions. Acceptance: accessible guild/text/voice channels appear, selection survives updates, forbidden channels cannot be opened, and navigation does not fetch every history.
9. **Message history and virtualized timeline.** Implement paging, partial updates, deletion, scroll anchoring, and bounded layout cache. Acceptance: page through 10,000 fixture messages without retaining all of them; prepending/image-height changes do not jump the viewport; widget count tracks viewport size.
10. **Send messages.** Add composer, pending/failed states, nonce reconciliation, explicit mention policy. Acceptance: official client receives one message; REST/Gateway ordering does not duplicate it; ambiguous failure does not automatically repost.
11. **Edit/delete own messages.** Add ownership-gated actions and partial-state reconciliation. Acceptance: own message changes synchronize with another client; foreign messages lack those actions; attachments are not accidentally removed by text edits.
12. **Replies.** Add reply composition, preview, reference loading, and jump-to-message. Acceptance: official client sees a genuine reply; uncached/deleted target behaves correctly; replied-user mention follows explicit setting.
13. **DMs and group DMs.** Add private conversation list/open/create using selected recipients and reuse the timeline/composer. Acceptance: DM and group DM history/send/edit/delete/reply work with official clients; gateway participant/channel updates appear; no bulk discovery/creation. (Group membership, owner, name, icon, leave and close, #78, build on this; see PROTOCOL.md "Group DM management (#78)". Message requests, Ignore and privacy, #80, keep a pending request out of the ordinary DM list and apply one suppression rule to ignored and blocked users everywhere; see PROTOCOL.md "Message requests, Ignore and privacy (#80)". Reporting, #81, is a review-and-submit flow over Discord's own server-driven report menu from a message's More row, the profile card and the server header; see PROTOCOL.md "Reporting messages, users and servers (#81)".)
14. **Attachment viewing and bounded CDN cache.** Add signed URL fetching/refresh, image view, file metadata, explicit download/open. Acceptance: large image thumbnails stay bounded; expired URL refresh works; cross-origin requests never carry token; unsupported files remain accessible via explicit action.
15. **Unicode/custom emoji and picker.** Add attributed assets, inline runs, sequence handling, custom/animated assets, search/recent picker. Acceptance: skin tone/ZWJ/flag round-trips; custom and animated emoji match official rendering semantics; off-screen animation stops; entitlement failure is visible.
16. **Reactions.** Add normal reaction counts/self-state and add/remove. Acceptance: Unicode/custom reactions synchronize bidirectionally with official client; typed delete route is captured; remove-all and uncached events do not leak state.
17. **Voice session and encrypted RTP transport.** Implement voice-v8 control, IP discovery, AEAD modes, RTP/RTCP parsing, SSRC mapping, and leave. Acceptance: both mode vectors pass; nonce/sequence wrap cases are distinguished; malformed headers reject safely; join handshake never sends plaintext media.
18. **DAVE/MLS integration.** Add pinned davey adapter and all required transitions/binary messages. Acceptance: reference frame/MLS vectors and official-client membership churn pass; rekey failure pauses media; stream-independent session state is possible; no transport-only downgrade.
19. **Audio engine and native device backend.** Add cpal ownership, format/rate conversion, rings, Opus encode/decode. Acceptance: local microphone loopback/codec round trip on each OS; allocation instrumentation shows no callback allocations; teardown stops callback threads.
20. **Voice playback, jitter, and mixing.** Connect DAVE media to adaptive jitter, FEC/PLC, mixing, and real call output. Acceptance: bidirectional official-client audio works under reordering/loss; four remote speakers mix without clipping/queue growth; joins/leaves release decoders.
21. **Voice controls and speaking UI.** Add join/leave controls, mute/deafen, per-user gain, speaking indicators. Acceptance: mute sends no audible microphone data; deafen also mutes capture; gains affect only selected user locally; speaking attribution survives SSRC changes. (Push-to-talk, push-to-mute, the voice-activity threshold and the global shortcuts, #82, feed this same capture gate: server mute/deafen, self mute/deafen and push-to-mute always close it, and push-to-talk, forced where the channel lacks "use voice activity", opens it for the key plus a release delay; see PROTOCOL.md "Push-to-talk and communication shortcuts (#82)".)
22. **Audio-device selection.** Add settings, persistence, system-default choice, hotplug/errors. Acceptance: switching input/output during a call works; unplugging an explicit headset does not silently route to speakers; restart restores valid choices.
23. **Noise suppression: Off/Light setting.** `Denoiser` trait, nnnoiseless with scaling/delay, persisted Off/Light setting. Acceptance: deterministic noisy speech fixture improves the selected quality metric without clipping; release cost is recorded against the 10 ms budget; Off/mute does no inference.
23a. **Noise suppression: High quality mode.** DeepFilterNet 3 behind the same trait, lazily loaded and dropped on switch-away. Acceptance: keyboard/background-voice fixture suppressed better than Light; measured CPU, RAM, and added latency recorded; switching modes mid-call causes no audio gap longer than one frame; RAM returns to the Light baseline after switching away.
24. **Go Live connection lifecycle.** Implement create/watch/pause/delete/ping signaling and independent stream DAVE group identity. Acceptance: real owner/viewer sessions establish independently of parent voice; correct stream MLS group is verified; failure/end does not kill parent voice; no claim of finished video yet.
25. **H.264 RTP receive/send and video feedback.** Add packetization/reassembly, DAVE frame boundary, RTCP/NACK/RTX/PLI, bounded pacer/retransmit buffers. Acceptance: fragmented/keyframe/loss fixtures reconstruct exactly; official-client H.264 stream packets decrypt and depacketize; feedback produces fresh-nonce retransmissions.
26. **AV1 receive (and VP8/VP9 only if U4 requires).** Add AV1 depacketizer, DAVE transform, and `dav1d` decode. Acceptance: AV1 packet/frame vectors and a real AV1 stream decode to expected frames; only supported codecs are advertised.
27. **Windows codec path.** Media Foundation H.264 encode/decode (hardware MFT preferred, software MFT fallback) behind the `video` codec trait. Acceptance: fixture H.264 round trip on the hardware and software MFT; the selected backend is reported; no FFmpeg dependency.
28. **Video playback in iced.** Implement frame leases, reusable YUV shader textures, aspect/color handling, visibility gating. Acceptance: a real watched stream displays without per-frame image encoding; resize/minimize works; texture/frame pool remains bounded and releases on unwatch.
29. **Windows screen-share capture/send.** Connect Windows.Graphics.Capture to Media Foundation H.264/DAVE/RTP. Acceptance: display and window share are visible in an official client at 720p30; resize/source close/permission denial are handled; microphone voice remains live.
30. **Linux codec path.** VA-API H.264 encode/decode with `openh264` software fallback. Acceptance: fixture round trip on a VA-API driver and on the software path; absent hardware does not prevent share/watch; no leaked surfaces after cycles.
31. **macOS codec path.** VideoToolbox H.264 encode/decode. Acceptance: hardware-backed 720p30 encode/decode on Apple Silicon; Intel package works through a tested path; pool lifetimes survive stop/start.
32. **Windows zero-copy video.** Share one D3D11 device between capture and the hardware encoder; hand decoded D3D11 surfaces to rendering without CPU readback where safe. Acceptance: measured CPU drop versus milestone 29 at 720p30; GPU reset/backend failure is visible and automatic mode recovers to the copy path.
33. **Linux screen-share capture/send.** Implement portal/PipeWire capture and release packaging requirements. Acceptance: supported Wayland desktop display/window share reaches official client; portal cancellation/session close works; unsupported portal setup is diagnosed.
34. **macOS screen-share capture/send.** Implement ScreenCaptureKit capture and bundle permissions. Acceptance: packaged app shares window/display to official client; denial/revoke/source close and Retina scaling behave correctly on supported macOS.
35. **Windows stream audio.** Add WASAPI loopback selection, Opus SOUNDSHARE, and common stream-audio receive/mix/synchronization. Acceptance: official viewer hears selected app/system audio aligned with video; microphone remains separate; fastcord output exclusion or explicit broad-capture warning is demonstrated.
36. **Linux stream audio.** Add PipeWire audio source/monitor selection and explicit routing UI. Acceptance: selected source reaches official viewer with sync; no assumption that screen portal provides audio; whole-output echo risk is correctly indicated and separate routing works.
37. **macOS stream audio.** Add ScreenCaptureKit audio and self-exclusion. Acceptance: official viewer hears stream audio with sync; source/permission changes stop only affected media; fastcord is excluded on supported mode.
38. **Performance and release acceptance.** Enforce final byte caps, idle scheduling, resource teardown, and documented measurements; fix violations in the same feature commit. Acceptance: matched resource comparison with the official client per section 12 and mean idle CPU <0.5% one core on the recorded primary scenario; 30-minute voice and 20-cycle share tests meet section 12; all four packaged targets pass their defined live/manual matrix; every supported MVP action is usable without a stub.

43. **Server-backed message search.** Guild and DM search through Discord's search routes with content, author, channel and attachment filters, bounded paging (100 previews, 128 KiB), stale-answer rejection, visible indexing/error states and jump-to-message via the history intent. Acceptance: filters return paginated server results and a result opens its exact context; no history crawl and no persisted plaintext. Wire shapes are unverified until checked on the alt account.

### Feature coverage

| Required capability | Milestones |
|---|---|
| Workspace + three-OS CI | 1 |
| Tag releases | 2, 38 |
| User login (QR + token) and secure token storage | 4–5a, 6 |
| Gateway, lazy subscriptions, bounded cache | 3, 6–9 |
| Guild/text lists and history | 8–9 |
| Send/edit/delete/reply | 10–12 |
| DMs and group DMs | 13, reusing 9–12 |
| Attachments view | 14 (images inline; video/audio attachments open in the OS default player — no media demuxer in MVP) |
| Unicode/custom/picker/animated emoji | 15 |
| Reactions | 16 |
| Voice join/leave and audio | 17–21 |
| Mute/deafen, per-user volume, speaking | 21 |
| Audio devices | 19, 22 |
| Noise suppression | 23, 23a |
| Screen-share watch | 24–28, 30–32 |
| Screen-share send on all OSes | 24–25, 27, 29–34 |
| Stream audio send/watch | 35–37 |
| Performance targets and packaged interoperability | 38 |

### Post-MVP everyday-client milestones (approved 2026-10-15)

The initial MVP above remains unchanged and must pass M38 before these features ship. The approved next product stage adds everyday messaging and community participation on all three operating systems. It supersedes the MVP-only exclusions for attachment uploads, presence and thread/forum participation only in these milestones; camera, server administration/moderation, rich activity integrations, plugins and selfbot automation remain out of scope. Audio/video attachments continue to open in the OS player. Track implementation and acceptance separately in [ROADMAP.md](ROADMAP.md); the umbrella issue is [#67](https://github.com/skadibtw/fastcord/issues/67).

39. **Message formatting, embeds and mentions.** Render Markdown, code blocks, quotations, spoilers, links and received embeds. Resolve user/role/channel mentions and add composer mention completion. Preserve source text and explicit mention notification policy. Acceptance: Common message formatting and supplied embed metadata render correctly with bounded layout/media caches. Copying message text preserves the original source; editing does not rewrite Markdown or mention IDs. Completion inserts the intended user, role or channel reference; allowed-mentions behavior matches the explicit user choice. Track: [#54](https://github.com/skadibtw/fastcord/issues/54). Implemented offline 2026-10-16; live checks pending (docs/TESTING.md). Embed thumbnails (at most 80 px) and images (at most 400x300) are fetched for the shown rows from the media proxy only, credential-free, through the avatar store (at most 16 per window, 24 MiB of decoded pixels, 8 MiB per file); until loaded or when it fails the `Thumbnail: host` link stays. Not in M39: per-line-spanning emphasis. `<t:…>` timestamps render in local time (offset read once at process start, UTC fallback; relative style computed at row build, no timer) as a follow-up under #54.
40. **Attachment uploads.** Send user-selected files through a picker, drag-and-drop and explicit image paste. Show progress, cancellation and explicit retry. Reuse nonce-aware message reconciliation. Acceptance: Official Discord receives the selected attachments and optional text exactly once, including REST/Gateway reordering. Upload reads files incrementally with byte-bounded buffers, not a whole-file in-memory allocation; cancellation closes work and removes temporary data. Account/server size limits and permission failures are shown without losing the draft; ambiguous final POST is not blindly repeated. Implemented offline 2026-10-16; live checks pending (docs/TESTING.md). Track: [#55](https://github.com/skadibtw/fastcord/issues/55).
41. **Read state, unread markers and mentions.** Track read cursors and mention counts; expose first-unread navigation and mark-as-read. Synchronize read state across clients without treating last-message ID as a read cursor. Acceptance: New messages and mentions update channel/DM markers; first-unread navigation opens the correct position. Reading and explicit mark-as-read converge with the official client across reconnect and fresh READY. (The later unified unread/mention inbox, #77, reuses this table; see PROTOCOL.md "Unified inbox (#77)".) Browsing older history does not mark unseen newer messages read. Implemented offline 2026-10-16; live checks pending (docs/TESTING.md). Track: [#56](https://github.com/skadibtw/fastcord/issues/56).
42. **Native desktop notifications.** Add native message/call notifications, server/channel mute behavior, DND, preview privacy and navigation to the originating message. Honor the account settings supported by the client. Acceptance: Allowed events produce one notification; replay after Resume or refreshed history produces no duplicate. Server/channel suppression and DND are applied consistently; hidden-preview mode omits message text. Selecting a notification opens the correct conversation/message and handles deleted or inaccessible targets. Track: [#57](https://github.com/skadibtw/fastcord/issues/57). *Implementation status:* `fastcord-platform::notify` shows a toast with a click callback on Windows (WinRT `ToastNotification`, `Activated`), Linux (`org.freedesktop.Notifications` over the session bus, `ActionInvoked` with the `default` action) and macOS (`UNUserNotificationCenter`, packaged `.app` only); at most 8 clicks are waited for. A click opens `discord://-/channels/{guild|@me}/{channel}/{message}` through the second-launch hand-off, so a deleted or inaccessible target is handled like any link. The account's Do Not Disturb status silences toasts; hidden previews name only the author. Native checks are pending (docs/TESTING.md).
43. **Server-backed message search.** Search a guild or DM with author, channel, date and attachment filters, paginated results and jump-to-message. Do not crawl full history or introduce a persistent plaintext message database. Acceptance: Queries and filters return paginated server results and opening a result loads the exact message context. Out-of-order results from old queries are ignored; cancellation, indexing delays, errors and 429 are visible. Results and previews obey explicit byte limits; inaccessible/deleted messages fail gracefully. Track: [#58](https://github.com/skadibtw/fastcord/issues/58).
44. **Pinned-message browser.** Show a paginated pinned-message list and jump to each message. Pin and unpin of a message from its row actions is included, where permitted (DMs, or Manage Messages); the list is read on open and Refresh (no `CHANNEL_PINS_UPDATE` yet). Acceptance: The list matches official-client pins and reflects pin/unpin changes made elsewhere. Opening a pin selects the correct channel and message context. Deleted messages, changed permissions, stale list requests and pagination are covered. Track: [#59](https://github.com/skadibtw/fastcord/issues/59). Later free message actions (#76: copy link/ID, in-app message links, hide/restore link previews, `@silent` sends, spoiler/alt text on new files, removing attachments in an own-message edit) are specified in PROTOCOL.md "Message actions (#76)".
45. **Friends, relationship actions and profiles.** Add friends and incoming/outgoing requests, accept/decline/remove, block/unblock, a basic user profile and open-DM actions. Every relationship mutation must follow an explicit user action. Acceptance: Lists and individual actions converge with an official client across READY/reconnect. Accept/decline/remove/block/unblock show pending, success and actionable failure without duplicate mutation retries. Profile/open-DM navigation works from message authors and the friend list. Track: [#60](https://github.com/skadibtw/fastcord/issues/60). *Implementation status (offline checks only, see TESTING.md):* the reducer keeps at most 2000 relationships in its own byte-accounted table, the Direct Messages sidebar opens a Friends screen (All, Pending, Blocked, add by username) and message authors open a profile popup; there is no Online tab because presence is milestone 46, and live convergence with an official client is still to be checked.
46. **Presence and own status.** Display available contact presence and support setting the account online, idle, DND, invisible and a text status. Rich activity integrations and continuous polling are excluded. Acceptance: Own status changes are reflected in the official client and incoming changes update fastcord. Presence subscriptions are restricted to needed users and removed when no longer referenced. Invisible and unavailable presence are represented honestly rather than inferred from activity. Track: [#61](https://github.com/skadibtw/fastcord/issues/61).
47. **Direct and group calls.** Complete incoming/outgoing DM and group-DM call flows: ring, accept, decline, end and missed-call presentation, reusing the voice/DAVE and screen-share engines. Acceptance: Official-client calls work in both directions for DM/group DM, including mute/deafen/device settings and screen sharing. Only one parent voice connection exists; switching between guild voice and a private call ends the previous session correctly. Cancel-before-answer, answer elsewhere, disconnect, reconnect and participant changes do not leave ringing or stale media workers. Track: [#62](https://github.com/skadibtw/fastcord/issues/62). *Implementation status (offline checks only, see TESTING.md):* the reducer keeps bounded ringing/connected call state from `CALL_*` and guild-less voice states, the DM header has a Call/Join call button (opcode 4 with a null `guild_id`, then `POST /channels/{id}/call/ring`), an incoming-call bar offers Accept/Decline (`POST .../call/stop-ringing`), Leave cancels an unanswered call, and type-3 messages read as started, missed or ended; live interoperability with an official client is still to be checked.
48. **Text thread participation.** Navigate, read, reply to and create text threads; join/leave and access archived threads where allowed. Reuse history/composer and lazy subscriptions rather than duplicating the timeline. Acceptance: Public/private accessible threads appear correctly and inaccessible threads are never exposed. Create/reply/join/leave operations converge with official clients; archive and lock restrictions are shown accurately. Pagination and archived access work without eagerly fetching all threads or histories. Track: [#63](https://github.com/skadibtw/fastcord/issues/63). *Implementation status (offline checks only, see TESTING.md):* threads are channels in the reducer's byte-accounted store (at most 1024 per guild), listed under their parent while active, opened through the normal history/composer path, with join/leave, create from a message, an archived public list (25 per page) and a thread-created row that opens the thread; private archived threads, the joined-archived listing, and live convergence with an official client are still to be checked.
49. **Forum posts and replies.** Browse forum posts and tags, open posts, create a post and reply using thread infrastructure. Creating/configuring forum channels and moderation are excluded. Acceptance: Post lists, tags and paginated/archive views match accessible official-client content. New post title/body/attachments/tags and replies appear correctly in the official client. Required tags, permissions, archived/locked state and stale/deleted posts fail clearly without losing drafts. Track: [#64](https://github.com/skadibtw/fastcord/issues/64). *Implementation status (offline checks only, see TESTING.md):* forum and media channels (types 15 and 16) open a post list built from the reducer's active threads under the forum (pinned first, then activity or creation order, tags as chips, tag filter, at most 200 posts), archived posts through M48's archived listing, a per-forum new-post draft with title, body, tags (REQUIRE_TAG, at most 5, moderated tags only with Manage Threads) and files through the existing upload path, `POST /channels/{forum}/threads` sent once with unconfirmed-answer handling, and replies through the normal thread timeline and composer; the route shapes, the user-client search route (not used), permission edge cases and comparison with the official client are still to be checked live.
50. **Everyday keyboard navigation and session drafts.** Add a channel/DM quick switcher, keyboard navigation and per-conversation drafts retained in memory for the active session. Preserve composer metadata when switching contexts. Acceptance: Switching channels/DMs preserves text and reply/attachment intent within a bounded draft budget; draft loss is never silent. Quick switch and keyboard navigation work without loading all message histories. IME composition, focus restoration and navigation from search, pins and notifications work on Windows/Linux/macOS. Track: [#65](https://github.com/skadibtw/fastcord/issues/65). *Implementation status (offline checks only, see TESTING.md):* drafts (`drafts.rs`) hold the composer text, reply target with its notification choice, chosen files (paths and metadata only) and the `@everyone` choice per conversation (channel, thread, DM) in memory; at most 64 drafts and 256 KiB of text, the least recently left goes first and every eviction is counted in a red, dismissible line above the composer, never silent. Nothing is written to disk and logout clears them. Leaving a conversation while editing a sent message discards the edit (visible notice; the unsent draft underneath is kept). The quick switcher (`Ctrl+K`, `switcher.rs`) searches a reducer-side bounded query over accessible channels, held threads, DMs and group DMs (`Navigation::quick_switch`, at most 50 results, name prefix, word start, substring then subsequence match, mentions and unread first, no history, no network) and opens a result like a sidebar click (`SelectionIntent::Open`; a voice or Stage row opens its chat, never the call). Keyboard stepping (`Navigation::step`): `Alt+Up/Down` channel, `Alt+Shift+Up/Down` unread, `Ctrl+Alt+Up/Down` server, `Escape` closes overlays then cancels edit or reply, `F1` or `Ctrl+Shift+?` help; only modified chords (and Escape/arrows while an overlay is open) are read, so IME composition is untouched, and the composer regains focus after navigation and after closing overlays. Drafts follow the open conversation, so navigation from search, pins, the inbox and notifications restores them too. Native IME/focus checks on Windows, Linux and macOS are pending.
51. **Everyday-client release acceptance.** Validate M39-M50 together on Windows, Linux and macOS, repeat M38 performance/resource gates and document supported behavior and remaining limitations. Acceptance: Messages, uploads, read-state, notifications, search/pins, friends/status, private calls and thread/forum participation work end-to-end against official clients. Authenticated idle resource use is compared with the official client per section 12 on the primary Windows scenario, with mean CPU below 0.5% of one logical core over ten minutes; report other platforms and GPU memory separately. Thirty-minute voice, twenty share cycles, four packaged targets and return within 10 MB of warmed memory two minutes after media teardown pass the SPEC matrix. Track: [#66](https://github.com/skadibtw/fastcord/issues/66).

### Full free communication stage (approved 2026-10-16, after M51)

The maintainer expanded the post-M51 scope to all free communication capabilities. [COMMUNICATION_COVERAGE.md](COMMUNICATION_COVERAGE.md) is the ordered extension of this milestone list; [tracker #109](https://github.com/skadibtw/fastcord/issues/109) maps the existing work and issues #69-#108. Implement its forty rows in order after M51, respecting their prerequisites and one feature per commit. #108 is the final free-communication acceptance gate, while M51/#66 retains its narrower original meaning.

This stage supersedes prior exclusions for camera/video, inline media playback, communication-related server/channel/role administration and moderation, community tools, manually invoked app commands and free non-game collaborative activities. It preserves the native architecture, single-writer reducer, byte bounds, no FFmpeg and no selfbot automation. Paid/Nitro entitlements, billing/boosts, premium cosmetics or limits, subscription-only bookmarks/reminders, game library/game activities and store/ads remain excluded. Receiving legitimately accessible content from paid clients is not a premium sending feature. Each issue records free-account/platform eligibility, concrete acceptance, references and dependencies; an unsupported route or missing native check remains pending.

## 17. Sources and rationale evidence

At drafting, no implementation existed yet; `LICENSE:1–35` was the only repository codebase-adjacent evidence read. The files listed above were the intended homes for the decisions, tests, and documentation at that time; current implementation status is recorded in `docs/TESTING.md`, `docs/PROTOCOL.md`, and `CHANGELOG.md`. The following sources establish protocol/library/platform facts, not fastcord performance or implementation success.

- **S1 — User Gateway lifecycle, capabilities, intents, compression, rate limits:** https://docs.discord.food/gateway/using-gateway
- **S2 — User Gateway opcode table (14 deprecated; 37 bulk subscriptions):** https://docs.discord.food/gateway/opcodes-and-close-codes
- **S3 — Concrete user-client Identify/Resume/subscription payloads and member-list types:** https://github.com/dolfies/discord.py-self/blob/master/discord/gateway.py and https://github.com/dolfies/discord.py-self/blob/master/discord/types/gateway.py
- **S4 — Official REST rate-limit semantics:** https://docs.discord.com/developers/topics/rate-limits
- **S5 — User message/reply/edit/reaction routes and fields:** https://docs.discord.food/resources/message
- **S6 — User guild/private channel routes:** https://docs.discord.food/resources/channel
- **S7 — Signed attachment URL refresh:** https://docs.discord.food/topics/cloud-uploads
- **S8 — Official voice v8, March 2026 DAVE requirement, heartbeat, crypto modes:** https://docs.discord.com/developers/topics/voice-connections
- **S9 — Native video, codecs, SSRCs, rtpsize boundaries, stream identities and audio:** https://docs.discord.food/topics/voice-connections
- **S10 — Opus2 backend/build features:** https://docs.rs/crate/opus2/0.4.0
- **S11 — Davey direct Rust/OpenMLS implementation and codecs:** https://docs.rs/crate/davey/0.1.4 and https://docs.rs/davey/0.1.4/davey/enum.Codec.html
- **S12 — Official libdave C++ and native dependencies:** https://github.com/discord/libdave and https://github.com/discord/libdave/blob/main/cpp/README.md
- **S13 — DAVE protocol and transform ordering:** https://github.com/discord/dave-protocol/blob/main/protocol.md
- **S14 — cpal device/configuration and callback threading:** https://docs.rs/cpal/latest/cpal/
- **S15 — nnnoiseless state, sample scaling, and processing:** https://docs.rs/nnnoiseless/0.5.2/nnnoiseless/struct.DenoiseState.html
- **S16 — RNNoise original complexity estimate:** https://arxiv.org/pdf/1709.08243
- **S17 — Go Live main-Gateway signaling:** https://docs.discord.food/gateway/gateway-events
- **S18 — OpenH264 Rust encode/decode binding:** https://docs.rs/crate/openh264/0.9.8
- **S19 — OS-native codecs and dav1d:** https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-encoder ; https://developer.apple.com/documentation/videotoolbox ; https://code.videolan.org/videolan/dav1d
- **S20 — Windows.Graphics.Capture:** https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture
- **S21 — Windows application loopback capture and minimum build:** https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample
- **S22 — Linux ScreenCast portal lifecycle and PipeWire targeting:** https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
- **S23 — ScreenCaptureKit screen/audio and permissions:** https://developer.apple.com/documentation/screencapturekit
- **S24 — iced 0.14 shader integration:** https://docs.rs/iced_widget/0.14.2/iced_widget/shader/trait.Program.html
- **S25 — Twemoji and graphics attribution:** https://github.com/jdecked/twemoji/blob/main/README.md
- **S26 — Keyring ecosystem and selective credential backends:** https://github.com/open-source-cooperative/keyring-rs

### Voice panel (issue #115)
The conversation pane has no call bar. In a call a *Voice Connected* panel sits at the bottom of the 240 px sidebar above the account footer (mute, deafen, profile, safety, audio, log out stay in the footer): signal icon and state (green running, amber connecting, red audio failure), `channel / server` or the DM name (click: large call view, plus the call channel's chat for a server call), a red Disconnect; then icon buttons for camera, share screen (stop while sharing), soundboard (server calls) and call settings. Settings (an inline panel above it, at most 320 px, scrollable) hold the camera device, effect, Mirror preview, Find cameras, camera status, per-person volume with presets, server restrictions and Watch/Stop watching for streams. Sidebar occupants under every voice row show a speaking ring (own call), LIVE for `self_stream`, a camera glyph for `self_video`, and muted/deafened glyphs; the voice states come from the existing selected-guild subscription, READY_SUPPLEMENTAL and PASSIVE_UPDATE_V2.

### Text-to-speech and read-aloud (#104)

Local only: `fastcord-platform::speech` (one dedicated thread; bounded queue of 8 utterances x 2 KiB; `Stop` clears it. Windows: WinRT `SpeechSynthesizer` played by a `MediaPlayer`. Linux: the speech-dispatcher daemon through SSIP on its Unix socket (`SPEECHD_ADDRESS` or `$XDG_RUNTIME_DIR/speech-dispatcher/speechd.sock`, started once with `speech-dispatcher --spawn` like libspeechd), end/cancel notifications drive the queue, `CANCEL self` never silences a screen reader; `Unsupported` without the daemon. macOS: one `/usr/bin/say` process per utterance, text on standard input, killed by Stop) and the app's `speech.rs` (settings, plain-text extraction from the markup spans, the policy for received TTS messages, the process-wide speaker started off the calling thread). Explicit read-aloud lives in the message More row. Sending: a leading `/tts` word sets `tts: true` in the create-message body (`send::strip_tts`, same convention as `@silent`) and the composer refuses it without SEND_TTS_MESSAGES (`PermissionSummary::send_tts`, reducer-computed). Received `tts: true` messages are spoken per the setting Never / For the selected channel only / All channels (default Never), never when own, muted, suppressed or DND. Narration stops when the session ends. No account text goes to any cloud service.

### Chat and accessibility preferences (#103)

`chat_prefs.rs` holds the local preferences: text scale (85-150%, applied once through iced's window `scale_factor`, which scales text and layout together rather than each `.size()`), compact or cozy rows (`Prefs::row_layout`, used by the timeline's one row builder), reduced motion and animated-emoji autoplay (`Prefs::animated`, gating the emoji tick subscription and the displayed frame), spoiler reveal, link embeds, inline attachment previews and players, 12h/24h time (`localtime::set_hour12`, read by headers and `<t:...>` markup) and a high-contrast palette (`style::pane`, `text_color`, `muted`). They persist in the audio settings file and are mirrored into one process-wide copy; no message content is stored. Not implemented: screen-reader semantics (iced 0.14 has no accessibility tree; this stays an outstanding gate), OS spellcheck (unavailable in iced text editors), locale-derived time and date formats, and the multi-OS keyboard-only/IME acceptance runs. Attachment GIFs are shown as a still preview and media never autoplays, so autoplay and reduced motion only affect emoji.

### Conversation organization and notification settings (#106)

**2026-10-22 safety amendment:** notification-setting writes are quarantined pending independent validation after a reported suspicious-activity restriction while using Alerts (causation unconfirmed). Opening Alerts sends no request; the panel reads held settings and retains local hide-muted. All authenticated work and Gateway reconnects stop on account-verification codes 40002/50178, connection revocation 40012 or CAPTCHA, in addition to HTTP 401. Recovery is through the official client; no guarantee against account restrictions is made. This amendment overrides the write-enabled behavior described below.

The reducer keeps the account's server folders (`Store::folders`, decoded by `fastcord-discord::user_settings` from READY `user_settings.guild_folders` or the protobuf `guild_folders`, replaced whole by `USER_SETTINGS_PROTO_UPDATE`/`USER_SETTINGS_UPDATE`) and notification levels and suppressions (`state/read.rs`, from `user_guild_settings`). `Navigation::guild_window` flattens the rail: unplaced (new) servers first, then the settings' order, each folder a header row (`GuildRow::folder`) followed by its servers while the local layout has it open. `Navigation::set_layout` takes the local, per-account `Layout` (open folders, folded categories, servers hiding muted channels), which the app keeps in the settings file (`organize.rs`) and publishes in one process-wide slot read by the Gateway worker. `channel_window` drops folded and, on request, muted channels but never the open channel, unread ones or voice channels with occupants, and marks a category `collapsed`. `Store::notify_level` (channel override, then server, then the server default) feeds `Store::unread`, `note_read_effects` and the desktop-notification policy; `Store::notifies_account` applies the suppressions. The sidebar's "Alerts" button opens `notify_settings.rs`, the one place that sends `PATCH /users/@me/guilds/{g|@me}/settings` for mute durations, levels and suppressions (docs/PROTOCOL.md). Folder reordering, favorites and push preferences are not implemented.

### Desktop communication integration (#105)

Windows is the target; other OSes return `Unsupported` with a reason from `fastcord-platform` and nothing is worked around (no Wayland or macOS tray, protocol or overlay substitutes). Each native piece has its own thread that blocks in `GetMessageW`, so none polls.

- **Single instance** (`platform::instance`, always on): `main` takes the per-session mutex `Local\fastcord-single-instance` and a hidden message-only window before anything else starts. A second launch finds that window, sends its first command-line argument as `WM_COPYDATA` (tag `0x46430001`, at most 2 KiB of control-free UTF-8; an empty argument only raises the window) and exits without starting a session, so two processes never share a token. If the mutex cannot be created it also exits. The hand-off is validated on receipt (tag, size, UTF-8, control characters); the primary only enqueues it on a bounded channel read by a Subscription. Off Windows there is no guard yet.
- **Tray** (`platform::tray`, `app::desktop`): a hidden top-level window and `Shell_NotifyIconW` with the committed `fastcord.ico` (the 32 px entry, embedded with `include_bytes!`). Left click opens; the menu is Open, Mute/Unmute microphone and Disconnect (only in a call), Quit; the tooltip shows the unread mention count (`Inbox.mentions`). The icon re-adds itself when Explorer restarts. State reaches it through `tray::set_state`, which posts one message only when the state changed.
- **Closing** is a setting, default Quit: `exit_on_close_request` is off and `CloseRequested` goes through `desktop::close_action`, which hides the window (`Mode::Hidden`, not drawable, so video and read marking pause) only when the user chose the tray and the icon is up. Quit leaves the call (opcode 4) and exits after 300 ms; it is the tray's Quit and the window close without the tray setting, and the only way a call ends with the window.
- **Autostart** (`platform::autostart`, Windows, off by default): the checkbox writes or deletes `HKCU\...\CurrentVersion\Run\fastcord` with the quoted `current_exe`; nothing else writes the registry (reading at startup is read-only).
- **Deep links** (`desktop::parse`): `discord://-/channels/{g|@me}/{c}[/{m}]`, `discord://-/invite/{code}`, `https://discord.com|ptb|canary|discordapp.com/channels/...` and invite links, from the launch argument, a second launch or Enter in the quick switcher. A channel link opens the conversation (a message link jumps like a clicked message link, through the shared `jump_to`); before sign-in it waits as `pending_link`. An invite link only opens a panel with the invite and a Join button: joining stays one explicit click. Registering `discord://` as the OS URL handler is not implemented (it would take over the official client's handler); `fastcord.exe <link>` works.
- **Compact call mode** (`Message::ToggleCompact`, Pin button in the voice panel): the main window becomes a 300x260 always-on-top window showing who is speaking (speaking rings), the account's own mute and deafen buttons, Disconnect and a way back; it ends with the call. A second iced window (`daemon`) was judged too invasive for this milestone. No game injection or hooking of other processes: an overlay inside other apps is not offered and is not advertised.
- **Notification actions**: not implemented. The #57 toast is a plain WinRT toast for an unpackaged app; activating it into the running process needs a COM activator registered for an AUMID (a Start-menu shortcut), which this build does not create.

### 8.23 Manual moderation and audit log (#95)

Implemented in `fastcord-discord::member` (the actions, `MemberPermissions`, `Store::member_action_allowed` and `can_move_to`), `fastcord-discord::moderation` (ban list, bulk deletion rules, audit-log parsing), `RestRequest::with_audit_reason`, `fastcord-app::member_menu`, `fastcord-app::moderation` (Bans and Audit log panels, opened from the server menu), `fastcord-app::history::moderation` (deleting other messages, Select messages, reaction removal) and the thread tools in `fastcord-app::threads`. Routes are in `docs/PROTOCOL.md`, all unverified.

- **Member menu.** Server mute/deafen, disconnect, move, timeout, nickname (own and others), kick and ban with a delete window; kick, ban, disconnect and timeout need an explicit Confirm; an optional reason goes to the audit log for kick, ban, timeout and nickname. The reducer decides every control (permission, hierarchy, administrators cannot be timed out, destination channels) and checks again before sending.
- **Messages.** Manage Messages shows Delete on other people messages (same Confirm and change tracker as own deletions), a header "Select messages" mode (at most 100 messages under 14 days old, one bulk request after Confirm) and Remove / "Clear all of this emoji" in the who-reacted list.
- **Panels.** Bans (paged, at most 500 held, Unban with Confirm) and Audit log (50 per page, at most 4 pages held, filters by action type and user ID). Neither is polled or kept after closing.
- **Threads.** Archive/Unarchive and Lock/Unlock in a thread header for Manage Threads.
- **Not included.** Automated moderation, member search or bulk member lists, pruning, member screening review, timeouts longer than a week, reasons for message deletion.

### 8.24 App access and webhooks (#101)

`fastcord-discord::apps` holds the requests and bounded parsers (100 rows, 80-character webhook names, the token in a zeroizing type that never formats); `apps.rs` in the app is one panel state machine for authorized apps (account panel), a server's integrations (server settings, MANAGE_GUILD) and a channel's incoming webhooks (channel edit panel, MANAGE_WEBHOOKS, beside the #98 follower list, which stays the only home of type 2). Permissions come from the reducer's admin read model and are re-checked at the click. Removals need a Confirm; one request in flight; stale answers for a closed or changed panel are ignored; logout and account change clear everything including webhook tokens. Connections reuse the profile editor and gain the activity toggle. Adding an app is a browser hand-off to Discord's authorize page, never a native grant. No game, store or paid-app features; webhooks are configured, never posted to. All routes unverified (PROTOCOL.md).

### 8.25 Shared Activities (#102): not implemented, awareness only

Maintainer decision 2026-10-22: no native Activities. Running one needs an embedded webview (WebView2/`wry`: a second browser engine, tens of MB of RAM and a web attack surface next to the user token) and the Embedded App SDK bridge between the page and the client, whose protocol is unverified and not captured. Neither fits the RAM and attack-surface goals, so fastcord adds no webview, no dependency and no join or launch. It only shows that one runs: `EMBEDDED_ACTIVITY_UPDATE`/`_V2` is kept per guild voice channel (at most 3 per channel and 32 per guild, names cut to 64 characters), shown as an "ACTIVITY" line under the channel in the server list and, in the call view, as "Running an Activity — join it in the official Discord client". Games stay ordinary presence. The full scope of #102 (join, invite, leave, interoperation, session end) is an outstanding gate.
### 8.26 AutoMod and communication safety (#96)

Implemented in `fastcord-discord::automod` (rule model, Discord's limits as `validate`, create/edit bodies that keep unknown fields, REST calls, the `AutoModFeed` of Gateway changes), the safety fields of `GuildSettings` and `GuildEdit`/`set_mfa_level` in `guild_admin`, `fastcord-app::automod` (the panel) and the server settings form in `fastcord-app::guild_admin`. Routes are in `docs/PROTOCOL.md`, all unverified. This is manual configuration of Discord's own AutoMod; there is no client-side moderation engine.

- **Rules panel.** Opened from the server menu for Manage Server; the list is read on open and dropped on close. Rows show trigger, counts, actions and exemptions with an On/Off switch, Edit and Delete (Confirm). The editor offers what each trigger allows (keywords and patterns, word lists, mention limit, block message, alert channel, timeout, exempt roles and channels), checks Discord's limits before sending and keeps unknown metadata and actions.
- **Live updates.** The Gateway worker keeps the last 8 rule changes in the navigation snapshot; the open panel applies the ones it has not seen. Create and edit also apply Discord's answer.
- **Blocked sends and alerts.** A send refused with 200000/200001 shows Discord's reason and stays an unsent outbox item. Type 24 messages read as an AutoMod flag with the rule and user.
- **Safety settings.** Verification level, safety alerts channel and the owner's two-factor requirement sit in the server settings form next to the media filter.
- **Not included.** `AUTO_MODERATION_ACTION_EXECUTION`, changing raid or DM spam protection (a `features` update replaces the whole list), creating member profile rules, client-side filtering.
