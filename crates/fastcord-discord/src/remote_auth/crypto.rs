//! Per-attempt RSA material for the remote-auth handshake.
//!
//! The private key exists only inside [`AttemptKey`]; `rsa` zeroizes it on drop.
//! Nothing here implements `Debug` for plaintexts, and decrypted buffers are
//! wrapped in [`Zeroizing`] before they leave this module.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rand_core::OsRng;
use rsa::pkcs8::EncodePublicKey;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::RemoteAuthError;

const KEY_BITS: usize = 2048;
/// RSA-2048 ciphertexts are exactly 256 bytes; reject larger base64 up front.
const MAX_CIPHERTEXT_BASE64_BYTES: usize = 1024;

pub(crate) struct AttemptKey {
    private: RsaPrivateKey,
    spki: Vec<u8>,
}

impl fmt::Debug for AttemptKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AttemptKey([REDACTED])")
    }
}

impl AttemptKey {
    /// CPU-heavy (hundreds of milliseconds in release builds); run it on a
    /// blocking worker, never on the UI thread or a reactor thread.
    pub(crate) fn generate() -> Result<Self, RemoteAuthError> {
        let private =
            RsaPrivateKey::new(&mut OsRng, KEY_BITS).map_err(|_| RemoteAuthError::KeyGeneration)?;
        let spki = RsaPublicKey::from(&private)
            .to_public_key_der()
            .map_err(|_| RemoteAuthError::KeyGeneration)?
            .into_vec();
        Ok(Self { private, spki })
    }

    /// Standard base64 of the SubjectPublicKeyInfo DER, as `init` requires.
    pub(crate) fn encoded_public_key(&self) -> String {
        STANDARD.encode(&self.spki)
    }

    /// Base64url (unpadded) SHA-256 of the SubjectPublicKeyInfo DER.
    pub(crate) fn fingerprint(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(&self.spki))
    }

    /// Decrypts a base64 RSA-OAEP(SHA-256, MGF1-SHA-256) message, using blinding.
    pub(crate) fn decrypt(&self, encoded: &str) -> Result<Zeroizing<Vec<u8>>, RemoteAuthError> {
        if encoded.len() > MAX_CIPHERTEXT_BASE64_BYTES {
            return Err(RemoteAuthError::Crypto);
        }
        let ciphertext = STANDARD
            .decode(encoded)
            .map_err(|_| RemoteAuthError::Crypto)?;
        self.private
            .decrypt_blinded(&mut OsRng, Oaep::new::<Sha256>(), &ciphertext)
            .map(Zeroizing::new)
            .map_err(|_| RemoteAuthError::Crypto)
    }

    /// The nonce proof is the decrypted nonce, base64url without padding.
    pub(crate) fn nonce_proof(
        &self,
        encrypted_nonce: &str,
    ) -> Result<Zeroizing<String>, RemoteAuthError> {
        let nonce = self.decrypt(encrypted_nonce)?;
        Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(&*nonce)))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use rsa::pkcs8::DecodePublicKey;

    /// Plays Discord's role: encrypts to the public key received in `init`.
    pub(crate) fn encrypt_for(encoded_public_key: &str, plaintext: &[u8]) -> String {
        let spki = STANDARD.decode(encoded_public_key).unwrap();
        let public = RsaPublicKey::from_public_key_der(&spki).unwrap();
        let ciphertext = public
            .encrypt(&mut OsRng, Oaep::new::<Sha256>(), plaintext)
            .unwrap();
        STANDARD.encode(ciphertext)
    }

    pub(crate) fn fingerprint_of(encoded_public_key: &str) -> String {
        let spki = STANDARD.decode(encoded_public_key).unwrap();
        URL_SAFE_NO_PAD.encode(Sha256::digest(spki))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{encrypt_for, fingerprint_of};
    use super::*;

    #[test]
    fn key_is_2048_bit_spki_and_fingerprint_matches_its_sha256() {
        let key = AttemptKey::generate().unwrap();
        let encoded = key.encoded_public_key();
        let spki = STANDARD.decode(&encoded).unwrap();
        // A 2048-bit RSA SubjectPublicKeyInfo is 294 bytes: 30 82 01 22 ...
        assert_eq!(spki.len(), 294);
        assert_eq!(&spki[..4], &[0x30, 0x82, 0x01, 0x22]);
        assert_eq!(key.fingerprint(), fingerprint_of(&encoded));
        assert_eq!(key.fingerprint().len(), 43);
        assert!(!key.fingerprint().contains(['+', '/', '=']));
        assert_eq!(format!("{key:?}"), "AttemptKey([REDACTED])");
    }

    #[test]
    fn keys_are_unique_per_attempt() {
        let first = AttemptKey::generate().unwrap();
        let second = AttemptKey::generate().unwrap();
        assert_ne!(first.encoded_public_key(), second.encoded_public_key());
        assert_ne!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn oaep_round_trip_and_nonce_proof_encoding() {
        let key = AttemptKey::generate().unwrap();
        // Bytes that differ between standard and url-safe base64 (`+`/`/`).
        let nonce = [0xfb, 0xff, 0xbe, 0x01, 0x02, 0x03];
        let encrypted = encrypt_for(&key.encoded_public_key(), &nonce);
        assert_eq!(&**key.decrypt(&encrypted).unwrap(), &nonce);
        assert_eq!(&**key.nonce_proof(&encrypted).unwrap(), "-_--AQID");
    }

    #[test]
    fn malformed_wrong_key_and_oversized_ciphertexts_are_rejected() {
        let key = AttemptKey::generate().unwrap();
        let other = AttemptKey::generate().unwrap();
        assert_eq!(
            key.decrypt("not base64!").unwrap_err(),
            RemoteAuthError::Crypto
        );
        assert_eq!(key.decrypt("AAAA").unwrap_err(), RemoteAuthError::Crypto);
        assert_eq!(
            key.decrypt(&"A".repeat(MAX_CIPHERTEXT_BASE64_BYTES + 4))
                .unwrap_err(),
            RemoteAuthError::Crypto
        );
        let for_other = encrypt_for(&other.encoded_public_key(), b"secret");
        assert_eq!(
            key.decrypt(&for_other).unwrap_err(),
            RemoteAuthError::Crypto
        );
    }
}
