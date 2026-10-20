# Free communication coverage

Approved 2026-10-16. [Tracker #109](https://github.com/skadibtw/fastcord/issues/109) extends [#67](https://github.com/skadibtw/fastcord/issues/67) beyond the existing MVP and M39-M51 everyday-client stage. Existing feature issues are reused, not recreated. Product availability is checked against official documentation and then verified with a free alt account; this file is not a runtime-success claim.

## Scope and order

The user now includes all free communication features: camera/video, inline media, communication-related server/channel/role management and moderation, community participation and manual communication-app interactions. Earlier exclusions of these functions remain historical boundaries of M1-M51 only. Preserve SPEC §16's existing order, then implement the new rows below in order, respecting prerequisites in each issue. One coherent feature per commit with tests/docs/changelog; the final new acceptance issue depends on every new capability.

Exclude paid/Nitro features, purchases/billing/boosts, premium cosmetics/themes and quality/upload privileges, Nitro bookmarks/reminders, game library/game activities, store/ads and selfbot automation. Ordinary incoming content created by paying users remains readable when the account has access. Do not implement premium sending or bypass server access controls. Free soundboard is server-local; camera backgrounds include free blur/presets only. No FFmpeg, unbounded queues/caches or UI-thread decoding.

## Existing coverage

| Capability already tracked | Existing issues |
|---|---|
| Login/QR, credentials, Gateway, bounded state and navigation | [#1](https://github.com/skadibtw/fastcord/issues/1), [#2](https://github.com/skadibtw/fastcord/issues/2), [#3](https://github.com/skadibtw/fastcord/issues/3), [#4](https://github.com/skadibtw/fastcord/issues/4), [#5](https://github.com/skadibtw/fastcord/issues/5), [#6](https://github.com/skadibtw/fastcord/issues/6), [#7](https://github.com/skadibtw/fastcord/issues/7) |
| Timeline, send/edit/delete/reply, DM/group DM, attachments | [#8](https://github.com/skadibtw/fastcord/issues/8), [#9](https://github.com/skadibtw/fastcord/issues/9), [#10](https://github.com/skadibtw/fastcord/issues/10), [#11](https://github.com/skadibtw/fastcord/issues/11), [#12](https://github.com/skadibtw/fastcord/issues/12), [#13](https://github.com/skadibtw/fastcord/issues/13) |
| Emoji/reactions, encrypted voice/DAVE, devices and denoising | [#14](https://github.com/skadibtw/fastcord/issues/14), [#15](https://github.com/skadibtw/fastcord/issues/15), [#16](https://github.com/skadibtw/fastcord/issues/16), [#17](https://github.com/skadibtw/fastcord/issues/17), [#18](https://github.com/skadibtw/fastcord/issues/18), [#19](https://github.com/skadibtw/fastcord/issues/19), [#20](https://github.com/skadibtw/fastcord/issues/20), [#21](https://github.com/skadibtw/fastcord/issues/21), [#22](https://github.com/skadibtw/fastcord/issues/22), [#23](https://github.com/skadibtw/fastcord/issues/23) |
| Screen-share transport/codecs/playback/capture/audio and MVP release | [#24](https://github.com/skadibtw/fastcord/issues/24), [#25](https://github.com/skadibtw/fastcord/issues/25), [#26](https://github.com/skadibtw/fastcord/issues/26), [#27](https://github.com/skadibtw/fastcord/issues/27), [#28](https://github.com/skadibtw/fastcord/issues/28), [#29](https://github.com/skadibtw/fastcord/issues/29), [#30](https://github.com/skadibtw/fastcord/issues/30), [#31](https://github.com/skadibtw/fastcord/issues/31), [#32](https://github.com/skadibtw/fastcord/issues/32), [#33](https://github.com/skadibtw/fastcord/issues/33), [#34](https://github.com/skadibtw/fastcord/issues/34), [#35](https://github.com/skadibtw/fastcord/issues/35), [#36](https://github.com/skadibtw/fastcord/issues/36), [#37](https://github.com/skadibtw/fastcord/issues/37), [#38](https://github.com/skadibtw/fastcord/issues/38) |
| Formatted messages/mentions, uploads, read state/notifications, search/pins | [#54](https://github.com/skadibtw/fastcord/issues/54), [#55](https://github.com/skadibtw/fastcord/issues/55), [#56](https://github.com/skadibtw/fastcord/issues/56), [#57](https://github.com/skadibtw/fastcord/issues/57), [#58](https://github.com/skadibtw/fastcord/issues/58), [#59](https://github.com/skadibtw/fastcord/issues/59) |
| Friends/profiles/status, private calls, threads/forums, navigation and M51 acceptance | [#60](https://github.com/skadibtw/fastcord/issues/60), [#61](https://github.com/skadibtw/fastcord/issues/61), [#62](https://github.com/skadibtw/fastcord/issues/62), [#63](https://github.com/skadibtw/fastcord/issues/63), [#64](https://github.com/skadibtw/fastcord/issues/64), [#65](https://github.com/skadibtw/fastcord/issues/65), [#66](https://github.com/skadibtw/fastcord/issues/66) |
| Remaining maintenance regressions | [#51](https://github.com/skadibtw/fastcord/issues/51), [#52](https://github.com/skadibtw/fastcord/issues/52), [#53](https://github.com/skadibtw/fastcord/issues/53), [#68](https://github.com/skadibtw/fastcord/issues/68) |


Closed issue state does not replace outstanding live/native evidence in TESTING.md. M51/#66 accepts the everyday-client stage, not the later parity stage.

## Expanded stage, ordered after M51

| Order | Area | Issue / capability |
|---|---|---|
| 1 | Text | [#69: Typing indicators](https://github.com/skadibtw/fastcord/issues/69) |
| 2 | Text | [#70: GIF picker and animated attachment playback](https://github.com/skadibtw/fastcord/issues/70) |
| 3 | Text | [#71: Free sticker selection and rendering](https://github.com/skadibtw/fastcord/issues/71) |
| 4 | Text | [#72: Message forwarding and snapshots](https://github.com/skadibtw/fastcord/issues/72) |
| 5 | Text | [#73: Poll creation, voting and results](https://github.com/skadibtw/fastcord/issues/73) |
| 6 | Media | [#74: Inline audio and video attachment player](https://github.com/skadibtw/fastcord/issues/74) |
| 6a | Media | [#116: MP4/WebM demuxer for inline attachment playback](https://github.com/skadibtw/fastcord/issues/116) (follow-up completing the video part of #74) |
| 7 | Media | [#75: Voice message playback and explicit recording](https://github.com/skadibtw/fastcord/issues/75) |
| 8 | Text | [#76: Complete free message actions](https://github.com/skadibtw/fastcord/issues/76) |
| 9 | Navigation | [#77: Unified unread and mention inbox](https://github.com/skadibtw/fastcord/issues/77) |
| 10 | Social | [#78: Group DM membership and conversation management](https://github.com/skadibtw/fastcord/issues/78) |
| 11 | Social | [#79: Free profile editing and contextual identity](https://github.com/skadibtw/fastcord/issues/79) |
| 12 | Safety | [#80: Message requests, Ignore and communication privacy](https://github.com/skadibtw/fastcord/issues/80) |
| 13 | Safety | [#81: Report messages, users and communication abuse](https://github.com/skadibtw/fastcord/issues/81) |
| 14 | Voice | [#82: Push-to-talk and global communication shortcuts](https://github.com/skadibtw/fastcord/issues/82) |
| 15 | Voice | [#83: Echo cancellation and microphone diagnostics](https://github.com/skadibtw/fastcord/issues/83) |
| 16 | Video | [#84: Camera send and receive in calls](https://github.com/skadibtw/fastcord/issues/84) |
| 17 | Video | [#85: Free camera blur and preset backgrounds](https://github.com/skadibtw/fastcord/issues/85) |
| 18 | Video | [#86: Multi-participant call layouts and stream controls](https://github.com/skadibtw/fastcord/issues/86) |
| 19 | Voice | [#87: Free server-local soundboard](https://github.com/skadibtw/fastcord/issues/87) |
| 20 | Text | [#88: Text chat in voice and Stage channels](https://github.com/skadibtw/fastcord/issues/88) |
| 21 | Voice | [#89: Stage audience, speaker and moderator flows](https://github.com/skadibtw/fastcord/issues/89) |
| 22 | Community | [#90: Server discovery, invitations and membership](https://github.com/skadibtw/fastcord/issues/90) |
| 23 | Community | [#91: Rules screening, onboarding and server guide](https://github.com/skadibtw/fastcord/issues/91) |
| 24 | Community | [#92: Scheduled community events](https://github.com/skadibtw/fastcord/issues/92) |
| 25 | Administration | [#93: Server and channel management for communication](https://github.com/skadibtw/fastcord/issues/93) |
| 26 | Administration | [#94: Roles and channel access management](https://github.com/skadibtw/fastcord/issues/94) |
| 27 | Administration | [#95: Manual moderation and audit log](https://github.com/skadibtw/fastcord/issues/95) |
| 28 | Administration | [#96: Server AutoMod and communication safety configuration](https://github.com/skadibtw/fastcord/issues/96) |
| 29 | Administration | [#97: Configure community onboarding and server guide](https://github.com/skadibtw/fastcord/issues/97) |
| 30 | Community | [#98: Announcement channel publishing and following](https://github.com/skadibtw/fastcord/issues/98) |
| 31 | Administration | [#99: Manage free server emoji, stickers and soundboard assets](https://github.com/skadibtw/fastcord/issues/99) |
| 32 | Apps | [#100: Manual commands and interactive message components](https://github.com/skadibtw/fastcord/issues/100) |
| 33 | Apps | [#101: Communication app access and non-game integrations](https://github.com/skadibtw/fastcord/issues/101) |
| 34 | Apps | [#102: Free non-game shared communication activities](https://github.com/skadibtw/fastcord/issues/102) |
| 35 | UX | [#103: Accessible communication interface and chat preferences](https://github.com/skadibtw/fastcord/issues/103) |
| 36 | UX | [#104: Text-to-speech and read-aloud communication](https://github.com/skadibtw/fastcord/issues/104) |
| 37 | UX | [#105: Desktop communication integration and overlays](https://github.com/skadibtw/fastcord/issues/105) |
| 38 | Navigation | [#106: Conversation organization and cross-client preferences](https://github.com/skadibtw/fastcord/issues/106) |
| 39 | Account | [#107: Account access and security needed for communication](https://github.com/skadibtw/fastcord/issues/107) |
| 40 | Acceptance | [#108: Free communication parity acceptance](https://github.com/skadibtw/fastcord/issues/108) |

Each issue contains boundaries, concrete acceptance, prerequisite references and official source links. Record implementation, offline, native and official-client evidence separately. The parity gate repeats SPEC §12 and adds explicit budgets for multi-party media, while preserving the original idle and one-send/one-watch baseline requirements.

## Source and eligibility notes

The [messaging index](https://support.discord.com/hc/en-us/sections/115000458672-Messaging-and-Chat) and [voice/video index](https://support.discord.com/hc/en-us/sections/201110537-Voice-Video-and-Streaming) ground the feature inventory; child issues cite focused references. [Bookmarks/reminders](https://support.discord.com/hc/en-us/articles/26442819646999-Message-Bookmarks-and-Reminders) are documented as Nitro and excluded. [Soundboard](https://support.discord.com/hc/en-us/articles/12612888127767-Discord-Soundboard-Guide-Using-Adding-and-Managing-Sounds) distinguishes server-local use from Nitro cross-server/entrance behavior. [Video backgrounds](https://support.discord.com/hc/en-us/articles/4413490191127-Video-Backgrounds) distinguish free presets from paid custom uploads; bundle only appropriately licensed assets.

[Voice messages](https://support.discord.com/hc/en-us/articles/13091096725527-Voice-Messages) currently document mobile-only recording/sending. Playback is a required capability; recording in fastcord is a compatibility-gated extension in #75, not an assumed existing desktop feature. Unknown routes, platform restrictions, challenges and free-account eligibility must be resolved before marking an issue complete. These dated sources are evidence of product behavior, not a guarantee that a user-account API route is supported.
