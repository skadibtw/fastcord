# ADR 0006: QR login plus token paste

Status: accepted (2026-10-07)

## Context
Token paste alone forces users to open browser DevTools — awkward and a common scam vector. Password login brings CAPTCHA/MFA challenges and higher ban risk.

## Decision
Support both:
- **QR login** (primary): Discord remote-auth protocol — WebSocket, ephemeral RSA-OAEP key pair, QR code scanned by the phone app, encrypted ticket exchanged for the token. Milestone 5a.
- **Token paste** (advanced): milestone 5.

Both feed the same validation (`GET /users/@me`) and keyring storage path.

## Consequences
- Normal users never handle the raw token.
- One extra milestone; the remote-auth protocol is community-documented (open risk U6), so verify with the alt account.
- CAPTCHA during ticket exchange stops the flow; no bypass.

## Rejected
- **Token only:** poor UX, encourages unsafe token handling.
- **QR only:** no fallback when remote auth breaks or a phone is unavailable.
- **Email/password:** CAPTCHA/MFA handling and higher account risk.

## Implementation notes (milestone 5a)
- Protocol (v2, verified live against `wss://remote-auth-gateway.discord.gg/?v=2` with `Origin: https://discord.com`): `hello` → `init` (base64 SPKI DER) → `nonce_proof` (the client answers with `{"op":"nonce_proof","nonce":<base64url of the decrypted nonce>}`) → `pending_remote_init` (fingerprint, checked against the base64url SHA-256 of our SPKI before any QR is shown) → `pending_ticket` (user payload encrypted to our key) → `pending_login` (ticket) or `cancel`. Heartbeat acks were observed across two heartbeat intervals. Observed session lifetimes were 299–339 s; the client uses the server's `timeout_ms`, clamped to 5 s–15 min.
- Ticket exchange is `POST /users/@me/remote-auth/login` without credentials. A bogus ticket returned HTTP 404. Any non-success response carrying `captcha_key`/`captcha_sitekey`/`captcha_service` is a CAPTCHA stop; the response body is never copied into messages or logs.
- Crypto: `rsa` 0.9 (pure Rust), RSA-2048, OAEP with SHA-256 and MGF1-SHA-256, blinded private-key operations. RUSTSEC-2023-0071 (Marvin timing side channel) concerns decryption timing observable by a network attacker; here only locally received ciphertext is decrypted and no timing reaches a third party. Revisit when `rsa` 0.10 is stable. Debug builds compile the bignum crates with `opt-level = 3` so key generation stays fast.
- TLS for the WebSocket uses the same rustls/ring stack and webpki roots as REST (no second crypto provider); `tokio-tungstenite` provides only WebSocket framing.
- Still unverified without the alt account's phone: the `pending_ticket`/`pending_login` payloads of a real scan, the real ticket exchange response, and whether Discord answers that exchange with a CAPTCHA (risk U6).
