# fastcord icons

Original artwork created for fastcord, distributed under the repository's MIT
license. No Discord logo, icon font, third-party artwork or network resources.

- `ui/`: 90 standalone SVG glyphs, including state variants; `currentColor`, a
  20 x 20 grid, 1.7 px round strokes. Muted/deafened/hidden states add a visible
  red slash. Use 14 px for participant tags, 16 px beside captions, 20 px for
  tools and 24 px in the server rail. Keep tooltips on buttons without captions.
- `app/`: original speech/bolt mark on a blurple rounded square. Editable SVG,
  transparent PNGs at 16/24/32/48/64/128/256/512/1024 px, a seven-size Windows
  ICO and a seven-size macOS ICNS (up to 1024 px).
- `preview.png`: complete contact sheet; `index.html`: offline SVG gallery with
  light/dark backgrounds. Open either locally to review the artwork.
- `generate.py`: the single source for all paths, SVGs, desktop exports and
  `crates/fastcord-app/src/icons/catalog.rs`. Edit paths here, then regenerate;
  do not edit generated files individually.

Regenerate using Python 3 with Pillow 12.3.0 and the pinned Rust toolchain's
`rustfmt` on PATH (or in `~/.cargo/bin`):

```sh
python assets/icons/generate.py
python assets/icons/generate.py --check
cargo fmt --all -- --check
```

Pillow is authoring-only. Building or running fastcord uses only committed
assets and existing Rust dependencies. Canvas paths are compiled into the app,
work with wgpu and tiny-skia and have no font lookup, SVG parsing, image decoding,
animation timer or network request. The one 64 px window icon decodes at startup.

For visual review in the real native renderer without logging in or starting
Discord connections (debug builds only):

```sh
cargo run -p fastcord-app -- --icon-gallery
```

Windows embeds `app/fastcord.ico` into the executable with the Windows SDK
resource compiler. A normal rustup shell finds it in the installed Windows 10/11
SDK; `RC` can override the compiler path. macOS packaging copies the ICNS into
`Contents/Resources` and declares it in `Info.plist`. Linux archives include the
desktop entry and hicolor PNG/SVG icons under `share/`; installation is manual
as described in `docs/RELEASE.md`.

Verification on Windows, 2026-10-22: all 90 glyphs inspected in the running
native gallery at 14/20/32 px; the window's title-bar icon is present. The
software-renderer draw test covers every glyph and both boolean states; footer
reachability and disabled-action checks pass. Workspace tests pass with local
loopback networking available (sandbox network restrictions otherwise prevent
the existing HTTP fixture tests). SVG XML, all PNG dimensions, all seven ICO
entries, ICNS decoding and the macOS plist were checked; packaging shell syntax,
generated-file freshness and workspace formatting pass. Native macOS/Linux
desktop presentation remains to be checked on those systems.
