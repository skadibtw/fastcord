# ashpd 0.11.1 ScreenCast v6 serial patch

Vendored from the crates.io ashpd 0.11.1 package, MIT license in LICENSE.
Package: https://static.crates.io/crates/ashpd/ashpd-0.11.1.crate
SHA-256: `d2f3f79755c74fd155000314eb349864caa787c6592eace6c6882dad873d9c39`
Upstream: https://github.com/bilelmoussaoui/ashpd
Reference API: https://github.com/bilelmoussaoui/ashpd/blob/8fd77df02790e9a8b8ec16563224e6ae7501d303/client/src/desktop/screencast.rs
Issue: https://github.com/skadibtw/fastcord/issues/33

The runtime change in src/desktop/screencast.rs deserializes the optional u64
`pipewire-serial` property and expose Stream::pipewire_serial(). This follows
upstream's v6 accessor without adopting its zbus 6 migration. Old responses
remain valid. Unknown unrelated properties retain upstream behavior.

src/desktop/icon.rs and src/desktop/clipboard.rs explicitly spell four inferred
lifetimes as `'_`, preserving the signatures' meaning while avoiding Rust 1.99
`mismatched_lifetime_syntaxes` warnings exposed by the local path dependency.

Platform capture tests deserialize old/v6/malformed D-Bus stream structures
and check serial-versus-node connection planning. Real portal acceptance
remains pending. Remove the patch after adopting a compatible upstream release
with the accessor. Upstream package Cargo.lock and registry metadata omitted.
