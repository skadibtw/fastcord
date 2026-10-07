# fastcord Technical Specification

## 1. Decision

Build fastcord as a single-process, native Rust desktop application using Rust 1.99.0 stable, the MSVC target on Windows, and iced 0.14 with its wgpu renderer. Separate Discord protocol/state, real-time audio, video/capture, platform integration, and presentation into a small acyclic Cargo workspace. Use a direct user-account Gateway/REST implementation rather than adapting a bot framework; a bounded, single-writer state store; native UDP/RTP media with both required transport AEAD and DAVE; `davey` for the DAVE/MLS implementation; `opus2` with bundled libopus; `cpal` for microphone/playback; and `nnnoiseless` for microphone noise suppression. Use OS screen-capture APIs, H.264 as the interoperable send baseline, OS-native video codecs (Media Foundation, VideoToolbox, VA-API) plus `dav1d` for AV1 decode (no FFmpeg; see `docs/adr/0001-video-codecs.md`), and iced shader rendering for video. All networking, decoding, and audio processing stay outside the UI thread. Ship Windows, Linux, and macOS artifacts through GitHub Actions, with bounded caches and event-driven rendering designed around an idle process-memory target below 150 MB.

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

The repository inspection found only `LICENSE`, whose opening identifies the GNU GPL version 3. There is no existing application architecture to preserve. Every path in this specification other than `LICENSE` describes a proposed implementation.

Protocol facts cited from official Discord documentation are stronger evidence than user-client behavior documented by Discord Userdoccers or observed in `discord.py-self`. Community-documented behavior is explicitly a compatibility dependency, not a public API guarantee. Design budgets and acceptance thresholds below are requirements, not measured results. No implementation benchmark or live-account interoperability check has been performed for this specification.

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
  fastcord-platform/src/    # credential store, OS capture/audio, native handles
  fastcord-app/src/         # binary, iced UI, orchestration, settings, image cache
assets/emoji/              # versioned index and attributed assets
native/                    # pinned native dependency build recipes/manifests
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

Identify properties use a centralized, versioned **web-client profile** (ADR 0005): `browser: "Chrome"`, a matching `browser_user_agent`/`browser_version`, truthful `os`/`os_version`/`system_locale`, `release_channel: "stable"`, and a current `client_build_number`. The build number is fetched from discord.com web assets at startup (cached with a timestamp) with the value bundled at release time as fallback. Keep the profile stable across reconnects; never randomize it. **UNKNOWN U1:** server acceptance of this profile with the selected capability set is not established. Milestone 6 must record a sanitized accepted payload and stop on a server challenge; this spec does not authorize CAPTCHA bypass. **UNKNOWN U1b:** the official web client negotiates voice through WebRTC, whereas fastcord uses the native UDP voice protocol; whether a web profile plus `protocol: "udp"` is accepted or flagged must be verified in milestone 17 on the alt account.

### 4.4 Lazy subscriptions

Use opcode 37, `{"subscriptions": {guild_id: subscription}}`; opcode 14 is the deprecated predecessor and is not a permanent fallback. Track desired subscriptions separately from the last transmitted set. [S2, S3]

- Subscribe to the selected guild and the guild containing the active voice channel.
- Request member-list ranges only for the visible channel/sidebar viewport, rounded to protocol range blocks; begin with one 100-entry range and no more than three live ranges per visible list.
- Request individual members only when required for visible authors, reply targets, voice participants, or permission computation; evict unreferenced members later.
- Disable activities and typing subscriptions because neither presence dashboards nor typing indicators are MVP requirements.
- Remove unused ranges on navigation; coalesce rapid selection changes.
- Implement `SYNC`, `INSERT`, `UPDATE`, `DELETE`, and `INVALIDATE` member-list operations with index-correct behavior.
- Consume passive guild updates so unsubscribed guild metadata does not become permanently stale.

**UNKNOWN U2:** exact unsubscribe/partial-update semantics for the current server must be captured in the opcode-37 fixture and verified with two guilds, including keeping voice membership fresh while browsing another guild. Do not assume an omitted field clears a subscription. [S2, S3]

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
- In-app attachment view supports images and common media formats included in the media build; other files expose type, name, size, and user-driven open/download.
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

Use preallocated rings with approximately 40 ms normal buffering and a hard 100 ms cap. On an output underrun emit silence and count it; on capture overrun discard the oldest stale input rather than increasing latency indefinitely. Session teardown and device replacement happen on the owner thread.

### 7.2 Jitter, mixing, and controls

- Per-SSRC adaptive jitter target: 40 ms initial, 20–120 ms range; estimate interarrival jitter from timestamps.
- Reorder packets and discard duplicates/too-late packets. Use Opus in-band FEC when the next frame is available; otherwise use PLC.
- Keep encoded queues bounded, including unknown SSRCs; release inactive decoder state after a short inactivity interval while retaining required DAVE member state.
- Mix in f32 without clipping intermediate sums; apply a final limiter. Per-user volume is a local 0–200% gain, persisted by account/user ID, with short ramps to avoid clicks.
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

DeepFilterNet is not the default because its CPU/RAM cost (estimated 5–15% of one core, +10–20 MB) conflicts with the low-resource goal; it is opt-in for users who need Krisp-like suppression of non-stationary noise (keyboard, voices). Record measured CPU, RAM, and latency for both engines on the reference PC in milestones 23 and 23a. Noise suppression is not acoustic echo cancellation; recommend a headset and do not claim speakerphone-quality AEC.

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

Use a small pacer queue capped at 100 ms. Initial congestion policy: reduce bitrate by 20% when receiver loss exceeds 5%, the pacer exceeds its cap, or RTT grows more than 100 ms above the running minimum; increase slowly (at most 5% per stable interval) after several seconds below 2% loss. Honor a lower server/receiver bitrate request immediately. Lower resolution/frame rate before queueing stale frames. These values are design defaults to validate, not claims about Discord's proprietary congestion controller.

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

Loopback capture must not unknowingly retransmit the call itself. On Windows/macOS exclude fastcord when the selected native mode supports it. On Linux, require an explicit monitor/application routing choice and explain when whole-output capture includes call audio; provide separate-output routing instructions. Audio capture is opt-in and visibly indicated. A source without audio may share video normally, but a failed requested audio source is an error shown to the user, not silent success.

## 9. Emoji and reactions

Use a versioned Unicode emoji index with grapheme-aware parsing for variation selectors, skin-tone modifiers, flags, and ZWJ sequences. Preserve original Unicode text for copy/paste and sending. Render recognized emoji as consistently sized inline Twemoji assets; load only required assets rather than a full decoded atlas. Twemoji graphics require CC-BY-4.0 attribution, separate from its MIT code license. Unsupported/new sequences fall back to text/font rendering, not replacement of unrelated text. [S25]

Custom emoji tokens `<:name:id>` and `<a:name:id>` become inline assets loaded from the Discord emoji CDN with static PNG/WebP or animated GIF/WebP as supported. Keep a text fallback for unavailable/deleted assets. Animated emoji only advance while visible; cap simultaneous decoding and reuse frame buffers. Picker cells are virtualized and searchable by name/alias, with categories, recent items, Unicode variants, and available guild emoji.

The picker inserts Unicode or the correct custom token at the cursor; it does not upload or create emoji. Availability for sending outside the source guild and animated emoji may depend on account entitlement and permissions; reflect known restrictions and surface server rejection without fabricating Nitro capabilities.

Reactions store aggregate counts and the current user's state, not every reacting user. Handle add/remove/remove-all/remove-emoji and events for uncached messages without allocating their histories. Support normal reactions only; do not expose a nonfunctional paid burst-reaction UI. Clicking one's existing reaction removes it. Reaction-detail users load only on explicit inspection. [S5]

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

No fixed 60 Hz subscription. Enable animation ticks only for currently visible animated emoji or ongoing media, and stop them on occlusion/inactivity. Baseline text navigation must remain usable while voice/video workers are busy.

## 11. Login and token storage

MVP login offers two methods (ADR 0006): **QR code** (primary, shown first) and **explicit token paste** (advanced) with a clear account/ToS warning and password-style input. There is no embedded Discord login page, token extraction, browser-store scraping, or password collection. Validate the resulting token once with `GET /users/@me` before establishing the Gateway.

Use the keyring ecosystem with one selected native backend per OS: Windows Credential Manager, macOS Keychain, and Linux Secret Service. Prefer `keyring-core` plus the exact backend crate, not the CLI feature that pulls every store. Native-store calls run off the UI thread. Store under application service `fastcord` and account ID after validation. [S26]

If a secure store is unavailable/locked, offer an explicitly nonpersistent session and explain that it will require login next launch; never silently write a plaintext token. A successful logout clears in-memory secrets, removes the saved credential, tears down workers, and purges account caches. Deleting a local credential is not a claim that every Discord session was revoked.

Use secret wrapper types, no `Debug` serialization of tokens, and zeroization of owned secret buffers where feasible. This cannot guarantee erasure of arbitrary copies inside third-party TLS/HTTP/GUI internals; do not promise that it can. Do not read the clipboard automatically or erase unrelated clipboard content.

QR login implements Discord's remote-auth protocol: a dedicated WebSocket (`wss://remote-auth-gateway.discord.gg`), an ephemeral RSA-OAEP key pair generated per attempt, the fingerprint rendered as a QR code (`https://discord.com/ra/{fingerprint}`), display of the pending user after the phone scans, and the encrypted ticket exchanged for the token through the documented REST call. Keys and tickets live in memory only and are dropped on success, cancel, or timeout; the QR expires with the server's timeout and can be regenerated explicitly. If the exchange requires a CAPTCHA, stop and offer token paste or the official client. Password login, MFA entry, CAPTCHA solving, and account verification are not implemented. If Discord requires a challenge, stop the affected action and direct the user to the official client; do not retry indefinitely. The accepted ToS risk does not guarantee the account will remain usable. **UNKNOWN U6:** current remote-auth opcode set and ticket-exchange endpoint must be captured from the community documentation and verified with the alt account in milestone 5a.

## 12. Performance budgets and measurement

All figures in this section are design targets until milestone 38 records results.

### 12.1 Idle target

The hard product target is **less than 150,000,000 bytes** of steady-state process-resident memory on the primary Windows scenario, with working set and private committed memory both reported. Other OS measurements report RSS and their platform-specific private/footprint metric. Report dedicated/shared GPU memory separately; do not hide it in an RSS comparison.

**Renderer backend (ADR 0002):** iced uses wgpu pinned to the platform-native API — DX12 on Windows, Metal on macOS, Vulkan on Linux. OpenGL is excluded; wgpu's automatic selection is not used. Measured on the primary Windows PC with an empty window, release build (2026-10-07, working set): automatic selection 172 MB, DX12 77 MB, Vulkan 96 MB, GL 221 MB, tiny-skia 21 MB. **Risk:** DX12 alone exceeds the 45 MiB "runtime + idle renderer" allowance below; milestone 38 must either reduce renderer overhead (wgpu limits, staging-belt size, atlas sizes) or rebalance the envelope, and the gap must be tracked from the first UI milestone onward.

Planning envelope (MiB, not MB):

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
- Design active-session resident-memory ceiling: 300 MB for voice plus one send and one watched 720p stream. This does not replace the idle 150 MB requirement.
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
fastcord-vX.Y.Z-native-sources.tar.gz
SHA256SUMS
```

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
| Password/CAPTCHA login in MVP | More unstable user-auth surfaces and challenge handling; QR remote auth plus token paste cover login (ADR 0006) |
| Plaintext token configuration fallback | Turns a missing keyring into credential exposure; explicit memory-only login is safer and honest |

These are project design decisions, not benchmark claims about the alternatives.

## 15. Risks and explicit unknowns

| Risk | What breaks | Detection and resolution gate |
|---|---|---|
| U1: user Identify/profile/capabilities drift | Login rejected or READY parsed incorrectly | Milestone 6: sanitized live alt-account handshake plus READY/READY_SUPPLEMENTAL fixtures; do not spoof around challenges |
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

## 16. Ordered milestones — exactly one commit per item

Each item is one coherent feature commit containing its implementation, deterministic tests, and the relevant README/protocol/testing/changelog update. Later acceptance does not retroactively excuse an earlier broken feature. Platform/media test fixtures are sanitized; live tests use the accepted alt account and a cooperating test participant. An implementer performs the checks below; none have been run for this specification.

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
13. **DMs and group DMs.** Add private conversation list/open/create using selected recipients and reuse the timeline/composer. Acceptance: DM and group DM history/send/edit/delete/reply work with official clients; gateway participant/channel updates appear; no bulk discovery/creation.
14. **Attachment viewing and bounded CDN cache.** Add signed URL fetching/refresh, image view, file metadata, explicit download/open. Acceptance: large image thumbnails stay bounded; expired URL refresh works; cross-origin requests never carry token; unsupported files remain accessible via explicit action.
15. **Unicode/custom emoji and picker.** Add attributed assets, inline runs, sequence handling, custom/animated assets, search/recent picker. Acceptance: skin tone/ZWJ/flag round-trips; custom and animated emoji match official rendering semantics; off-screen animation stops; entitlement failure is visible.
16. **Reactions.** Add normal reaction counts/self-state and add/remove. Acceptance: Unicode/custom reactions synchronize bidirectionally with official client; typed delete route is captured; remove-all and uncached events do not leak state.
17. **Voice session and encrypted RTP transport.** Implement voice-v8 control, IP discovery, AEAD modes, RTP/RTCP parsing, SSRC mapping, and leave. Acceptance: both mode vectors pass; nonce/sequence wrap cases are distinguished; malformed headers reject safely; join handshake never sends plaintext media.
18. **DAVE/MLS integration.** Add pinned davey adapter and all required transitions/binary messages. Acceptance: reference frame/MLS vectors and official-client membership churn pass; rekey failure pauses media; stream-independent session state is possible; no transport-only downgrade.
19. **Audio engine and native device backend.** Add cpal ownership, format/rate conversion, rings, Opus encode/decode. Acceptance: local microphone loopback/codec round trip on each OS; allocation instrumentation shows no callback allocations; teardown stops callback threads.
20. **Voice playback, jitter, and mixing.** Connect DAVE media to adaptive jitter, FEC/PLC, mixing, and real call output. Acceptance: bidirectional official-client audio works under reordering/loss; four remote speakers mix without clipping/queue growth; joins/leaves release decoders.
21. **Voice controls and speaking UI.** Add join/leave controls, mute/deafen, per-user gain, speaking indicators. Acceptance: mute sends no audible microphone data; deafen also mutes capture; gains affect only selected user locally; speaking attribution survives SSRC changes.
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
38. **Performance and release acceptance.** Enforce final byte caps, idle scheduling, resource teardown, and documented measurements; fix violations in the same feature commit. Acceptance: idle <150 MB and mean <0.5% one core on the recorded primary scenario; 30-minute voice and 20-cycle share tests meet section 12; all four packaged targets pass their defined live/manual matrix; every supported MVP action is usable without a stub.

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

## 17. Sources and rationale evidence

Implementation does not exist yet; `LICENSE:1–35` is the only repository codebase-adjacent evidence read. Proposed files above are the intended homes for the decisions, tests, and documentation. The following sources establish protocol/library/platform facts, not fastcord performance or implementation success.

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

