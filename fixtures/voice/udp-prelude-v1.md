# Experimental UDP prelude v1

`udp-prelude-v1.bin` is exactly 1,200 zero bytes, original generated work under
the repository license. It is invalid Discord discovery, STUN and RTP, contains
no captured packet or secret, and is not borrowed from zapret or another project.
Its effectiveness on a filtered network has not been established.

SHA-256: `655a3ef0465a9f30fddf25f4dde0c19a05c6f9069b83961800c1944165955273`.

Regenerate from the repository root with Python:

```python
from pathlib import Path
Path("fixtures/voice/udp-prelude-v1.bin").write_bytes(bytes(1200))
```

It is used only after explicit opt-in, once per fresh direct UDP flow (three,
then six datagrams), before normal IP discovery and never after Select Protocol.
See SPEC §5.4 and NETWORK_TESTING.md for all per-user-join limits and open gates.
