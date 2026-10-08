# fastcord

Native, lightweight third-party Discord desktop client written in Rust.

> **Warning:** fastcord logs in with a user token. Third-party clients violate Discord's Terms of Service; your account can be banned. Use an alternate account.

Status: early development. See [docs/SPEC.md](docs/SPEC.md) for the design and milestone plan.

## Build

Requires Rust (pinned by `rust-toolchain.toml`) and, on Windows, Visual Studio Build Tools with the C++ workload.

```sh
cargo run -p fastcord-app          # debug
cargo build --release -p fastcord-app
```

## Login

The login screen explains the Terms of Service/account-ban risk and requires
acknowledgement before any login method can start. Two methods are offered:

- **QR code (recommended).** Shows a QR code; scan it with Discord on your phone
  (Settings, Scan QR Code) and confirm there. fastcord uses Discord's remote-auth
  protocol with a fresh RSA key pair per attempt, so you never handle the token.
  The screen shows which account scanned the code. Cancel, **Generate new code**,
  and the server-side expiry (about 5 minutes) all work; keys and tickets live only
  in memory and are dropped when an attempt ends. If Discord demands a CAPTCHA,
  fastcord stops and points you to token login or the official client; it does not
  solve or bypass CAPTCHAs.
- **Token login (advanced).** Paste a token into the masked field.

Either way the resulting token is validated once with Discord's `GET /users/@me`
before the account is shown, then follows the same storage path below.

By default, the validated token is saved under service `fastcord` and the Discord
account ID in Windows Credential Manager, macOS Keychain, or Linux Secret Service.
On launch, saved credentials are discovered in that native store and validated
again; if several accounts are saved, choose which one to restore. No token is
stored in a configuration file. Linux requires a running Secret Service provider
and a session D-Bus connection.

Choose **Memory-only session** to skip saving. If the native store is locked or
unavailable, the app explains the failure and asks before continuing without
saving. Previously saved credentials are not removed by choosing memory-only;
use **Forget saved login** or log out of the restored account to remove them.

**Log out** stops authenticated work, drops account state and in-memory secrets,
and removes the saved account credential. If removal fails, the app says logout
is incomplete and offers a retry; it does not pretend the credential is gone.
Local logout does not revoke every Discord session.

## Connection

After login the account screen connects to Discord's Gateway (v10, zlib-stream
compression) and shows its state: connected (with server and direct-message
counts), reconnecting, a rejected login (back to the login screen), or stopped
with the reason. It identifies as the Discord web client (Chrome profile, current
build number fetched from discord.com, bundled fallback); the exact payload is
documented in [docs/PROTOCOL.md](docs/PROTOCOL.md). Dropped connections resume
the session with bounded, jittered backoff. If Discord demands an action or a
challenge on the account, fastcord stops and says so; it never completes or
bypasses one. Logging out closes the Gateway session, so the account goes
offline.

Account state is held in one bounded store (12 MiB capacity-accounted budget
for guild, channel, role, user, member, and voice data). Its subscription API
follows only the selected server and active voice server, with bounded visible
member-list ranges. Guild/channel lists are permission-gated and virtualized.
Passive updates keep other servers' channel and voice markers current. Optional
unreferenced detail is evicted first, releasing its backing allocations. If
required identity/permissions or pinned state alone cannot fit, the connection
stops with a visible safety error instead of silently truncating the account.

## Message history

Opening an accessible text or announcement channel loads only that channel's
latest 50 messages. Scroll toward the beginning to page backward; newer pages
can be fetched again after older browsing has evicted them. Jump to latest
returns to the live end. New messages follow the viewport only when it was
already at the bottom. Gateway edits and deletions update retained rows.

History is memory-only and byte-bounded (16 MiB, at most 500 messages per
channel and 2,000 globally). Variable-height rows build widgets only around the
viewport, with measured heights and message-ID scroll anchoring. Attachment
metadata is shown; fetching and viewing attachment images is milestone 14.

## Sending messages

Where your account may send, a composer sits under the timeline: Enter sends,
Shift+Enter starts a new line, and unsent drafts are kept per channel while you
look elsewhere. Mentions of users and roles you type notify them; `@everyone`
and `@here` notify only when you tick the box for that message.

A message you sent shows as "Sending…" until Discord confirms it, then appears
once in the timeline. fastcord never sends a message again by itself. If
Discord refused it, it is marked "Not sent" with Retry, Edit, and Discard. If
the connection failed while sending, Discord may or may not have it, so it is
marked "Not confirmed": it resolves by itself if Discord reports it, and Retry
send cannot post it twice within a few minutes, because it reuses the original
nonce.

## Editing and deleting your messages

Point at one of your own messages to show Edit and Delete; other people's
messages, and system messages, never offer them. Edit moves the text into the
message box (your unsent draft is set aside and comes back afterwards): Enter
saves, Escape cancels. Only the text is changed, so attachments stay. Delete
asks for confirmation in the message itself. A change that Discord did not
confirm stays on the message with Retry and Dismiss; fastcord never retries by
itself. Edits and deletions made in another client appear here as well.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
