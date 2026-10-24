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
stages, 16 remembered connection endpoints and bounded selector/settings state.
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
