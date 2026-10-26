# Network recovery evidence

Implementation and offline evidence do not complete M38, M51, #108 or the new
network milestones' live acceptance. No blocked-network effectiveness is claimed.

## M92: application-local TLS recovery

Checked on Windows, 2026-10-24:

- `cargo test -p fastcord-net --locked`: 47 passed, no ignored tests. Includes
  structural parser vectors/mutations/oversize input, arbitrary record/write/read
  fragmentation, byte-identical handshake reassembly, partial/Pending I/O,
  cancellation and delay, staging permits, actual localhost rustls TLS 1.2/1.3,
  ALPN h2, forced HRR, TLS 1.2 resumption, protected records and application data.
  The deliberately public localhost test key is not an account credential.
- CONNECT fixtures cover authentication, authority/private-address/pipeline/header
  rejection, opaque TLS and HTTP/2 request/response, forwarding backpressure and
  exact probe wire-byte caps. They model streams, not physical TCP segments.
- Injected-clock selector tests cover budgets, cancellation, network generation,
  role/family/endpoint confirmations, service-health distinctions and coalesced
  UI notifications. Canary URLs/credentials stay out of errors.
- Gateway driver-restart regressions retain the failed-attempt, discovery-403,
  rolling Identify and backoff ledgers. Six upload tests passed with local socket
  permission; a restricted Windows process's socket error 10013 had initially
  caused two misleading `Changed` failures. The shared transport never retries
  an ambiguous write.
- `cargo fmt --all`, workspace Clippy with warnings denied and
  `cargo test --workspace --all-targets --locked` passed. Live/hardware tests
  remain ignored under their existing annotations.
- Actual `fastcord --network-preview` launched and rendered the network settings
  and nine service rows. This isolated debug launch reads no saved credentials,
  restores no account and sends no network requests. Normal Windows app builds
  and the complete test-target type check passed.
- `scripts/check_network_boundary.py` passed. Third-party licenses were
  regenerated after the lockfile change; no additional third-party crate was
  introduced (the http crate was already in the dependency graph).

Production connection inventory:

| Connection | Context consumer | Preserved feature policy |
|---|---|---|
| Authenticated REST/discovery | discord/rest | fixed API origin, limiter, write allowlist, no automatic write replay |
| Main Gateway | discord/ws + gateway/transport | normal Origin, framing, account retry ledger |
| Remote-auth WS/exchange | discord/remote_auth | explicit QR attempt, no ticket retry |
| Password/MFA | discord/password_auth | one attempt, challenge behavior |
| Build-number fetch | discord/gateway/profile | credential-free, once per process |
| Upload storage | discord/upload | separate credential-free client, validated storage URL, no redirect |
| CDN/redirects | app/attachments | separate credential-free client, each hop revalidated |
| Voice/camera/Go Live WS | media/gateway/transport | same broker, normal origin TLS and signaling |
| Media UDP | media/gateway/transport | explicit broker bind, no proxy-mode direct fallback |

Not performed: blocked-network A/B, physical packetization capture, authenticated
Windows login/Gateway/CDN/upload through a filtered route, native Linux/macOS
network-change or UI checks, kernel/HTTP/TLS-inclusive memory measurements.
Those remain acceptance gates; localhost interoperability proves only that the
shaping preserves valid unfiltered TLS. Unsupported network-change backends
return Unsupported and use demand/manual reassessment, with no polling.

The current Windows integration has network-change notifications but no stable
network-identity provider. It therefore uses the specified process-only generation
fallback: learned confirmations do not survive a restart. The bounded, versioned
persisted role-hint schema is covered offline; stable salted identity acquisition
and persisted-profile reuse are not claimed as production evidence.

The app-managed buffer bound is an explicit code budget: shared 32 tunnel slots,
64 KiB forwarding per tunnel, 8 KiB bounded header staging, four <=96 KiB TLS
stages, 32 transient CONNECT receipts, one active recovery endpoint and bounded selector/settings state.
Kernel socket buffers and existing HTTP/TLS allocations are outside that budget
and must be reported separately in a resource measurement.

## M93: user-configured TCP proxy

Windows offline evidence, 2026-10-24:

- HTTP CONNECT and SOCKS5 fixtures check authenticated/unauthenticated transport,
  remote destination DNS (deliberately unresolvable origin names), fragmented
  replies, IPv4/IPv6 origin address encodings, negotiation bounds, EOF, cancellation,
  refusals and unavailable-proxy behavior. An origin listener sees no connection
  when the selected proxy is unavailable.
- A shared-factory fixture exercises all nine HTTP roles and four WS roles:
  exactly 13 CONNECT requests reach the configured proxy, origin-token/private-
  bridge credentials never enter upstream proxy headers, and probe dials stay zero.
- Actual HTTPS-proxy TLS rejects an untrusted proxy certificate. Trusted local
  fixture TLS carries separately verified origin TLS and h2 ALPN. Real reqwest
  HTTP/2, uploaded PUT bytes and a non-followed 302 cross the private bridge and
  selected upstream proxy. Negative HTTP/WS origin-certificate fixtures reject
  a self-signed origin with CertificateRejected, without a shaping campaign.
- Positive HTTP CONNECT, HTTPS CONNECT and SOCKS5 fixtures complete a real
  RFC 6455 Upgrade and receive Gateway HELLO after verified origin TLS/ALPN;
  proxy credentials do not reach the origin, and no Identify/token is sent.
- Five native-store mock tests cover independent endpoint-bound slots, separate
  account namespace, empty/oversized passwords, overwrite/delete, locked-store
  failure and redaction. The credential key includes kind/host/port/username;
  network-settings.json contains no password.
- App tests verify the pre-join explanation precedes voice-state/ring intent and
  that password UI messages redact a canary. The real Windows proxy settings
  screen was built, launched and visually inspected in the offline preview.
- Workspace Clippy passes with warnings denied. App/native-store suites passed
  (822/62 tests respectively, existing hardware/live tests ignored). Final net
  counts are recorded by the test command; the proxy subset passes with one
  explicit Windows-only ignored IPv6 proxy-listener gate.

This Windows host refuses outgoing ::1 sockets with error 10013 even outside the
restricted process (also checked independently with native TCP). The actual IPv6
proxy-listener fixture is explicitly ignored on Windows; IPv6 destination ATYP4
through an IPv4 proxy remains an active passing fixture. Re-run the ignored test
on an unrestricted IPv6 host. Authenticated Windows login/Gateway/CDN/upload
through real configured proxies, and blocked-direct real-network A/B, remain
manual acceptance gates. SOCKS5 UDP has not been implemented in M93.

## M94: experimental direct UDP discovery preludes

The app no longer offers or applies this opt-in (#223): on 2026-10-26 a blocked
Windows route closed every join as `Network` after ~31 s with it on and joined
normally with it off. The evidence below covers the retained library code only.

Windows offline evidence, 2026-10-24:

- All 178 media tests passed, including unchanged AEAD, DAVE/MLS, RTP and H.264/
  AV1 vectors. Independent datagram/signaling fixtures verify direct/3/6 ordering,
  exactly fifteen one-second-spaced discovery requests over three retained local
  ports, nine 1200-byte preludes, one signaling connection/Identify and no Select
  Protocol for unanswered discovery. Old-flow replies, wrong source and wrong
  SSRC cannot select; a valid reply selects once and stops all preludes/retries.
- A port allocator deliberately reuses released ports; retained old flows force
  distinct ports. They are released when the join ends. A new WS handshake does
  not advance the variant or refill the opt-in campaign's socket/request/byte
  budget. A drop after Select Protocol cannot restart experimental discovery.
- Stalled bind and prelude futures end at the same sixteen-second join deadline.
  Heartbeat requests and leave remain responsive during a stalled prelude. The
  existing HELLO/READY/Session Description and normal established Resume tests
  pass. With the experiment off, the previous direct-only signaling retry policy
  is preserved; no laboratory datagram is sent.
- Net tests verify separate opt-in, false defaults in Off/Auto, Proxy-disabled
  behavior, shared budget preservation and coalesced variant notifications.
  The app shows variants and unanswered UDP separately from TLS reachability.
- Final M94 runs: 822 app tests, 178 media tests and 65 net tests passed; one
  existing Windows IPv6 proxy fixture and existing app hardware/live gates remain
  ignored. Workspace Clippy with warnings denied and the network audit passed.
  The real Windows preview showed the separate checkbox off by default; explicit
  opt-in changed only preview memory, with no account or network traffic.
- The original fixture, regeneration and SHA-256 are in
  `fixtures/voice/udp-prelude-v1.md`; no captured/third-party payload is used.

Filtered-network A/B and real NAT behavior are unverified. Zeros are only a
laboratory candidate, never a supported arbitrary-payload bypass claim. The
experiment remains off by default and unavailable on proxy routes. The feature
adds no OS interception, TTL adjustment, extra voice Identify for rotation,
post-selection strategy change or real-time audio callback work.

## M95: separate SOCKS5 UDP media routing

Windows offline evidence, 2026-10-24:

- Ten real loopback UDP/control fixtures cover authenticated association,
  fragmented replies, wildcard/domain BND addresses, IPv4 and IPv6 inner media
  destinations, source/address-family checks, malformed headers, unsupported
  fragments, spoofed outer and inner sources, MTU accounting and oversized
  datagrams. The source hint in CMD=3 is a family-matched wildcard with port zero
  because a private local socket does not establish the proxy-visible NAT source;
  fixtures independently observe the actual nonzero UDP source port.
- EOF or unexpected control data stops sends and receives. Cancellation and
  negotiation deadlines release control sockets. Eight association permits are
  shared across mode/configuration views; dropping a socket releases its permit.
  Unsupported CONNECT proxies and unavailable SOCKS5 proxies cannot produce a
  direct media socket.
- An independent media fixture runs the production datagram adapter under fake
  signaling: discovery crosses a real local relay, Select Protocol uses the
  response's external address/port, and control loss closes media without another
  signaling connection or Identify/Resume. Public fixture destinations are only
  encoded inside loopback packets; these tests send no traffic to those hosts.
- Final net/media suites passed: 76/179 tests, respectively. The existing Windows
  IPv6 proxy-host test remains ignored; IPv6 destination encapsulation through an
  IPv4 relay is exercised. The app suite includes an explicit SOCKS5 call-intent
  regression, while CONNECT media joins still explain the limitation first.
- The private bridge's random credential is now shared for the application
  lifetime across context views. A regression verifies this, and the constructor
  audit rejects direct TLS dependencies outside net as well as raw sockets.
- Full workspace tests (2,201 passed, 52 existing live/hardware/platform gates
  ignored), Clippy with warnings denied, formatting, the network
  boundary audit and third-party license checks passed. No new third-party crate
  was added. The final Windows binary was built and its isolated network preview
  was visually checked; no credentials were loaded and no account was connected.

`MANAGED_BUFFER_RESERVATION_BYTES` reserves 3.5 MiB against a compile-time 4 MiB
budget: 32 tunnels with 64 KiB forwarding and 8 KiB header staging each, four
96 KiB TLS stages, eight reused 64 KiB relay receive buffers, two 64 KiB settings
buffers and 256 KiB for bounded control metadata/scratch. This is a code budget,
not a total process-memory measurement. Kernel sockets and existing HTTP/TLS and
feature-media allocations require separate measurement.

Still required: real configured-proxy login/Gateway/CDN/upload, native IPv6 proxy
hosts, proxy NAT interoperability, thirty-minute Windows bidirectional voice and
screen-share send/watch with direct routing unavailable, filtered-network A/B,
and Linux/macOS native checks. UDP to the proxy must itself be reachable. These
gates remain open; neither SOCKS5 UDP nor the experimental direct preludes are
claimed to repair every blocked media path.

## Live login follow-up, 2026-10-24

The user reported a failed Auto login behind a process-specific VPN exclusion
for fastcord. The new debug-only `fastcord.exe --network-check` runs before
credential access/settings/account restore/instance handoff and sends one
anonymous `GET /api/v9/gateway` per Off/Auto context. An eligible Auto failure
uses the existing bounded TLS selector. All output is categorical, with no
account token, proxy password, response body or endpoint address.

With the exact workspace `target/debug/fastcord.exe`, both initial requests
failed with `TcpTimeout`; the Auto TLS check ended with `TlsTimeout`, without a
confirmed working strategy. No login or Gateway Identify was sent. A preliminary
check from a differently named executable returned HTTP 200, but is not direct-
route evidence when VPN policy applies to fastcord specifically. The successful
offline tests above therefore do not establish that the implemented recovery
works on this filtered route. The failure cause beyond timeout is unconfirmed.

At that point the implemented TLS catalog had record/write splitting only. Flowseal's
[general](https://github.com/Flowseal/zapret-discord-youtube/blob/main/general.bat)
and [FAKE TLS AUTO](https://github.com/Flowseal/zapret-discord-youtube/blob/main/general%20%28FAKE%20TLS%20AUTO%29.bat)
include packet fake injection, TCP sequence overlap and/or disorder techniques
absent here. The zero-filled UDP prelude is explicitly original laboratory data,
not a port of those profiles. The Windows upstream packet backend uses WinDivert;
see its [Windows documentation](https://github.com/bol-van/zapret/blob/master/docs/windows.md).

Login notices now retain the available typed transport category or HTTP server
status instead of using one message for both. Unknown/body failures retain safe
fallback categories; account rejection is still distinct. No automatic login or
authenticated-request replay was introduced.

Follow-up validation: all 830 app tests passed (six existing ignored gates),
including transport-vs-server login notices; app Clippy with warnings denied,
formatting and the network boundary audit passed. The Windows binary was rebuilt
and the isolated native preview launched. This validates the diagnostic change;
direct login remains unresolved on the reported route.

The user then explicitly supplied a test credential. A temporary debug harness
inside the same `fastcord.exe` invoked the production `login::validate` once in
Auto, with a ninety-second outer deadline. Input was bounded, provided through
a private stdin pipe from a masked prompt and kept only in memory, with zeroizing
Rust input/token owners;
no credential, account identifier or response body was printed or written to a
file. The attempt returned the typed API `TcpTimeout`, without an HTTP response.
It therefore did not establish whether the credential was accepted or rejected.
No account was persisted and no Gateway was opened. The temporary harness was
removed and the normal binary rebuilt; the direct-login failure remains open.

## M96: optional Windows packet recovery, 2026-10-25

The user explicitly approved an optional WinDivert mode with administrator UAC.
Default Auto remains unprivileged and does not start a driver. The same executable
now has a separate restricted helper entry, authenticated local named-pipe IPC,
public-hello-only messages, deadlines, cancellation and exact per-hello filters.
HTTP CONNECT and direct WS use one strategy selection function; authenticated
requests, protected TLS records and origin certificate verification remain in
the ordinary process. No credential or response body is logged or saved by
the diagnostic. No account is persisted by the live check.

Strategy references, inspected on this date:

- Flowseal [general.bat](https://github.com/Flowseal/zapret-discord-youtube/blob/main/general.bat)
  contains multisplit/sequence-overlap profiles (including 681 for media/Google
  and 568 in general-list profiles). fastcord's experimental overlap candidate
  uses 681 at split 1 and midsld; it is not a verbatim copy of that batch file.
- Flowseal [FAKE TLS AUTO](https://github.com/Flowseal/zapret-discord-youtube/blob/main/general%20%28FAKE%20TLS%20AUTO%29.bat)
  provides the fake/multidisorder/badseq approach. fastcord generates its own
  same-size public decoy from its ClientHello, uses six sequence-10000 fakes,
  then reversed real segments at 1/midsld. No winws binary, batch file or
  third-party fake TLS/UDP payload is shipped.
- Native ABI, filtering, checksums and queue constraints follow the
  [WinDivert 2.2.2 documentation](https://github.com/basil00/WinDivert/blob/v2.2.2/doc/windivert.html).
  Official unmodified x64 binaries, LGPLv3 terms, corresponding source and
  SHA-256 provenance are in `crates/fastcord-platform/native/windivert/`.

Live evidence on the reported route:

1. All direct-route tests used the exact workspace
   `target/debug/fastcord.exe`, which the user excluded from VPN. A differently
   named test executable was not used as direct-route evidence.
2. Base Direct/write/record/native-socket experiments did not restore access.
   The unreliable TransmitFile, urgent-data and TTL prototypes were removed.
3. An initial UAC attempt from an external PowerShell launcher did not appear;
   no driver result was claimed. The user then confirmed the real native
   fastcord button's UAC manually. The first API check exposed an HTTP-bridge
   selection bug (it still chose Direct); that result was a failure, not packet
   effectiveness evidence. HTTP and WS now share selection, covered offline.
4. After the correction, the helper confirmed that **PacketFake** was applied
   on hardware egress. Anonymous `GET /api/v9/gateway` returned **HTTP 200** and
   the native UI displayed `API: Service answered`.
5. A bounded debug-only check received the explicitly supplied alt-account
   token through a private stdin pipe from a masked prompt. Production
   `login::validate` succeeded; the production Gateway then delivered **READY**
   and its closer confirmed a clean shutdown. Only categorical results were
   printed. This proves the previously failing login and initial Gateway on
   this route with PacketFake, without a proxy or account persistence.

Offline coverage includes real localhost TLS/ALPN/h2 and application bytes
through both packet-capability selections (a fixture forwards the original
hello, without loading a driver), IPv4/IPv6 packet reconstruction with TCP
options and wrapping sequence numbers, malformed packet rejection, and bounded
IPC framing/private-address/port checks. These fixtures prove byte preservation
and routing integration; they do not model an ISP's DPI.

Final Windows validation: `cargo test --workspace --all-targets --locked`
passed **2,207 tests**, with 52 existing live/hardware/platform gates ignored;
`cargo clippy --workspace --all-targets --locked -- -D warnings`, formatting,
the network constructor/dependency audit and regenerated third-party license
checks passed. A normal debug executable was rebuilt after the live check.

Still open: blocked-route effectiveness of PacketOverlap, a physical independent
capture, kernel-inclusive resource measurements, CDN/upload breadth and
thirty-minute bidirectional voice/camera/screen-share tests. The packet helper
handles TCP TLS only; existing UDP preludes remain unverified and opt-in.


## Transport review follow-up (2026-10-25)

The three review findings are covered by offline regressions in `fastcord-net`:

- Scoped recovery tests hold an active CDN campaign gate and verify that another
  CDN endpoint, the API and a different role do not wait. The matching endpoint
  does wait; unrelated requests cannot consume its pending demand. Network
  changes remove stale demands so a new endpoint can recover without a budget
  reset.
- Per-attempt tests interleave two connections to the same endpoint and verify
  distinct addresses/errors, role/authority binding, unique IDs, revocation on
  completion/cancellation and the 32-receipt cap. Paused-clock connector tests
  check TLS timeout reporting, permit release, cancellation without a false
  diagnostic demand, missing-scope refusal and IPv6 authority normalization.
- A real local HTTP proxy accepts two concurrent requests from one HTTP client
  to the same origin: one receives proxy authentication refusal and the other
  encounters an untrusted origin certificate. Both typed errors remain distinct;
  private CONNECT credentials, origin queries and request canaries never reach
  the upstream proxy headers. Existing HTTP/2 and upload fixtures continue to
  exercise the actual reqwest connector and both proxy hops.
- Dial tests inject `AAAA, AAAA, A` and `A, A, AAAA` answer orders. A first-family
  timeout still leaves the other family its own complete attempt deadline;
  duplicate addresses are not redialed and two timeouts end at two attempts.

These are deterministic/local correctness checks. They do not add live blocked-
network, native Linux/macOS, resource-measurement or media interoperability
acceptance evidence.

## Unprivileged bypass on the reported route (2026-10-26, #218)

The VPN (a TUN adapter) was switched off for this measurement, so every dial
below used the ordinary Ethernet route and the workspace `fastcord-net` code.
Diagnosis was done outside the client with a throwaway Python socket harness that
sent hand-built ClientHellos; the production acceptance used the shipped code
unchanged apart from the strategy described here.

What the route does, measured repeatedly:

- TCP to `162.159.136.232:443` (and `discord.com`, `gateway.discord.gg`,
  `cdn.discordapp.com`) completes; the TLS handshake then receives nothing. The
  same address with an SNI of `example.com` or `cloudflare.com` answers in 40-90 ms,
  including in mixed case, so the filter matches the `discord.com` name in the
  ClientHello rather than an address or port.
- Neither a whole ClientHello (185-238 B and 1518-1536 B) nor TCP write splitting
  at 1, the SNI or midsld restores an answer. Two-record splitting also fails: the
  same hello as two records, each written as its own segment, stayed a black hole.
- A refragmented ClientHello passes only when write boundaries avoid record
  boundaries. Four records written one per segment: blocked, 0/9. The same four
  records with each write ending one byte before the next record boundary:
  answered, 9/9, in 50-90 ms. Three, four and five records and offsets of -1, +1,
  -2, -5 and -10 bytes all pass; padding-free legal records only.
- The winning layout holds for both the real `rustls` hello (`238 B`, no
  post-quantum key share) and a `1536 B` one, so it does not depend on hello size,
  extension order or a particular client fingerprint.

Production acceptance with the shipped code (`fastcord-net` only, no helper, no
driver, no administrator rights):

1. A fresh `NetworkContext` in Auto with the anonymous API check: the first dial
   ended `TlsTimeout` (4 s) and recorded the demand. The automatic campaign then
   probed `Direct` (failed) and `RecordFragment` twice (both answered, 2.7 KiB
   received), confirmed it, and the same request succeeded after 4.9 s.
2. A subsequent `GET https://discord.com/api/v9/gateway` through the production
   `http_builder(Role::Rest)` returned **HTTP 200** after the campaign, and the
   status became `ServiceReachable`.
3. The next dial reused the confirmed strategy and completed in **111 ms**,
   without another probe.

Offline coverage added with the strategy: the refragmented bytes reassemble to
the original handshake, every interior boundary begins a record header, the four
writes start at offsets `0, b1-1, b2-1, b3-1` with the documented 10 ms spacing,
a hello that already spans records is preserved, and the existing real TLS
1.2/1.3, ALPN, HRR and application-data suites run `RecordFragment` too.

Not claimed: media. Voice and camera use UDP, which has its own path and is not
shaped by this strategy; the packet helper still handles TCP TLS only. The
fragment layout was verified on this one route; other filters may need a
different one, which is why recovery keeps the rest of the catalog and the
privileged helper.

### Follow-up: one recovered service did not carry the others (same day)

A live app session on the same route recovered login and then stayed useless:
no message history, no avatars or attachments, and the voice Gateway retried
`connection: connecting` nine times before closing with `Network`. A measurement
that walks the app's own sequence in one process showed why:

| step | first dial | second dial | probes run | rolling dials |
|---|---|---|---|---|
| login (`Role::Login`, Http) | `TlsTimeout` 8.0 s | **HTTP 200** 4.9 s | Direct failed, RecordFragment twice | 6 |
| REST (`Role::Rest`, Http) | `TlsTimeout` 8.0 s | `TlsTimeout` 8.0 s | **none** | 6 |
| CDN (`Role::Cdn`, Http) | `TlsTimeout` 8.0 s | `TlsTimeout` 8.0 s | **none** | 6 |
| Gateway (`Role::Gateway`, WS) | `TlsTimeout` 4.1 s | `TlsTimeout` 4.0 s | **none** | 6 |

The confirmation was per exact endpoint, and the next endpoint's campaign was
refused by the 60 s start spacing, so REST, CDN and voice could only recover
minutes later, one service at a time, inside a 24-dial rolling cap. Reusing the
newest confirmation for the other endpoints of the same family (ten minutes)
changes the same measurement to:

| step | result | probes run | rolling dials |
|---|---|---|---|
| login | HTTP 200 after one campaign | Direct failed, RecordFragment twice | 6 |
| REST | **HTTP 200** 128 ms | none | 6 |
| CDN | **HTTP 200** 156 ms | none | 6 |
| Gateway | **TLS handshake** 127 ms | none | 6 |

The reuse is a preference, not a claim: an endpoint that fails with it still
records its own demand and gets its own campaign. The same day also exposed the
privileged path's failure mode — the helper exited while the app ran, `select_strategy`
kept choosing `PacketFake`, and every later dial failed with `ConnectionAborted`
while the panel still said the helper was on; a factory that reports itself
unavailable is now never selected, and recovery plans the ordinary catalog
instead.


