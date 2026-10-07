# Testing

## Automated (CI, every push)
`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on Windows, Linux, and macOS. Tests are deterministic and offline: protocol fixtures in `fixtures/`, injected clocks, crypto/media vectors. No live credentials in CI.

### REST transport and scheduler (milestone 4)
- `cargo test -p fastcord-discord --locked` exercises the real scheduler with an injected monotonic clock and sanitized `fixtures/rest/rate-limits.json`; no sockets, credentials, wall-clock sleeps, or server timestamps are involved.
- Coverage includes shared bucket hashes across methods/routes without conflating major IDs, fractional resets, global and per-route 429 pauses (body/header/scope), concurrent reservations and out-of-order responses, unknown-route serialization, priority ordering, cancellation, byte budgets, and account-wide stop when the transport observes a 401.
- Raw `Authorization` headers and token/request Debug redaction are checked offline. Network and 5xx failures are returned to callers for reconciliation/retry policy; only confirmed 429 rejections are rescheduled. A failed or ambiguous message POST must not be retried blindly.
- Live alt-account `GET /users/@me`, correct-account display, app restart restoration, and real-account credential deletion remain pending: the maintainer's authenticated browser was not available through the browser relay or CDP during milestone 5. The implemented token path is covered offline and can be exercised without touching a real account.

### Login and native credential storage (milestone 5)
- `cargo test -p fastcord-app --locked` uses sanitized `fixtures/rest/current-user.json` and deterministic UI-state transitions. It covers warning acknowledgement, input byte limits, invalid-token stop, explicit consent after storage failure, saved versus memory-only state, immediate authentication-worker cancellation on logout, state/secret purge, deletion failure/retry, saved-account identity mismatch, and redaction of text-input messages/session results.
- `cargo test -p fastcord-platform --locked` uses the keyring ecosystem's in-memory test store for the real platform adapter's account naming/discovery, save/load/overwrite/delete, locked-store behavior, idempotent deletion, malformed-secret handling, and categorical error redaction. CI never opens a real user's native store.
- In an interactive desktop session with an unlocked native store, run `cargo test -p fastcord-platform native_dummy_save_restart_restore_delete --locked -- --ignored`. It saves a dummy secret under the separate service `fastcord-m5-test`, opens a new store instance to discover/restore it, deletes it, and confirms both retrieval and discovery are empty. No real Discord credential is used or printed.
- Windows UI checklist: launch `target/debug/fastcord.exe`, observe the ToS/alt-account warning and masked input, acknowledge it, submit an obviously fake token, and confirm the real Discord API rejection returns to the login screen without an authenticated account. Check the explicit memory-only choice and the lack of inactive QR buttons.
- Windows milestone 5 results: the actual debug app opened and closed normally; the warning, acknowledgement, explicit memory-only choice, masked token field, and lack of inactive QR controls were observed. Submitting an obviously fake token through the real UI reached Discord and returned the rejected-token error with the field cleared and no authenticated account. The separate-service native dummy save/new-store restore/delete check also passed and confirmed no test entry remained.
- Pending live-alt checklist (Windows, Linux, macOS): obtain only the authorized alt token, paste it into the real app, confirm display name/username/ID against that account, restart and confirm only the saved token restores, then log out and verify the native entry is absent. Check locked-store explanation/explicit nonpersistent consent; restart a memory-only session and confirm it does not restore. Test a real invalid token and confirm login stops.
- macOS maintainer checklist: use the CI-built native app; perform the preceding live-alt checks with login Keychain unlocked, then locked/denied; use Keychain Access to confirm service `fastcord` and the alt's account ID disappear on logout. Linux native checks need an interactive session D-Bus and Secret Service provider.

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
