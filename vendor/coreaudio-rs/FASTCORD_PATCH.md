# coreaudio-rs 0.14.2 failed-registration ownership patch

This directory vendors the `coreaudio-rs` 0.14.2 crates.io package (MIT OR
Apache-2.0; `LICENSE-MIT`, `LICENSE-APACHE`). It is pulled in only by cpal on
Apple targets. The root workspace patches only this package.

- Upstream: <https://github.com/RustAudio/coreaudio-rs>
- Package: <https://static.crates.io/crates/coreaudio-rs/coreaudio-rs-0.14.2.crate>
- Package SHA-256:
  `7d5d7dca3ebcf65a035582c9ad4385371a9d9ee6537474d2a278f4e1e475bb58`
- Issue: <https://github.com/skadibtw/fastcord/issues/68>

## Defect

`AudioUnit::set_render_callback` and `set_input_callback` called
`Box::into_raw` on the callback wrapper (and, for input, the `AudioBufferList`
plus the `mem::forget`-ed byte `Vec`) before the fallible `set_property`. The
raw pointers were stored for `Drop` only after success, so a failed
registration leaked all of them.

## Change

Only `src/audio_unit/render_callback.rs`. Ownership stays with local `Box`/`Vec`
values until `set_property` succeeds; the C-visible pointer is taken from the
box contents (stable across `Box::into_raw`). Every early return, including a
failed registration, therefore frees the wrapper, the buffer list and its
backing buffer through ordinary drops. On success the same values are
relinquished exactly as before, so `free_render_callback`,
`free_input_callback`, replacement and `Drop` are unchanged.

## Validation

Checked on Windows by cross-compiling only:

```sh
cargo check --target aarch64-apple-darwin -p coreaudio-rs
```

Not yet exercised natively: forced registration failure, successful
creation/replacement/play/stop, and the #51 device tests need a Mac (see
`docs/TESTING.md`). Remove this patch when cpal consumes a coreaudio-rs release
containing an equivalent fix.
