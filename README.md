# fastcord

Native, lightweight third-party Discord desktop client written in Rust.

> **Warning:** fastcord logs in with a user token. Third-party clients violate Discord's Terms of Service; your account can be banned. Use an alternate account.

Status: early development. See [docs/SPEC.md](docs/SPEC.md) for the design and milestone plan.

The approved delivery order and issue links are in [docs/ROADMAP.md](docs/ROADMAP.md): finish and verify the three-platform MVP, then add everyday messaging and community features. Implemented features and passed live/native acceptance are tracked separately.

## Download

Builds are on the [Releases](https://github.com/skadibtw/fastcord/releases) page. On Windows, download `fastcord-v…-x86_64-pc-windows-msvc.exe` and run it (the `.zip` holds the same executable with the licences). It links the C runtime statically, so no Visual C++ Redistributable is needed. Builds are unsigned, so SmartScreen may warn. Linux and macOS archives are `.tar.gz`; on macOS right-click the app and choose Open the first time.

## Network recovery

**Network recovery** on the login screen selects Off, Auto or a user-configured TCP proxy before sign-in.
Auto uses bounded TLS-only checks of a failed required service and application-local
TLS splitting. It preserves certificate verification and all account retry limits.
It changes no system network settings. TCP write boundaries are best effort;
fragmentation is not universal and the base splitting catalog has no confirmed
blocked-network effectiveness.

On Windows x64, **Start packet recovery (administrator prompt)** optionally
starts a separate WinDivert helper for this session. Confirm UAC manually; the
ordinary account client stays unprivileged. The helper sees only a bounded public
TLS ClientHello for a particular fastcord flow. It supports packet fake/disorder
and sequence overlap inspired by zapret-discord-youtube. The native screen can
check the public Discord API before sign-in. PacketFake restored API access,
alt-account validation and Gateway READY on the reported VPN-excluded Windows
route; that is not a universal bypass or UDP/media claim. Keep the packaged
`windivert/` folder beside release builds. Start is explicit after each restart;
Off, Proxy and Stop release the helper. See [the evidence](docs/NETWORK_TESTING.md).
The base catalog splits TLS records/writes around SNI; it does not implicitly
enable the optional packet helper. Successful offline transport tests alone do
not establish blocked-network effectiveness.
HTTP/HTTPS CONNECT and SOCKS5 send destination names to the selected proxy.
Origin TLS verification remains on; failures never fall back directly. Proxy
passwords live in native credential storage, bound to the exact proxy settings.
CONNECT is TCP-only; the UI explains media limitations before joining. SOCKS5
uses a separate UDP association for voice, camera and Go Live. The proxy must
support UDP and UDP to it must be reachable; this does not carry media over TCP.
Control loss stops media, with no direct fallback. SOCKS5 itself does not encrypt
the connection to the proxy. Real relay interoperability/NAT checks are pending.
Network settings use `network-settings.json` beside the audio settings, with a
separate fake-mode namespace. See SPEC §5.4 and TESTING for limits and evidence.

Debug builds support `fastcord.exe --network-check` for process-specific routing
diagnosis (for example, a VPN exclusion tied to this executable). It makes one
anonymous gateway-discovery GET in Off and Auto, then runs the existing bounded
TLS check only after an eligible Auto failure. It prints categorical results,
loads no credentials or saved settings, starts no account/Gateway session and
changes no system routing. Run the exact executable covered by the routing rule.

**Experimental UDP discovery preludes** are a separate opt-in, off by default.
They try at most three direct UDP flows before voice protocol selection, with
visible progress and fixed request/byte/deadline limits. The bundled zero-filled
laboratory payload has no proven blocked-network effectiveness. Proxy mode keeps
this experiment disabled; it never opens a direct media route behind a proxy.

## Build

Requires Rust (pinned by `rust-toolchain.toml`), CMake (the audio engine builds the bundled libopus), on Windows Visual Studio Build Tools with the C++ workload, and on Linux the ALSA, VA-API and PipeWire development headers plus clang (`libasound2-dev libva-dev libpipewire-0.3-dev clang`).

```sh
cargo run -p fastcord-app          # debug
cargo build --release -p fastcord-app
```

## Audio devices

Open **Settings** on the login screen (or the **Audio settings** icon once signed in) to choose microphone
input and playback output independently. **System default** follows the OS
default during a call; an explicit device never falls back to another output
(particularly speakers) when it disappears. A stopped direction is reported
inline, while the healthy direction keeps running; select a device to retry.
Use **Refresh devices** after plugging a device in.

Choices are machine-local, non-secret settings in `fastcord/audio-devices.json`
under the OS configuration directory (`%APPDATA%` on Windows). Stable backend
IDs, not display names, restore the choices on restart; missing saved devices
remain selected and marked unavailable. Discovery and supported-format probes
run off the UI thread, only when settings or a call needs them. Switching devices
in an active call preserves local mute/deafen and per-user playback volumes.
Official-client call interoperability acceptance is still pending; see
[docs/TESTING.md](docs/TESTING.md).

## Noise suppression

The same dialog chooses how your microphone is cleaned up before it is sent. It
applies to the microphone only (never to shared or system audio), is saved with
the device choices, and changes a running call without reopening any device.

- **Off** sends the microphone as captured.
- **Light** (default) is RNNoise through the `nnnoiseless` crate: 0.3 % of one
  core and 10 ms of delay on the reference PC. It reduces steady noise such as
  fans and hiss but lets keyboard clicks and background voices through.
- **High quality** is DeepFilterNet 3: it also suppresses keyboard noise and
  background voices, at 30 ms of delay, 4-5 % of one core and about 13 MB of
  memory while selected. It is built on its own thread when you select it, so
  the call keeps running with Light until it is ready (about 0.15 s), and
  switching away releases its memory.

During a call the dialog shows what is actually running; otherwise it shows your
saved choice. If High quality cannot be built or cannot keep up with real time
(debug builds are much slower), the call continues with Light and the dialog
says why. A muted microphone runs no noise suppression. Noise suppression is
not echo cancellation: use a headset. Measurements and pending live checks are
in [docs/TESTING.md](docs/TESTING.md); the engines' licenses are in
[docs/THIRD_PARTY_NOTICES.md](docs/THIRD_PARTY_NOTICES.md).

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
- **Email and password.** Windows only for now. Discord may ask for a CAPTCHA, which fastcord shows to you in a
  temporary window (it never solves or bypasses it), and for an authenticator code. Password login is the
  riskiest method for an account; prefer QR.
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

After login the account screen connects to Discord's Gateway (zlib-stream
compression) and shows its state: connected (with server and direct-message
counts), reconnecting, a rejected login (back to the login screen), or stopped
with the reason. It presents the official Discord client's identity (the
official desktop/Electron profile): the fields, version, headers and cookies
the official client uses, with the identity work specified in
[docs/adr/0005-client-identity.md](docs/adr/0005-client-identity.md) and the
remaining gaps and known residuals listed in
[docs/IDENTITY_AUDIT.md](docs/IDENTITY_AUDIT.md); the same profile is what its
API requests carry, and the exact payload is documented in
[docs/PROTOCOL.md](docs/PROTOCOL.md). Impersonation is best-effort and does not
guarantee that an account survives. Dropped connections resume
the session with bounded, jittered backoff. If Discord demands an action or a
challenge on the account, fastcord stops and says so; it never completes or
bypasses one. Logging out closes the Gateway session, so the account goes
offline.

Account state is held in one bounded store (12 MiB capacity-accounted budget
for guild, channel, role, user, member, and voice data). Its subscription API
follows only the selected server and active voice server, with bounded visible
member-list ranges. Guild/channel lists are permission-gated and virtualized.
Passive updates keep other servers' channel and voice markers current. Optional
unreferenced detail is evicted first, releasing its backing allocations. If
required identity/permissions or pinned state alone cannot fit, the connection
stops with a visible safety error instead of silently truncating the account.

## Message history

Opening an accessible text or announcement channel loads only that channel's
latest 50 messages. Scroll toward the beginning to page backward; newer pages
can be fetched again after older browsing has evicted them. Jump to latest
returns to the live end. New messages follow the viewport only when it was
already at the bottom. Gateway edits and deletions update retained rows.

History is memory-only and byte-bounded (16 MiB, at most 500 messages per
channel and 2,000 globally). Variable-height rows build widgets only around the
viewport, with measured heights and message-ID scroll anchoring. Image attachments
offer Load preview, which fetches a thumbnail on request, and a larger in-app view.
Every attachment shows its name, size, and type with Download, which opens a native
save dialog. Passive types (images, audio, video, PDF, and text) also offer Open with
the OS default application; other types are Download-only. Previews and Open use a
byte-bounded 256 MiB disk cache with a 64 MiB per-file limit.

## Sending messages

Where your account may send, a composer sits under the timeline: Enter sends,
Shift+Enter starts a new line, and unsent drafts are kept per channel while you
look elsewhere. Mentions of users and roles you type notify them; `@everyone`
and `@here` notify only when you tick the box for that message.

A message you sent shows as "Sending…" until Discord confirms it, then appears
once in the timeline. fastcord never sends a message again by itself. If
Discord refused it, it is marked "Not sent" with Retry, Edit, and Discard. If
the connection failed while sending, Discord may or may not have it, so it is
marked "Not confirmed": it resolves by itself if Discord reports it, and Retry
send cannot post it twice within a few minutes, because it reuses the original
nonce.

## Editing and deleting your messages

Point at one of your own messages to show Edit and Delete; other people's
messages, and system messages, never offer them. Edit moves the text into the
message box (your unsent draft is set aside and comes back afterwards): Enter
saves, Escape cancels. Only the text is changed, so attachments stay. Delete
asks for confirmation in the message itself. A change that Discord did not
confirm stays on the message with Retry and Dismiss; fastcord never retries by
itself. Edits and deletions made in another client appear here as well.

## Reactions

Reactions appear as chips under a message: the emoji and how many people used
it, highlighted when you did. Click a chip to add your reaction or take it
back; point at a message and press **React** (or the **+** after its chips) to
choose another emoji in the picker. Right-click a chip to see who reacted, which
is loaded only then (the first 25 names). A reaction you just asked for looks
selected but stays disabled until Discord confirms it. If Discord refuses it,
the reason appears above the message box and fastcord does not try again by
itself. Only normal reactions are offered. Reactions added, removed, or cleared
in other clients appear here too; fastcord keeps counts and your own reaction,
never the full list of who reacted.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
