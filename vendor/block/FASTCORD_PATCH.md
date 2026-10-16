# block 0.1.6 compatibility patch

This directory vendors the `block` 0.1.6 crates.io package, authored by Steven
Sheldon and licensed under MIT. The root workspace patches only this package;
the iced, wgpu, and Metal versions and their public APIs are unchanged.

- Upstream: <https://github.com/SSheldon/rust-block>
- Package: <https://static.crates.io/crates/block/block-0.1.6.crate>
- Package SHA-256:
  `0d8c1fef690941d3e7788d328517591fecc684c084084702d6ff1641e993699a`
- Issue: <https://github.com/skadibtw/fastcord/issues/53>

The source was copied from the package with the above verified checksum.
The published manifest, README, and original test sources are retained.
The package declares MIT but omits a license file; `LICENSE` supplies the same
standard MIT text and author credit already shipped in fastcord's generated
third-party notices.

## Changes from the package

- Replace the private, uninhabited `Class` enum with an inhabited opaque
  `#[repr(C)]` structure. Only pointers to this type are stored; the class
  object is never read, allocated, or passed by value.
- Take `_NSConcreteStackBlock`'s address with `ptr::addr_of!`, without creating
  a reference. The symbol and the block's pointer-sized `isa` field are unchanged.
- Spell out the existing C ABI on foreign declarations and function pointers.
  Vendored path dependencies otherwise surface Rust's `missing_abi` warnings.
- Add this provenance document, the MIT text, and a standalone runtime smoke
  test. No lint is suppressed, and no registry-cache source is modified.

Upstream master still had the empty enum declaration when inspected on
2026-10-15. Remove this patch when the renderer dependency chain can consume a
compatible upstream correction, after repeating the native tests below.

## Validation

From the repository root on macOS, compile with warnings denied and run tests
against the actual system Blocks runtime:

```sh
cargo rustc --locked -p block --lib -- -D warnings
rustc --edition=2024 --test vendor/block/tests/runtime.rs \
  --extern block=target/debug/libblock.rlib -D warnings \
  -o target/block-runtime-tests
./target/block-runtime-tests
```

The standalone test avoids the published package's unavailable local Objective-C
test-helper directory and checks stack invocation, heap copying, cloning, and
exactly-once captured-resource release. The original upstream test sources stay
unchanged for comparison; `cargo test -p block` cannot test a dependency outside
the workspace.

On a non-Apple host with the Apple Rust target installed, the following checks
the declaration and ABI types but does not execute the runtime or test Metal:

```sh
cargo rustc --locked --target aarch64-apple-darwin -p block --lib -- -D warnings
rustc --edition=2024 --test --emit=metadata --target aarch64-apple-darwin \
  vendor/block/tests/runtime.rs \
  --extern block=target/aarch64-apple-darwin/debug/libblock.rlib \
  -D warnings -o target/block-runtime-tests.rmeta
```

Both native macOS architectures still need the runtime test and packaged
fastcord launches with Metal and the existing tiny-skia fallback. Cross-compiling
this crate does not satisfy those acceptance checks.
