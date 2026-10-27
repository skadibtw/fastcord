"""Synthetic, credential-free UDP classification candidates; not a bypass claim.

QUIC v1 key derivation/protection: RFC 9001 sections 5.1-5.4.
All TLS fields and connection identifiers here are original deterministic data.
Nothing is sent by this generator. No capture or third-party packet is used.
"""
from pathlib import Path
import hashlib
import hmac
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

def u16(n):
    return n.to_bytes(2, "big")

def vint(n):
    if n < 64:
        return bytes([n])
    assert n < 16384
    return u16(n | 0x4000)

def expand(secret, label, size):
    label = b"tls13 " + label
    info = u16(size) + bytes([len(label)]) + label + b"\0"
    return hmac.new(secret, info + b"\x01", hashlib.sha256).digest()[:size]

def keys(dcid):
    salt = bytes.fromhex("38762cf7f55934b34d179ae6a4c80cadccbb7f0a")
    initial = hmac.new(salt, dcid, hashlib.sha256).digest()
    client = expand(initial, b"client in", 32)
    return tuple(expand(client, label, size) for label, size in
                 [(b"quic key", 16), (b"quic iv", 12), (b"quic hp", 16)])

# Independently published RFC 9001 A.1 key material checks the derivation.
assert keys(bytes.fromhex("8394c8f03e515708"))[0].hex() == "1f369613dd76d5467730efcbe3b1a22d"

dcid = hashlib.sha256(b"fastcord public UDP candidate v2 destination").digest()[:8]
scid = hashlib.sha256(b"fastcord public UDP candidate v2 source").digest()[:8]
name = b"www.google.com"
extensions = []
def extension(kind, payload):
    extensions.append(u16(kind) + u16(len(payload)) + payload)

extension(0, u16(len(name) + 3) + b"\0" + u16(len(name)) + name)
extension(10, bytes.fromhex("0002001d"))
extension(13, bytes.fromhex("000408040403"))
extension(43, bytes.fromhex("020304"))
extension(51, bytes.fromhex("0024001d0020") + b"\x09" + bytes(31))
extension(16, bytes.fromhex("0003026833"))
extension(57, b"\x0f\x08" + scid + b"\x03\x02" + vint(1200))
ext = b"".join(extensions)
body = (b"\x03\x03" + hashlib.sha256(b"fastcord public QUIC ClientHello v2").digest()
        + b"\0\0\x02\x13\x01\x01\0" + u16(len(ext)) + ext)
hello = b"\x01" + len(body).to_bytes(3, "big") + body
crypto = b"\x06\0" + vint(len(hello)) + hello
# Four-byte packet number zero; only CRYPTO and PADDING, no application data.
prefix = b"\xc3\0\0\0\x01\x08" + dcid + b"\x08" + scid + b"\0"
payload_size = 1200 - len(prefix) - 2 - 4 - 16
header = prefix + vint(4 + payload_size + 16) + bytes(4)
payload = crypto + bytes(payload_size - len(crypto))
key, iv, hp = keys(dcid)
encrypted = AESGCM(key).encrypt(iv, payload, header)
mask = Cipher(algorithms.AES(hp), modes.ECB()).encryptor().update(encrypted[:16])
protected = bytearray(header)
protected[0] ^= mask[0] & 15
for i in range(4):
    protected[-4 + i] ^= mask[1 + i]
quic = bytes(protected) + encrypted
assert len(quic) == 1200
assert AESGCM(key).decrypt(iv, encrypted, header) == payload
assert b"www.google.com" in hello

root = Path(__file__).parent
(root / 'udp-quic-v2.bin').write_bytes(quic)
print('quic', len(quic), hashlib.sha256(quic).hexdigest())
