#!/usr/bin/env bash
# Package a release build of fastcord for one target.
# Usage: scripts/package.sh <target-triple> <version-without-v> <out-dir>
set -euo pipefail

target="$1"
version="$2"
out="$3"
name="fastcord-v${version}-${target}"
stage="$(mktemp -d)/${name}"
bin_dir="target/${target}/release"

mkdir -p "$stage" "$out"
cp LICENSE README.md CHANGELOG.md docs/THIRD_PARTY_NOTICES.md docs/THIRD_PARTY_CRATES.md "$stage/"
# Every archive statically links libopus: refuse to package without its BSD-3-Clause notice.
grep -q "Copyright 2001-2023 Xiph.Org, Skype Limited" "$stage/THIRD_PARTY_NOTICES.md" || {
  echo "THIRD_PARTY_NOTICES.md lacks the libopus notice" >&2
  exit 1
}
mkdir -p "$stage/assets/emoji"
cp assets/emoji/NOTICE.txt assets/emoji/TWEMOJI-LICENSE-GRAPHICS.txt \
  assets/emoji/TWEMOJI-LICENSE.txt assets/emoji/EMOJI-DATA-LICENSE.txt \
  "$stage/assets/emoji/"

case "$target" in
  *-windows-msvc)
    cp "${bin_dir}/fastcord.exe" "$stage/"
    zip_out="$(cd "$out" && (pwd -W 2>/dev/null || pwd))/${name}.zip"
    (cd "$(dirname "$stage")" && 7z a -tzip -bso0 "$zip_out" "$name")
    ;;
  *-apple-darwin)
    app="$stage/fastcord.app/Contents"
    mkdir -p "$app/MacOS"
    cp "${bin_dir}/fastcord" "$app/MacOS/"
    sed "s/@VERSION@/${version}/g" packaging/macos/Info.plist > "$app/Info.plist"
    tar -C "$(dirname "$stage")" -czf "${out}/${name}.tar.gz" "$name"
    ;;
  *-linux-gnu)
    cp "${bin_dir}/fastcord" "$stage/"
    tar -C "$(dirname "$stage")" -czf "${out}/${name}.tar.gz" "$name"
    ;;
  *)
    echo "unsupported target: $target" >&2
    exit 1
    ;;
esac
