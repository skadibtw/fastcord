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

The app-managed buffer bound is an explicit code budget: shared 32 tunnel slots,
64 KiB forwarding per tunnel, 8 KiB bounded header staging, four <=96 KiB TLS
stages, 16 remembered connection endpoints and bounded selector/settings state.
Kernel socket buffers and existing HTTP/TLS allocations are outside that budget
and must be reported separately in a resource measurement.
