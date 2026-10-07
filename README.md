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

## License

GPL-3.0-only. See [LICENSE](LICENSE).
