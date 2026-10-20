#!/usr/bin/env python3
"""Regenerate aac-lc-tone.m4a: 0.5 s of a stereo test tone (left 440 Hz, right 880 Hz,
amplitude 12000/32768, 48 kHz 16-bit PCM) encoded to AAC-LC by the Windows Media Foundation
AAC encoder (Windows.Media.Transcoding, profile CreateM4a/Medium, which resamples to
44.1 kHz). No third-party audio. Windows only (needs powershell.exe); the committed output
is the encoder's own file, so the exact bytes depend on the Windows build. The container
fixture generator (../video/gen_container_fixture.py) reads the frames from that file.

    python fixtures/audio/gen_aac_tone.py
"""

import math
import pathlib
import struct
import subprocess
import tempfile
import wave

HERE = pathlib.Path(__file__).resolve().parent

with tempfile.TemporaryDirectory() as tmp:
    wav = pathlib.Path(tmp) / "tone.wav"
    with wave.open(str(wav), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(48000)
        w.writeframes(b"".join(
            struct.pack("<hh", int(12000 * math.sin(2 * math.pi * 440 * i / 48000)),
                        int(12000 * math.sin(2 * math.pi * 880 * i / 48000)))
            for i in range(24000)))
    subprocess.run(
        ["powershell.exe", "-NoProfile", "-File", str(HERE / "gen_aac_tone.ps1"),
         "-in", str(wav), "-out", str(HERE / "aac-lc-tone.m4a")],
        check=True)
