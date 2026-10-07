# Testing

## Automated (CI, every push)
`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on Windows, Linux, and macOS. Tests are deterministic and offline: protocol fixtures in `fixtures/`, injected clocks, crypto/media vectors. No live credentials in CI.

### REST transport and scheduler (milestone 4)
- `cargo test -p fastcord-discord --locked` exercises the real scheduler with an injected monotonic clock and sanitized `fixtures/rest/rate-limits.json`; no sockets, credentials, wall-clock sleeps, or server timestamps are involved.
- Coverage includes shared bucket hashes across methods/routes without conflating major IDs, fractional resets, global and per-route 429 pauses (body/header/scope), concurrent reservations and out-of-order responses, unknown-route serialization, priority ordering, cancellation, byte budgets, and account-wide stop when the transport observes a 401.
- Raw `Authorization` headers and token/request Debug redaction are checked offline. Network and 5xx failures are returned to callers for reconciliation/retry policy; only confirmed 429 rejections are rescheduled. A failed or ambiguous message POST must not be retried blindly.
- Live alt-account `GET /users/@me`, including actual credential validation, is deferred to milestone 5; these offline tests are not a claim of live Discord acceptance.

## Live / manual
| Platform | Where | Who | Limits |
|---|---|---|---|
| Windows | Maintainer's PC | Agent | Primary target; real GPU, audio devices, capture |
| Linux | Hyper-V VM on the same PC (Wayland + PipeWire) | Agent | No GPU passthrough: VA-API hardware paths unverified; software paths and portal capture testable |
| macOS | Maintainer's Mac | Maintainer, from a CI-built package | Agent prepares a checklist per milestone; milestone completes when the maintainer reports pass |

### Interop partner for voice and Go Live
- **fastcord** runs as the **alt account** (already logged in on discord.com in the maintainer's browser; obtain login via QR/token from that session).
- **Partner:** the maintainer's official Discord client on the same PC, in a private test server/DM with the alt.
- The agent verifies results itself: captures the official client's audio output (WASAPI process loopback) and analyzes it (presence, level, a known test tone/phrase), and screenshots the official client's window for video. No second human is required.
- The agent never sends messages, joins channels, or changes settings from the maintainer's main account beyond the agreed test server/DM.

Live Discord tests use the alt account only. Never commit tokens, voice tokens, DAVE state, or signed CDN URLs; sanitize captured fixtures.

## Per-milestone checklist
Each milestone in `docs/SPEC.md` §16 lists acceptance criteria. Before committing, exercise every criterion on the platforms it names and note the result (and any measurement) in the commit message body.
