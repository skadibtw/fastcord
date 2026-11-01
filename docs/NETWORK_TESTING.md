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
- Trust (#131): production configs verify with the OS trust store
  (`rustls-platform-verifier`, ring provider; embedded webpki roots only if it
  cannot start). Offline tests: an untrusted self-signed server is rejected and
  the ClientHello extension set, cipher suites and length equal the
  embedded-roots config's. Manual: with an enterprise root installed in the OS,
  an HTTPS proxy signed by it connects.
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


## SNI-focused recovery follow-up (2026-10-26, #218)

The existing debug `fastcord.exe --network-check` again failed: Off timed out,
Auto's Direct, RecordFragment, TlsRecordSplit, WriteSplit and Combined TLS probes
received no bytes. The TUN adapter was up during this run; it was not disabled
or reconfigured. These measurements establish improvement on the unchanged
reported route, not a new VPN-off acceptance claim.

A credential-free socket harness compared the real rustls public ClientHello
with Python's larger one. Uniform four-record fragmentation no longer reliably
worked for the small rustls hello. A 32-record prototype with 30 ms spacing
answered on one address but failed on four other DNS answers. Fragmenting the
structurally located hostname itself into one-byte records answered on all five
addresses; therefore no IP pin, DNS override or address-selection change was
introduced. The temporary harness and public hello are not shipped.

Catalog v4 adds SniFragment after Direct and RecordFragment. It fragments only
the final at most 30 hostname bytes, with at most 32 nonempty records and 31
asynchronous 30 ms delays, each write ending one byte before the next record
header. All handshake bytes, SNI, ALPN and transcript remain identical. A name
already spanning records is unsupported by this layout and passes unchanged
before any transformation. Staging limits and the four transformation permits
are unchanged; the fixed write plan now has at most 32 entries. Catalog changes
expire old stored hints.

One fresh production NetworkContext, Auto, no proxy or packet helper:

| operation | measured result |
|---|---|
| first anonymous login-role API request | TlsTimeout, 8.0 s |
| TLS campaign | Direct and RecordFragment timed out; SniFragment succeeded twice |
| next login-role `GET /api/v9/gateway` | HTTP 200, 591 ms |
| REST-role `GET /api/v9/gateway` | HTTP 200, 557 ms |
| credential-free default CDN avatar | HTTP 200, 853 ms |
| five separate Gateway TLS connections | all succeeded, 746–832 ms |

The campaign reserved 8 of the unchanged 24 rolling dials; the later services
used its confirmation without another campaign. The harness used production
HTTP builders and connect_tls, sent no credential, Identify or user write, and
did not access account storage. Gateway TLS is not authenticated READY or a
voice/media acceptance test. UDP is unaffected.

Offline validation covers exact reassembled bytes, every input write boundary,
short writes, delay timing, cancellation/shutdown, one- and 253-byte names,
refusal before mutation for staging overflow and a name crossing records,
TLS 1.2/1.3 with ALPN/HRR/application bytes, the real opaque HTTP bridge, and
confirmation/reuse within the unchanged campaign budget.

The Network page's anonymous Check connection previously returned after the
initial failure without starting its pending campaign; a second click or the
advanced Retry pending TLS check was needed. It now waits for bounded recovery
in Auto and, only after TLS confirmation, repeats this credential-free GET once.
Off, proxy mode, helper selection, noneligible errors and account/write replay
policies are unchanged. The debug --network-check now calls that same flow.

Final executable acceptance used the separately built
`C:/Users/user/.codex/worktrees/discord-route-218/fastcord/target/debug/fastcord.exe`
with `--network-check`, before any settings, native credential access or
single-instance handoff. Off returned TcpTimeout; Auto's first call to the same
Check connection flow printed two successful SniFragment probes, then
`Anonymous Discord API check passed (HTTP 200)`, ServiceReachable and 8 rolling
dials. A preliminary executable built against the main checkout's shared target
had selected the old catalog; it was discarded, and the final build uses its
own target directory. No global network settings or running main client changed.

Workspace tests and strict workspace Clippy passed; the final connection-check
change was followed by app/net regression tests and strict Clippy in the isolated
build directory. Formatting and the production network-boundary audit passed.
The Windows app builds, with the existing bundled-libopus MSVC runtime linker
warnings; no audio/linker change is included in this fix. Authenticated READY,
voice UDP and a VPN-off run remain unverified for this follow-up.

## Gateway greeting follow-up (2026-10-26, #218)

The alt-account app accepted login but stayed empty. A single-connection
diagnostic reached a verified HTTP 101 Upgrade, then timed out waiting for
HELLO; it never sent a Gateway Identify. This distinguishes API/TLS reachability
from a usable Gateway. Raw TLS reads and the upstream WebSocket client reproduced
the missing frame. Existing unprivileged byedpi presets, SNI fragmentation plus
those presets, Ethernet source binding, smaller receive buffers, delayed TCP
writes after TLS and TLS 1.2 did not restore a greeting in these trials.

With SNI disabled in rustls, while retaining the original DNS ServerName and
the ordinary certificate verifier, all five current Gateway A records returned
a valid opcode-10 HELLO with zlib-stream through fastcord-ws. No IP is pinned.
The final production NetworkContext in Auto then received two consecutive valid
HELLOs and recovered the anonymous REST GET to HTTP 200 in the same process.
These checks ran from the exact excluded fastcord.exe path, without account
storage, Identify, proxy, helper or UAC. The user left Throne enabled and stated
that fastcord is excluded from it; no VPN-off claim is made.

The WIP TLS profile v3 applies DiscordWebSocketWithoutSni to Auto's main,
voice and stream WebSockets, and HttpWithoutSni to REST/login/profile HTTP.
HTTP retains h2/http/1.1 ALPN. These profiles use Direct, without record shaping
or a packet helper. Off/Proxy, CDN/upload and remote-auth retain SNI. The profile and its bounded TLS-only probes use the same
configuration; cross-service shape preferences cannot cross the SNI policy.
Offline real-rustls tests verify an absent wire SNI, successful application
bytes, rejection of a trusted certificate for the wrong DNS name and rejection
of an untrusted chain. There is no certificate exception or DNS substitution.

Startup also has a separate 30-second READY/RESUMED deadline, unaffected by
heartbeat ACKs and using the existing retry/Identify limits. The UI distinguishes
waiting for HELLO from sending Identify, and keeps failure/reconnect banners
visible on Friends and message-request panels. Deterministic paused-clock
Identify/Resume tests cover the startup bound. App/discord/net regression tests,
strict Clippy and the production constructor audit pass. Windows builds retain
the existing bundled-libopus runtime linker warnings.

Authenticated READY and voice/media acceptance are recorded separately below
when exercised; the anonymous greeting alone does not establish either.

### Current acceptance result: NOT complete

The authorized alt was explicitly identified by the user before the final
checks. A bounded standalone diagnostic used its existing native credential
entry only in memory, verified the returned current-user ID, and printed no
credential, account ID, response body or protected payload. Three isolated
one-Identify tests received READY: bundled properties, minimal properties and
native OS/locale/installation properties with bundled versions. Live and bundled
public versions were identical (host 1.0.9265, native 96160, web 649389), so a
version mismatch was not established. Anonymous no-SNI WebSockets also returned
Pong and a protocol response after a >1 KiB outbound frame. A five-request
HTTP/2 trial returned four complete HTTP 200 responses, then one timeout; this
is not a sustained-transport acceptance claim.

The GUI still repeatedly receives HELLO and sends Identify without reaching
READY. One earlier GUI session reached READY/RESUMED, but later runs did not
reproduce it reliably. Capping outbound TLS records at 512 bytes and moving
socket reads ahead of UI watch notifications did not fix it; both experiments
were reverted. The original read/control ordering remains. This difference
between the standalone SDK and GUI startup is unresolved.

Voice logging showed only audio-device startup followed by repeated voice
WebSocket connecting and Network closure, not UDP discovery or a connected
media transport. The former green call bar was therefore false evidence; it
now requires the media driver's Connected state. Live UDP discovery, ping,
AEAD/DAVE media and bidirectional audio/share are NOT accepted. No new UDP
bypass is shipped or enabled. The previously disabled zero-prelude experiment
remains disabled. Synthetic QUIC/STUN candidates exist only in ignored local
diagnostic artifacts, not the application or acceptance evidence.

The recorded investigation reached the project's 20-Identify/24-hour limit
(1 + 4 + 3 + 6 + 3 GUI attempts and three one-attempt SDK checks). All live GUI
processes were closed; no further authenticated test, quota reset or driver
recreation is used to evade it. The working-tree changes remain uncommitted
because GUI/UDP acceptance is incomplete. No UAC, driver, external proxy or VPN
configuration change was performed.

The debug-only --login-check now exercises Auto directly from its existing
bounded private stdin input; it never starts the packet helper or asks for
UAC. It still prints categorical validation/READY results only. Do not put a
token in command arguments, files, chat, fixtures or output.

Final offline verification after reverting the unsuccessful experiments and
cleaning only the app/discord/net Cargo artifacts: strict Clippy passed;
app 860, discord 688, media 185 and net 102 tests passed (1,835 total, 14 existing
ignored gates). Formatting, diff whitespace and the production constructor
audit passed. The normal GUI executable was rebuilt with incremental compilation
disabled. Its credential-free --network-check returned Off: TcpTimeout and
Auto: HTTP 200 / ServiceReachable with zero TLS-probe dials, before account
storage or instance setup. No additional authenticated attempt was made.

Mute/deafen controls remain usable while a call is Connecting, so correcting
the green status does not prevent muting before the media keys become ready.
The standalone native-credential diagnostic executable was removed; its source
and categorical reports remain only in ignored local test artifacts. The
remaining blocker is GUI startup compared with the working isolated SDK path;
there is no evidence yet to label a new UDP prelude as an effective bypass.

### Credential-free recheck (2026-10-26, evening)

The unchanged final debug executable was checked again at the user's request.
`--network-check` returned Off: TcpTimeout and Auto: HTTP 200 /
ServiceReachable, with zero TLS-probe dials. Its separate `--network-preview`
window was then switched to Automatic and Check connection was clicked; the
window displayed "Anonymous Discord API check passed (HTTP 200)" and no packet
helper. This preview starts before account restoration and does not access
credentials. No authenticated Gateway, voice connection or new Identify was
started because the recorded 20-Identify rolling window has not expired. These
results confirm anonymous API reachability only; GUI sign-in and UDP remain
unaccepted.

### Explicitly authorized alt-account recheck (2026-10-26, 19:59–20:03 MSK)

The user explicitly overrode the local rolling-Identify testing restriction
and requested a live test on the previously approved alt. The account shown in
the GUI was mochi. No source limit or ledger was changed. The unchanged final
debug executable sent one main-Gateway Identify and received READY; server
navigation, channels and a loaded message history were visible. The main
Gateway remained connected throughout this approximately four-minute test,
without a reconnect or a second main-Gateway Identify. This supersedes the
earlier result that GUI startup could not be reproduced, but does not establish
reliable repeated sign-in or long-session acceptance.

The first voice target reached voice Identify and discovery but closed with
UdpUnreachable. A second target, empty before the test join, received a valid
UDP discovery response and Session Description: the log advanced through
Discovering, Negotiating and waiting for end-to-end keys. The microphone was
muted during the second test. It never reached Connected; the voice driver
reconnected approximately every 35 seconds and repeated the same stages. The
categorical log does not establish the cause of those reconnects, nor whether
waiting for a group while alone explains the pending DAVE state. No bidirectional
media or effective UDP-bypass payload is accepted from this result.

The test left voice and closed its process at the end. Reports contain frame
sizes and categorical states only; no tokens, keys, endpoints or message bodies
were logged. There was no helper, UAC, external proxy or VPN configuration change.
The live findings narrow the remaining problem to unreliable voice/media;
the built-in UDP bypass remains unfinished.

### Isolated same-executable recheck (2026-10-26)

With the user's continued authorization, two bounded tests ran inside the same
debug executable excluded from the VPN. Each validated the approved saved alt
in memory and made one Gateway Identify, with no credentials in output. The
ordinary app identity reached HELLO but timed out waiting for READY; the bundled
identity reached HELLO but the connection ended with a network failure. Neither
test joined a channel or reached the voice transport. The comparison therefore
does not establish that changing identity fixes startup, and it provides no new
evidence about UDP or DAVE. The temporary test entry point was removed after
the runs. No proxy, UAC, helper, or VPN configuration change was used.

### TLS version and voice recheck (2026-10-27)

Further bounded checks showed that, after Identify, a heartbeat ACK arrived and
part of the large Gateway response was read, but the remaining bytes never
arrived before the connection closed. Disabling zlib-stream, disabling the
WebSocket compression offer, sending an early heartbeat, using a shorter
Identify and choosing another DNS answer did not complete READY. A physical
source address and per-socket interface selection also did not complete it.
These observations do not identify the network device responsible.

With the same Auto no-SNI profile restricted to TLS 1.2, one authorized mochi
GUI session received READY. In an initially empty guild voice channel it
completed voice WebSocket negotiation, UDP discovery and Session Description,
sent a DAVE key package, received the external sender and continued exchanging
heartbeats. The microphone was muted. The session remained in the key-waiting
state while alone; no bidirectional audio or blocked-UDP bypass was verified.
This is one live success, not a reliability measurement. The experimental
QUIC prelude was enabled only in the diagnostic build, but this call's initial
direct discovery succeeded, so that prelude was not exercised.

**Correction:** the user subsequently identified that this session was routed
through their VPN profile. Therefore the READY and voice result above, including
the later DAVE group and audio packets, is not evidence of direct blocked-network
recovery. A new test must bind all app TCP and UDP traffic to the physical
interface to distinguish it from the VPN route.

The production Auto session profile now tries TLS 1.2 with the same hostname
and certificate verification, and may retry once with default TLS versions
only after an eligible pre-application handshake failure. Local TLS fixtures
cover TLS 1.2, a TLS 1.3-only peer, hostname verification and retry categories.
The temporary socket/interface, compression and packet-candidate experiments
are not production settings. No VPN, system route, driver or proxy was changed.

### Physical-interface retest (2026-10-27)

The diagnostic executable bound its TCP sockets to the physical IPv4 address
and interface while the VPN remained enabled. A later audit found that its
UDP test hook was installed in the loopback-only constructor instead of the
production UDP constructor. Therefore every voice observation in this section
may have used the VPN and is not evidence of direct UDP. The test did not
change the system route. On one run the Gateway
reached READY after rotating through three current DNS answers; ordinary UDP
discovery succeeded immediately and voice reached Session Description, but
the UDP path subsequently failed its liveness check while the client waited
for DAVE keys. No direct media exchange was demonstrated in that run.

On the next run the Gateway reached READY on the first answer, but voice
discovery failed after the ordinary round and both experimental QUIC-prelude
rounds. The second participant joined while discovery was in progress. These
observations do not establish an effective UDP bypass or bidirectional voice,
and do not establish why the physical path stopped responding. The fake QUIC
candidate's success must not be inferred from the earlier direct round.

A third physical-interface TCP run reached READY after three DNS candidates. Its
ordinary UDP discovery succeeded, and after the user joined the voice channel
the DAVE group reached Connected. The user reported no audible connection;
the client then detected UDP liveness failure. A later rejoin failed all UDP
discovery rounds even with the QUIC prelude repeated before each request. That
diagnostic-only variation has not demonstrated an effective bypass. The diagnostic
campaign also exposed a client bug: after a selected media path died, its
experimental discovery budget prevented the ordinary direct reconnection. The
driver now switches to the existing direct-only reconnect policy in that case;
this fix does not prove that the blocked path can be recovered.

A corrected later run applied the physical interface and source address in the
production UDP socket constructor. Windows listed each of the three voice UDP
sockets on that physical IPv4 address, and interface selection succeeded for
each socket. Gateway reached READY, but none of the direct or two QUIC-prelude
discovery rounds received a valid response. Independent credential-free UDP
checks on this interface also timed out for a public STUN server and a public
DNS resolver; a DNS request to the LAN gateway also timed out, but the gateway
was not established to provide DNS. These observations do not identify the
device or policy dropping the datagrams and do not prove that all UDP is
blocked. No live direct voice success is claimed.

Repeating the QUIC prelude before every discovery retry on a later run with
the corrected physical UDP binding also produced UdpUnreachable. This variant
was diagnostic-only and has been removed. The user could not disable their
VPN without interrupting this session, so these tests do not distinguish the
VPN's system policy from an upstream network restriction.
As a control, the same UDP DNS query received a response through the system's
default route while the VPN was enabled; the STUN query still timed out there.
This confirms that the DNS test server was responsive on the default route,
but does not locate the cause of the Ethernet-bound timeout.

### Route-correlated GUI voice check (2026-10-27, afternoon)

The user explicitly delegated GUI operation to GPT-6.1 Terra while the primary
agent changed and tested the code. Throne's actual connection IDs were matched
between its fastcord process records and outbound categories, with only those
categories exposed in the report. Earlier entries included direct traffic,
whereas the first new run used the SOCKS outbound. The user then selected a
direct-routing profile; the next test's TCP and UDP entries were confirmed as
outbound/direct. No VPN configuration was changed by the agents.

The GUI initially failed before media startup because its selected WH-1000XM6
output was disconnected. Choosing the system-default output resolved that
separate failure. With the approved mochi alt muted, direct ZDRT only discovery
failed all three flows even with a 50 ms asynchronous gap after QUIC preludes.
The initially empty 0_0 channel succeeded on ordinary discovery. After several
UDP liveness recoveries it established DAVE with the user present, and the
connection then remained up for several minutes. A voice-WebSocket heartbeat
failure resumed without replacing the live UDP transport. Throne's associated
UDP outbound was direct; the counters reached 4,486 received audio RTP packets
and 1,137 sent after a short, explicitly authorized microphone test. The GUI
reported about 49 ms ping. The user was not listening during that microphone
test, so human audibility was not accepted from it. The bounded diagnostic
process was stopped by its ten-minute test deadline.

These results establish direct media exchange on one channel, not effective
QUIC preludes or reachability of every Discord voice server. Ordinary Auto
port recovery is being checked separately from the opt-in prelude experiment.

### zapret2 packet-helper extension (2026-10-27, WIP)

The official zapret2 v1.0.6 archive was fetched and its SHA-256 checked against
the GitHub release digest. Its MIT notice and unmodified 1200-byte Google QUIC
fixture are pinned under fastcord-platform/native/zapret2. The existing pinned
WinDivert helper now accepts only canonical public Discord UDP discovery,
matches its SSRC and endpoint, skips virtual copies and applies six raw fakes
at physical egress, optionally followed by exact-preserving IPv4 fragmentation.
It does not capture RTP/RTCP or accept arbitrary payloads. The standard account
client remains unprivileged; the user explicitly starts the helper with UAC.

Offline tests cover request/protocol validation, normal RTP bypassing the
factory, absence of implicit elevation and proxy fallback, caller cancellation,
first-three-request bounds, and exact UDP reassembly/header checksums including
an independent RFC 1071 vector. These tests do not establish live effectiveness.

The user reported audible voice in a manual session at 17:24. Route-correlated
Throne logs showed SOCKS output for that session's TCP and UDP, so this is not
accepted as direct packet-bypass evidence. The UDP RPC ended with BrokenPipe;
the helper previously exited on any per-request capture error. It now returns
the complete operation status and keeps the channel available for subsequent
flows. A timeout-then-success protocol regression test covers this behavior;
malformed input still terminates the request. Hidden-child diagnostic writes
are also best effort, so a missing console cannot panic the helper.

After the user selected direct again, the corrected build was restarted. Its
parent log confirmed a successfully applied PacketFake TCP operation. Direct
voice was subsequently checked in the same worktree executable built at 17:35.
The user relaunched it from Explorer at 17:51, started the helper, and used the
approved mochi alt. Terra observed the server and channels loaded, followed by
the green voice-connected state in [ZDRT x CHRV]. Throne connection records at
17:53 and 17:55 correlated fastcord TCP and UDP with outbound/direct. At 17:55
the user explicitly confirmed hearing the other participants and being heard.
The running parent and helper were both present; the call was left running.

This establishes human-confirmed bidirectional voice on that direct route with
the integrated helper enabled. The Explorer launch did not retain the previous
stderr redirection or opt-in media counters, so this call does not identify the
successful UDP variant or prove that raw fakes, rather than ordinary discovery
and port recovery, were necessary. VPN remained enabled for other applications;
direct egress is supported by the matched Throne outbound records, not an
OS-interface inference. Other voice endpoints, reconnect stability, screen
sharing and a sustained thirty-minute call remain separate acceptance checks.

### Live check record (#218, 2026-10-27)

`fastcord --network-check` (debug build, no account, no packet helper) with the `throne-tun` VPN adapter up: Off and Auto both returned anonymous gateway HTTP 200 with zero recovery dials. This does not exercise the SNI-fragment path; that needs the VPN off on the filtered route (maintainer run).

## Opt-in Gateway startup timing (#224)

Set `FASTCORD_GATEWAY_TIMING=1` before launch. Main Gateway attempts produce at most two fixed-size stderr records (READY queued and termination; an interrupted/failed connect reports on teardown). Records contain process-local attempt IDs, elapsed milliseconds, six stage offsets in order: discovery completion/cached address, aggregate DNS/TCP/TLS completion, WebSocket Upgrade, HELLO, Identify/Resume send completion, READY queued. They also contain successful decrypted read count/bytes, first/last read offsets, complete WebSocket message count, decoded Gateway message count and total decode microseconds. Read counts include the HTTP Upgrade response and bytes below WebSocket framing; they never retain or print payloads, addresses, credentials or session identifiers. The opt-in path has fixed memory and no polling. Defaults, pacing and authenticated retry budgets are unchanged.

`gateway::timing::tests::partial_frame_reads_remain_visible_when_the_remainder_stalls` supplies a partial binary frame through a duplex stream, stalls its remaining bytes for30 simulated seconds, and verifies that read evidence survives without a complete-message count. Existing Gateway missing-READY/heartbeat/backpressure fixtures remain required. DNS/TCP/TLS are one aggregate stage; version discovery before Gateway startup and actual reducer/UI publication are not yet individually instrumented. `ReadyQueued` means decoded READY was queued for the consumer, not that UI rendering completed. Cold/warm p50/p95 on direct filtered and unfiltered routes remains pending; this diagnostic change makes no acceleration claim.
## Issue 229 request-scoped check evidence
The consuming `HttpBuilder::anonymous_get` creates a fresh one-request client and strips default headers. Its receipt records mode/profile/helper availability, bridge-selected strategy, actual packet factory invocation and successful helper completion separately. Receipts are carried by CONNECT attempt IDs; recovery probes do not write them. `opaque_forwarding_preserves_tls_h2_alpn_and_backpressure` now checks receipts for SNI packet transforms and no-SNI Direct with zero factory calls. `failed_helper_attempt_is_distinct_from_completion_and_other_requests` covers reset and isolation. `anonymous_receipts_do_not_inherit_headers_or_claim_packet_attempts_on_timeout` checks bounded anonymous failures in Off/Auto/Proxy settings against local fixtures (not real proxy-route acceptance). No receipt claims Gateway READY, UDP reachability or censorship recovery.
## Native packet capability boundary (#226, #227)

`fastcord_platform::network::packet_capabilities()` reports build support separately from whether a helper is running. Windows ships TCP fake/overlap and canonical UDP discovery fake/IPv4 fragmentation behind explicit authorization. IPv6 fragmentation is unsupported. Linux and macOS report all these native packet operations unsupported; their helper entry returns `ErrorKind::Unsupported` before any side effect. The Network page states this explicitly while retaining portable TLS recovery/configured proxies. These facts do not promise provider effectiveness.

The shipping Linux packet backend remains unimplemented; the admission/lifetime core and isolated fixture described below are implemented. The2026-11-01 host process has zero effective/permitted capabilities, but fresh unprivileged user+network namespaces work and provide namespace-scoped capabilities. The reusable synthetic proof below exercises actual kernel NFQUEUE there. No host firewall rules were altered. A future backend needs verified socket/flow ownership beyond UID, peer-credential IPC, per-operation allowlists/deadlines, parent watchdog and session-only cleanup; namespace/veth tests must prove another same-UID application's exclusion before live filtered-route acceptance. No synthetic mock is presented as proof of kernel interception or cleanup.


### Fresh isolated NFQUEUE proof (2026-11-01)

`python3 scripts/test_nfqueue_namespace.py` passed on Linux. The launcher creates fresh user/network namespaces and validates a single-user mapping plus a different network namespace before any nft operation. Two child processes with the same UID send only synthetic loopback UDP. A rule matches the registered socket's source port, destination tuple and socket mark. One registered datagram is held in actual NFQUEUE while the unregistered datagram delivers; NF_ACCEPT without replacement then delivers the registered payload byte-for-byte. No second packet appears in the queue. After the queue listener closes, `queue bypass` allows the marked fixture to deliver. Finally the owned table is deleted, an empty ruleset is verified, child processes/sockets close, and namespace teardown removes remaining local state.

This demonstrates kernel feasibility and exclusion of a cooperative same-UID unregistered flow, not authenticated production registration or adversarial mark isolation. The prototype runs namespace-capable child processes, so it does not prove that another privileged process cannot spoof a mark/tuple. Production still needs authenticated Unix IPC and socket-FD/peer validation; narrowly allocated marks and collision handling; revocation/race protection; bounded operation allowlists and byte/deadline ledgers; helper capability dropping; parent watchdog/session cleanup; veth/physical-egress, NAT/TUN/offload/IPv6 coverage; and package/user-authorization integration. The shipping capability remains Unsupported. No packet fakes, protected data or public endpoints are involved in this original proof. The following increment implements and tests the admission/FD/lifetime subset; full production integration remains pending.

### Authenticated Linux admission and cleanup fixture (#226, 2026-11-02)

The new Rust `network::linux_session` core has real Unix-socket/SCM_RIGHTS tests for exact peer and per-message sender identity, same-UID delegated IPC impersonation, invalid/truncated/extra descriptors, queued-FD cleanup, FD/domain/protocol/bind/reuse checks, replay/operation/lease caps, fixed admission deadlines, disconnect/stop and parent exit. The parent-exit test retains the peer's IPC socket in another process so ordinary EOF cannot substitute for SO_PEERPIDFD. Cancellation keeps the source port occupied until the interception consumer releases its lease; a retired cookie cannot be re-admitted before cleanup. Deadline tests advance the production expiry branch explicitly, without wall-clock sleeps.

Run the kernel fixture on Linux (6.5+ for SO_PEERPIDFD, as recorded in the [kernel networking maintainers’ 2023 report](https://people.kernel.org/kuba/netdev-in-2023)):

```sh
cargo test -p fastcord-platform --locked network::linux_session
cargo build -p fastcord-platform --features test-support --example linux_packet_session --locked
python3 scripts/test_linux_packet_session.py target/debug/examples/linux_packet_session
```

The launcher first checks that the Rust example refuses the host namespace, then always creates a fresh user+network namespace. The Rust core independently verifies the namespace's actual owning user namespace, so a caller-supplied different name is not sufficient. One adversarial FD originates in the host namespace and is refused without sending on it. Registered and unregistered clients have the same UID and drop all effective/permitted/inheritable capabilities before sending. Their canonical discovery bytes are identical; only the Rust-admitted exclusive source tuple reaches the actual kernel NFQUEUE. A same-UID reuse-port bind cannot hijack that source. NF_ACCEPT delivers the registered bytes unchanged, and a stopped listener permits future matching packets through queue bypass. Cancellation then leaves the source port pinned by the Rust consumer while the harness deletes its owned nft table; only after the cleanup acknowledgement can a fresh socket reuse the port and deliver normally. The final namespace ruleset is empty.

Fresh local standalone compilation of the exact Rust core/example and the real kernel fixture passed these checks on 2026-11-02. Coordinated Cargo validation then rebuilt this worktree: all 16 session tests and the Unsupported-capability regression passed, all-target Clippy with test-support passed with warnings denied, and the namespace fixture passed again using the Cargo-built example. The fixture has no host nft operation, external endpoint, packet mark, raw injection or live account. Its Python NFQUEUE adapter remains test orchestration; no production queue engine or elevated launcher is claimed. Full packet engine/operation ledgers, privileged packaging, crash/SIGKILL cleanup, physical egress and filtered-route media gates remain pending. `packet_capabilities()` and `packet_helper_entry()` remain Unsupported on Linux.

## OS-profile persistence (#222)
Linux uses read-only NetworkManager ActiveConnections and Connection.Active Uuid/State/Default/Default6/Vpn properties and change signals. Official API: https://networkmanager.pages.freedesktop.org/NetworkManager/NetworkManager/gdbus-org.freedesktop.NetworkManager.Connection.Active.html . UUID identifies a reusable OS profile, so it never proves the current ISP or route. Raw values are not logged. No activation, settings mutation or connectivity-check method is called.

Offline regressions: profile_digest_is_salted_and_ambiguous_routes_have_no_identity; installation_salt_is_stable_and_corrupt_salt_is_not_replaced; profile_hint_reorders_but_needs_two_confirmations_and_failure_evicts_it; failed_saved_probe_is_evicted_and_disallowed_hint_cannot_expand_a_plan; profile_load_rejects_old_generations_and_never_refunds_budgets; profile_preferences_stay_out_of_proxy_and_off_modes. Optional read-only native test live_networkmanager_profile_returns_only_a_digest requires an unambiguous NM-managed default profile. It does not contact any public origin or change network settings. Two distinct filtered-route acceptance is still outstanding.
`gateway::timing::tests::partial_frame_reads_remain_visible_when_the_remainder_stalls` supplies a partial binary frame through a duplex stream, stalls its remaining bytes for30 simulated seconds, and verifies that read evidence survives without a complete-message count. Existing Gateway missing-READY/heartbeat/backpressure fixtures remain required. Separate fixed transport counters now report DNS, TCP and TLS durations/counts plus the last completion offset, including failure/cancellation. Proxy establishment is explicitly aggregated as `proxy`, without claiming origin DNS/TCP visibility inside that proxy. Manifest/build lookup reports its own bounded pre-Gateway record distinguishing a cached result, the caller performing the fetch, and a caller waiting for the shared fetch. The READY state carries an opt-in process-local stamp through the existing bounded outbox; the app records reducer application, snapshot publication and UI consumption against the original attempt ID. `UiConsumed` means presentation state was applied, not that GPU pixels were displayed. Cold/warm p50/p95 on direct filtered and unfiltered routes remains pending; this diagnostic change makes no acceleration claim.


The DNS/TCP/TLS instrumentation is carried explicitly through the socket broker; ordinary callers pass no timing collector. Phase guards also finish when their future is cancelled. Stored fields contain no addresses, payloads or TLS errors. Fixed stamp metadata is included in outbox accounting and is copied onto the READY event when queued, so a later attempt cannot relabel an earlier pending event. Tests cover cancelled DNS accounting, two queued attempts retaining distinct stamps and returning byte permits, and version-fetch timing without changing cached-fetch behavior. No probes, retries, deadlines, buffer limits or authentication rules change.

### Portable packet-plan validation (#225)

Pure TCP and UDP packet layouts now live in `fastcord-platform::network::packet_plan`, shared directly with the Windows WinDivert caller and compiled for offline tests on every host. `cargo test -p fastcord-platform --locked network::packet_plan` runs the existing profile/reassembly/malformed-input tests on Linux too, plus fixed independently calculated completed-UDP/IPv4-fragment vectors and explicit IPv4-options/IPv6 capability checks. The fixed synthetic vector uses documentation-only addresses, SSRC `0x12345678`, a completed UDP checksum `0x2303` and IPv4 checksum `0x3c1c`; expected fragments were calculated with separate Python network-order integer arithmetic, not generated by the Rust planner under test.

This extraction changes no profile or default: six pinned QUIC fakes, then original discovery or IPv4 offsets 8/16. IPv4 options remain accepted only by the unfragmented UDP profile; fragmented profiles reject them. IPv6 profiles retain the original unfragmented datagram and extension headers remain unsupported. Fake checksum completion, interception, interface selection, cancellation/watchdog and egress still belong to the Windows backend. These portable tests cannot establish those native operations, capture-based verification, effectiveness on filtered networks or human-confirmed media. New STUN fixtures, repeat/pacing catalogs and IPv6 fragmentation are not introduced. Fresh execution results are recorded by the central runner separately.


### Parameterized TLS catalog (#222, local incremental implementation)

Catalog version 5 retains the existing six initial automatic strategies unchanged.
The extended catalog adds finite typed profiles:2/4/6 ClientHello
records, writes one byte before/after record boundaries,0/10/30ms pacing,
and TLS-record/write splits at the first payload byte, midsld or hostname
midpoint. There are42 entries including legacy defaults. No arbitrary script,
offset or timer enters a profile; privilege/helper defaults are unchanged.
Saved hints carry only these categorical parameters and expire across catalog
versions; no endpoint or payload is persisted.

Local coverage extends actual rustls TLS1.2/TLS1.3+HRR+application data tests
across the catalog, plus partial-input/short-write transcript preservation,
actual fragment-count/write-boundary/timing assertions, protected-data pass
through, bounded-parameter rejection and hint serialization/version expiry.
Formatting/diff checks passed; fresh Cargo results are supplied by the central
runner. Subsequent ordinary recovery campaigns continue a bounded per-target
catalog cursor after those first six fail; no timer or background traffic starts
another campaign. The existing six-probe/twelve-dial campaign, 60-second gap and
24-dial rolling ten-minute cap remain unchanged. A manual full-search action
continues a concrete failed endpoint under the same limits and cannot override
Off, Proxy, no-SNI or an explicitly selected packet helper. Progress distinguishes
incomplete, confirmed and fully tried catalogs, and shows the remaining cooldown.
At most 16 target cursors are retained, cleared by network generation changes;
only typed role preferences can be persisted. Per-role confirmed TLS candidates
are displayed without pretending every pooled connection uses a global winner.
No filtered-route success is claimed. Session-only source binding now reaches direct HTTP, WebSocket, bridge upstream,
probe TCP and UDP, with strict family/no-unbound-fallback behavior. Proxy explicitly
ignores it; the private CONNECT link/fake origins do not inherit it. Added actual
loopback TCP local-address/family/refused-bind tests, production UDP bind (no
packets sent), synthetic local proxy isolation, and UI numeric-input/no-persistence
checks. These prove local socket behavior, not physical-egress/TUN escape or
filtered-route effectiveness. Central Cargo results remain pending for this increment.
