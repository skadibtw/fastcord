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
