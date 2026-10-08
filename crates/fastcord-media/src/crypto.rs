//! Discord's rtpsize transport encryption (SPEC §6.2).
//!
//! Packet layout for both modes:
//!
//! ```text
//! clear header (AAD) | ciphertext | 16-byte tag | 4-byte big-endian nonce
//! ```
//!
//! The clear header is the RTP fixed header, CSRCs, and extension preamble
//! ([`RtpHeader::clear_len`](crate::rtp::RtpHeader::clear_len)), or the 8-byte
//! RTCP header and sender SSRC ([`RTCP_CLEAR_LEN`]). The AEAD nonce is the
//! 32-bit counter in big-endian order followed by zero bytes: 12 bytes for
//! AES-256-GCM, 24 for XChaCha20-Poly1305. The suffix is stripped before
//! verification.
//!
//! The send counter starts at 0 and is never reused for a key: once nonce
//! `u32::MAX` has been used, [`TransportCipher::seal`] refuses with
//! [`CryptoError::NonceExhausted`] and the session must get a new key. This is
//! unrelated to RTP sequence numbers, which wrap every 65,536 packets.

use std::fmt;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::array::Array;
use aes_gcm::aead::{AeadInOut, KeyInit};
use chacha20poly1305::XChaCha20Poly1305;
use zeroize::Zeroizing;

pub const KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
pub const NONCE_SUFFIX_LEN: usize = 4;
/// Bytes rtpsize encryption adds to a packet.
pub const OVERHEAD: usize = TAG_LEN + NONCE_SUFFIX_LEN;
/// RTCP header (version/count/type/length) plus the sender SSRC.
pub const RTCP_CLEAR_LEN: usize = 8;

/// The transport modes fastcord implements. Deprecated XSalsa20 modes are
/// deliberately absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransportMode {
    Aes256GcmRtpSize,
    XChaCha20Poly1305RtpSize,
}

impl TransportMode {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Aes256GcmRtpSize => "aead_aes256_gcm_rtpsize",
            Self::XChaCha20Poly1305RtpSize => "aead_xchacha20_poly1305_rtpsize",
        }
    }

    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "aead_aes256_gcm_rtpsize" => Some(Self::Aes256GcmRtpSize),
            "aead_xchacha20_poly1305_rtpsize" => Some(Self::XChaCha20Poly1305RtpSize),
            _ => None,
        }
    }

    /// Chooses from the modes offered in voice READY: AES-GCM when offered and
    /// `aes_accelerated`, otherwise XChaCha20 (which Discord always offers).
    pub fn select<'a>(
        offered: impl IntoIterator<Item = &'a str>,
        aes_accelerated: bool,
    ) -> Option<Self> {
        let mut aes = false;
        let mut xchacha = false;
        for name in offered {
            match Self::from_wire(name) {
                Some(Self::Aes256GcmRtpSize) => aes = true,
                Some(Self::XChaCha20Poly1305RtpSize) => xchacha = true,
                None => {}
            }
        }
        if aes && (aes_accelerated || !xchacha) {
            Some(Self::Aes256GcmRtpSize)
        } else if xchacha {
            Some(Self::XChaCha20Poly1305RtpSize)
        } else {
            None
        }
    }
}

/// Whether this CPU has AES and carry-less multiply instructions, which the
/// RustCrypto AES-GCM implementation detects and uses at run time.
pub fn aes_accelerated() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        std::arch::is_x86_feature_detected!("aes")
            && std::arch::is_x86_feature_detected!("pclmulqdq")
    }
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("aes")
            && std::arch::is_aarch64_feature_detected!("pmull")
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CryptoError {
    /// Session Description carried a key that is not 32 bytes.
    KeyLength,
    /// Every nonce of this key has been used; a new key is required.
    NonceExhausted,
    /// Shorter than clear header + tag + nonce suffix.
    Truncated,
    /// Authentication failed: tampered, wrong key, or not for this session.
    Authentication,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::KeyLength => "transport key is not 32 bytes",
            Self::NonceExhausted => "transport nonce space exhausted; a new key is required",
            Self::Truncated => "encrypted packet is too short",
            Self::Authentication => "packet failed transport authentication",
        })
    }
}

impl std::error::Error for CryptoError {}

/// The 32-byte transport secret from Session Description. Zeroed on drop and
/// never printed.
pub struct TransportKey(Zeroizing<[u8; KEY_LEN]>);

impl TransportKey {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CryptoError> {
        let mut key = Zeroizing::new([0; KEY_LEN]);
        if bytes.len() != KEY_LEN {
            return Err(CryptoError::KeyLength);
        }
        key.copy_from_slice(bytes);
        Ok(Self(key))
    }
}

impl fmt::Debug for TransportKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TransportKey(<redacted>)")
    }
}

enum Cipher {
    Aes(Box<Aes256Gcm>),
    XChaCha(Box<XChaCha20Poly1305>),
}

/// One session's transport cipher: encrypts outgoing RTP/RTCP with a strictly
/// increasing nonce and verifies incoming packets.
pub struct TransportCipher {
    mode: TransportMode,
    cipher: Cipher,
    /// The nonce for the next sealed packet; `None` once `u32::MAX` was used.
    next_nonce: Option<u32>,
}

impl fmt::Debug for TransportCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportCipher")
            .field("mode", &self.mode)
            .field("next_nonce", &self.next_nonce)
            .finish_non_exhaustive()
    }
}

impl TransportCipher {
    pub fn new(mode: TransportMode, key: &TransportKey) -> Self {
        Self::starting_at(mode, key, 0)
    }

    /// A cipher whose next sealed packet uses `nonce`, for exercising the end
    /// of the nonce space.
    pub fn starting_at(mode: TransportMode, key: &TransportKey, nonce: u32) -> Self {
        let key = Array::from(*key.0);
        let cipher = match mode {
            TransportMode::Aes256GcmRtpSize => Cipher::Aes(Box::new(Aes256Gcm::new(&key))),
            TransportMode::XChaCha20Poly1305RtpSize => {
                Cipher::XChaCha(Box::new(XChaCha20Poly1305::new(&key)))
            }
        };
        Self {
            mode,
            cipher,
            next_nonce: Some(nonce),
        }
    }

    pub const fn mode(&self) -> TransportMode {
        self.mode
    }

    /// Nonces left for sealing, including the next one.
    pub fn remaining_nonces(&self) -> u64 {
        self.next_nonce
            .map_or(0, |next| u64::from(u32::MAX - next) + 1)
    }

    /// Writes `clear || encrypt(body) || tag || nonce` into `out` (cleared
    /// first; its capacity is reused). Consumes one nonce.
    pub fn seal(
        &mut self,
        clear: &[u8],
        body: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        let nonce = self.next_nonce.ok_or(CryptoError::NonceExhausted)?;
        let Some(packet_len) = clear
            .len()
            .checked_add(body.len())
            .and_then(|len| len.checked_add(OVERHEAD))
        else {
            return Err(CryptoError::Truncated);
        };
        out.clear();
        out.reserve(packet_len);
        out.extend_from_slice(clear);
        out.extend_from_slice(body);
        let (aad, buffer) = out.split_at_mut(clear.len());
        let tag: [u8; TAG_LEN] = match &self.cipher {
            Cipher::Aes(cipher) => cipher
                .encrypt_inout_detached(&Array(aes_nonce(nonce)), aad, buffer.into())
                .map(Into::into),
            Cipher::XChaCha(cipher) => cipher
                .encrypt_inout_detached(&Array(xchacha_nonce(nonce)), aad, buffer.into())
                .map(Into::into),
        }
        // Only oversized inputs fail, far beyond any UDP datagram.
        .map_err(|_| CryptoError::Truncated)?;
        out.extend_from_slice(&tag);
        out.extend_from_slice(&nonce.to_be_bytes());
        self.next_nonce = nonce.checked_add(1);
        Ok(())
    }

    /// Verifies and decrypts `packet` whose first `clear_len` bytes are the
    /// clear header. Writes the plaintext body (without header, tag, or nonce)
    /// into `out` and returns the packet's nonce.
    pub fn open(
        &self,
        packet: &[u8],
        clear_len: usize,
        out: &mut Vec<u8>,
    ) -> Result<u32, CryptoError> {
        let Some(min_len) = clear_len.checked_add(OVERHEAD) else {
            return Err(CryptoError::Truncated);
        };
        let Some(body_len) = packet.len().checked_sub(min_len) else {
            return Err(CryptoError::Truncated);
        };
        let (clear, rest) = packet.split_at(clear_len);
        let (ciphertext, rest) = rest.split_at(body_len);
        let (tag, suffix) = rest.split_at(TAG_LEN);
        let nonce = u32::from_be_bytes([suffix[0], suffix[1], suffix[2], suffix[3]]);
        let tag: [u8; TAG_LEN] = tag.try_into().map_err(|_| CryptoError::Truncated)?;
        out.clear();
        out.extend_from_slice(ciphertext);
        let result = match &self.cipher {
            Cipher::Aes(cipher) => cipher.decrypt_inout_detached(
                &Array(aes_nonce(nonce)),
                clear,
                out.as_mut_slice().into(),
                &Array(tag),
            ),
            Cipher::XChaCha(cipher) => cipher.decrypt_inout_detached(
                &Array(xchacha_nonce(nonce)),
                clear,
                out.as_mut_slice().into(),
                &Array(tag),
            ),
        };
        if result.is_err() {
            out.clear();
            return Err(CryptoError::Authentication);
        }
        Ok(nonce)
    }
}

fn aes_nonce(counter: u32) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[..4].copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn xchacha_nonce(counter: u32) -> [u8; 24] {
    let mut nonce = [0; 24];
    nonce[..4].copy_from_slice(&counter.to_be_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtcp::Compound;
    use crate::rtp::RtpHeader;

    const VECTORS: &str = include_str!("../../../fixtures/voice/transport-vectors.json");

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    struct Vector {
        name: String,
        kind: String,
        nonce: u32,
        clear: Vec<u8>,
        plaintext: Vec<u8>,
        packet: Vec<u8>,
    }

    fn vectors() -> Vec<(TransportMode, TransportKey, Vec<Vector>)> {
        let root: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
        root["modes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|mode| {
                let vectors = mode["vectors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| Vector {
                        name: v["name"].as_str().unwrap().to_owned(),
                        kind: v["kind"].as_str().unwrap().to_owned(),
                        nonce: u32::try_from(v["nonce"].as_u64().unwrap()).unwrap(),
                        clear: hex(v["clear"].as_str().unwrap()),
                        plaintext: hex(v["plaintext"].as_str().unwrap()),
                        packet: hex(v["packet"].as_str().unwrap()),
                    })
                    .collect();
                (
                    TransportMode::from_wire(mode["mode"].as_str().unwrap()).unwrap(),
                    TransportKey::from_slice(&hex(mode["key"].as_str().unwrap())).unwrap(),
                    vectors,
                )
            })
            .collect()
    }

    #[test]
    fn both_modes_match_the_independent_vectors() {
        let all = vectors();
        assert_eq!(
            all.iter().map(|(mode, _, _)| *mode).collect::<Vec<_>>(),
            [
                TransportMode::Aes256GcmRtpSize,
                TransportMode::XChaCha20Poly1305RtpSize
            ]
        );
        for (mode, key, vectors) in all {
            assert_eq!(vectors.len(), 4);
            for vector in vectors {
                let mut cipher = TransportCipher::starting_at(mode, &key, vector.nonce);
                let mut sealed = Vec::new();
                cipher
                    .seal(&vector.clear, &vector.plaintext, &mut sealed)
                    .unwrap();
                assert_eq!(sealed, vector.packet, "{mode:?} {}", vector.name);

                // Receive side derives the clear length from the packet itself.
                let clear_len = match vector.kind.as_str() {
                    "rtp" => RtpHeader::parse(&vector.packet).unwrap().clear_len(),
                    _ => RTCP_CLEAR_LEN,
                };
                assert_eq!(clear_len, vector.clear.len(), "{}", vector.name);
                let mut opened = Vec::new();
                let nonce = cipher.open(&vector.packet, clear_len, &mut opened).unwrap();
                assert_eq!(nonce, vector.nonce);
                assert_eq!(opened, vector.plaintext, "{mode:?} {}", vector.name);
            }
        }
    }

    #[test]
    fn decrypted_vectors_parse_as_the_expected_rtp_and_rtcp() {
        let (_, key, vectors) = vectors().remove(1);
        let cipher = TransportCipher::new(TransportMode::XChaCha20Poly1305RtpSize, &key);
        let mut body = Vec::new();
        let extension = vectors
            .iter()
            .find(|v| v.name == "rtp-csrc-extension")
            .unwrap();
        let header = RtpHeader::parse(&extension.packet).unwrap();
        cipher
            .open(&extension.packet, header.clear_len(), &mut body)
            .unwrap();
        let split = header.split_body(&body).unwrap();
        assert_eq!(split.extensions.find(1), Some(&[0x85][..]));
        assert_eq!(split.payload, (1..=20).collect::<Vec<u8>>());

        let padded = vectors
            .iter()
            .find(|v| v.name == "rtp-padding-last-nonce")
            .unwrap();
        let header = RtpHeader::parse(&padded.packet).unwrap();
        cipher
            .open(&padded.packet, header.clear_len(), &mut body)
            .unwrap();
        assert_eq!(header.split_body(&body).unwrap().payload, [1, 2, 3, 4, 5]);

        let rtcp = vectors.iter().find(|v| v.kind == "rtcp").unwrap();
        cipher
            .open(&rtcp.packet, RTCP_CLEAR_LEN, &mut body)
            .unwrap();
        let mut plain = rtcp.packet[..RTCP_CLEAR_LEN].to_vec();
        plain.extend_from_slice(&body);
        let packets: Vec<_> = Compound::new(&plain).collect::<Result<_, _>>().unwrap();
        assert_eq!(packets.len(), 1);
    }

    #[test]
    fn tampering_anywhere_fails_authentication() {
        for (mode, key, vectors) in vectors() {
            let cipher = TransportCipher::new(mode, &key);
            for vector in vectors {
                let clear_len = vector.clear.len();
                let mut out = Vec::new();
                for index in 0..vector.packet.len() {
                    let mut tampered = vector.packet.clone();
                    tampered[index] ^= 0x01;
                    assert_eq!(
                        cipher.open(&tampered, clear_len, &mut out),
                        Err(CryptoError::Authentication),
                        "{mode:?} {} byte {index}",
                        vector.name
                    );
                    assert!(out.is_empty());
                }
            }
        }
    }

    #[test]
    fn wrong_key_and_short_packets_are_rejected() {
        let key = TransportKey::from_slice(&[7; 32]).unwrap();
        let other = TransportKey::from_slice(&[8; 32]).unwrap();
        for mode in [
            TransportMode::Aes256GcmRtpSize,
            TransportMode::XChaCha20Poly1305RtpSize,
        ] {
            let mut sender = TransportCipher::new(mode, &key);
            let receiver = TransportCipher::new(mode, &other);
            let mut packet = Vec::new();
            sender.seal(&[0x80; 12], b"opus", &mut packet).unwrap();
            let mut out = Vec::new();
            assert_eq!(
                receiver.open(&packet, 12, &mut out),
                Err(CryptoError::Authentication)
            );
            for len in 0..12 + OVERHEAD {
                assert_eq!(
                    receiver.open(&packet[..len], 12, &mut out),
                    Err(CryptoError::Truncated)
                );
            }
            assert_eq!(
                receiver.open(&packet, usize::MAX, &mut out),
                Err(CryptoError::Truncated)
            );
            // An empty body is valid.
            sender.seal(&[0x80; 12], b"", &mut packet).unwrap();
            assert_eq!(packet.len(), 12 + OVERHEAD);
            assert_eq!(
                TransportCipher::new(mode, &key).open(&packet, 12, &mut out),
                Ok(1)
            );
        }
        assert_eq!(
            TransportKey::from_slice(&[0; 31]).err(),
            Some(CryptoError::KeyLength)
        );
        assert_eq!(
            TransportKey::from_slice(&[0; 33]).err(),
            Some(CryptoError::KeyLength)
        );
    }

    #[test]
    fn nonces_increase_and_are_never_reused_after_the_last_one() {
        let key = TransportKey::from_slice(&[1; 32]).unwrap();
        let mut cipher = TransportCipher::new(TransportMode::Aes256GcmRtpSize, &key);
        let mut packet = Vec::new();
        let mut seen = Vec::new();
        for _ in 0..3 {
            cipher.seal(&[0x80; 12], b"x", &mut packet).unwrap();
            seen.push(packet[packet.len() - 4..].to_vec());
        }
        assert_eq!(seen, [[0, 0, 0, 0], [0, 0, 0, 1], [0, 0, 0, 2]]);

        let mut cipher = TransportCipher::starting_at(
            TransportMode::XChaCha20Poly1305RtpSize,
            &key,
            u32::MAX - 1,
        );
        assert_eq!(cipher.remaining_nonces(), 2);
        cipher.seal(&[0x80; 12], b"x", &mut packet).unwrap();
        cipher.seal(&[0x80; 12], b"x", &mut packet).unwrap();
        assert_eq!(&packet[packet.len() - 4..], [0xFF; 4]);
        assert_eq!(cipher.remaining_nonces(), 0);
        // The counter does not wrap to 0: the key is finished.
        let before = packet.clone();
        assert_eq!(
            cipher.seal(&[0x80; 12], b"x", &mut packet),
            Err(CryptoError::NonceExhausted)
        );
        assert_eq!(packet, before);
        assert_eq!(
            cipher.seal(&[0x80; 12], b"x", &mut packet),
            Err(CryptoError::NonceExhausted)
        );
    }

    #[test]
    fn mode_selection_prefers_accelerated_aes_and_requires_a_supported_mode() {
        let both = [
            "xsalsa20_poly1305_lite_rtpsize",
            "aead_aes256_gcm_rtpsize",
            "aead_xchacha20_poly1305_rtpsize",
        ];
        assert_eq!(
            TransportMode::select(both, true),
            Some(TransportMode::Aes256GcmRtpSize)
        );
        assert_eq!(
            TransportMode::select(both, false),
            Some(TransportMode::XChaCha20Poly1305RtpSize)
        );
        assert_eq!(
            TransportMode::select(["aead_xchacha20_poly1305_rtpsize"], true),
            Some(TransportMode::XChaCha20Poly1305RtpSize)
        );
        assert_eq!(
            TransportMode::select(["aead_aes256_gcm_rtpsize"], false),
            Some(TransportMode::Aes256GcmRtpSize)
        );
        assert_eq!(
            TransportMode::select(["xsalsa20_poly1305", "aead_aes256_gcm"], true),
            None
        );
        for mode in [
            TransportMode::Aes256GcmRtpSize,
            TransportMode::XChaCha20Poly1305RtpSize,
        ] {
            assert_eq!(TransportMode::from_wire(mode.wire_name()), Some(mode));
        }
    }

    #[test]
    fn keys_never_print() {
        let key = TransportKey::from_slice(&[0xAB; 32]).unwrap();
        let cipher = TransportCipher::new(TransportMode::Aes256GcmRtpSize, &key);
        for text in [format!("{key:?}"), format!("{cipher:?}")] {
            assert!(
                !text.to_lowercase().contains("ab, ") && !text.contains("171"),
                "{text}"
            );
        }
    }
}
