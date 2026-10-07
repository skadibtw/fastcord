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

## License

GPL-3.0-only. See [LICENSE](LICENSE).
