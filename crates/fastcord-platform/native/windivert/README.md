# Optional Windows packet helper

Unmodified WinDivert 2.2.2-A x64 library and signed driver from the official
[release](https://github.com/basil00/WinDivert/releases/tag/v2.2.2). File and
archive hashes are in `provenance.json`. Nothing is loaded until the user starts
packet recovery and accepts Windows UAC. The ordinary account client remains
unprivileged. This is not a system-wide zapret installation.

WinDivert is distributed here under **LGPL version 3** (the upstream alternative
is GPL version 2). `LICENSE` includes LGPLv3 and its incorporated GPLv3 text.
`WinDivert-2.2.2-source.zip` contains the matching upstream source and build
scripts. UDP recovery data and profiles from zapret2 have their own MIT notice
and provenance in the adjacent `zapret2/` directory. Strategy references are
documented in `docs/NETWORK_TESTING.md` in the fastcord repository.

The helper dynamically loads the library from this directory beside a release
executable, or this source directory in debug builds. Full paths and SHA-256
checks prevent accidental DLL substitution during elevation. To use a modified
ABI-compatible WinDivert, build it with the included upstream build scripts,
replace these two binaries, update their expected hashes in
`crates/fastcord-platform/src/network/packet.rs`, and rebuild fastcord with
`cargo build --release -p fastcord-app`. All application source is available
under the repository's MIT license; no relinking keys or application secrets
are required. Windows independently requires appropriately signed drivers.

Dropping the helper pipe ends the elevated child and closes its filter handles.
WinDivert manages its driver service; stopping the filter does not promise that
Windows immediately removes the loaded driver/service. The bundled upstream
source/documentation describes its service lifecycle and removal.
