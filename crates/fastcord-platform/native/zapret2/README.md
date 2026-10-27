# zapret2 UDP profile data

The unmodified `quic_initial_www_google_com.bin` comes from the official
[zapret2 v1.0.6 release](https://github.com/bol-van/zapret2/releases/tag/v1.0.6),
commit `221d7dff24bfec0cd5ac0a5070b8acef2008af3d`. Release and selected-file
hashes are in `provenance.json`. Upstream's MIT license is included verbatim
in `LICENSE` and shipped with Windows archives.

The optional Windows packet helper implements the upstream six-repeat UDP fake
profile, followed by original discovery or IPv4 fragmentation at transport
offsets 8 and 16. References: upstream `docs/manual.en.md`,
`lua/zapret-antidpi.lua` and `blockcheck2.d/standard/90-quic.sh`.
It uses fastcord's existing pinned WinDivert library. The ordinary signed-in
client remains unprivileged; no separate winws2/Cygwin/Lua runtime is bundled.

Capture matches the public destination, UDP port, canonical discovery size
and current call's SSRC. Virtual-interface copies pass unchanged; recovery is
applied only at physical egress. Protected media is outside the filter. Each
filter ends after one discovery or the watchdog deadline. The explicitly
started helper is tied to its authenticated parent pipe.

Profiles remain candidates until validated on the user's filtered route.
