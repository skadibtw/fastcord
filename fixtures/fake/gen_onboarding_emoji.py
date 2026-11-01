"""Regenerate the original 32px Go-stone fixture; standard library only."""
import pathlib
import struct
import zlib


def chunk(kind, data):
    return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data))


pixels = bytearray()
for y in range(32):
    pixels.append(0)
    for x in range(32):
        distance = (x - 15.5) ** 2 + (y - 15.5) ** 2
        rgba = (240, 200, 120, 255) if distance > 180 else (28, 34, 42, 255)
        if (x - 11) ** 2 + (y - 10) ** 2 < 12:
            rgba = (120, 130, 144, 255)
        pixels.extend(rgba)
header = struct.pack('!2I5B', 32, 32, 8, 6, 0, 0, 0)
png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', header) + chunk(b'IDAT', zlib.compress(pixels, 9)) + chunk(b'IEND', b'')
pathlib.Path(__file__).with_name('onboarding-emoji.png').write_bytes(png)
