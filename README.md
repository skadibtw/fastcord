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

The current app supports advanced token paste and local logout. The login screen
explains the Terms of Service/account-ban risk, requires acknowledgement, and
masks the token like a password. It validates the token once with Discord's
`GET /users/@me` before showing the account.

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
Local logout does not revoke every Discord session. QR login is milestone 5a,
not an inactive button in the current UI.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
