//! WinZip AES-256 (AE-2) encryption for ZIP entries.
//!
//! Layout of an encrypted entry's data: salt(16) | verifier(2) | ciphertext | HMAC-SHA1(10).
//! The cipher is AES-CTR with a little-endian counter starting at 1.

use aes::Aes256;
use ctr::Ctr128LE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

pub const SALT_LEN: usize = 16;
pub const HEADER_LEN: usize = SALT_LEN + 2;
pub const TRAILER_LEN: usize = 10;
pub const OVERHEAD: u64 = (HEADER_LEN + TRAILER_LEN) as u64;

pub struct AesEncryptor {
    cipher: Ctr128LE<Aes256>,
    mac: Hmac<Sha1>,
    header: [u8; HEADER_LEN],
}

impl AesEncryptor {
    pub fn new(password: &[u8]) -> Self {
        let mut salt = [0u8; SALT_LEN];
        getrandom::fill(&mut salt).expect("system RNG");
        let mut dk = [0u8; 66];
        pbkdf2::pbkdf2_hmac::<Sha1>(password, &salt, 1000, &mut dk);
        let mut iv = [0u8; 16];
        iv[0] = 1;
        let cipher = Ctr128LE::<Aes256>::new(dk[..32].try_into().unwrap(), &iv.into());
        let mac = <Hmac<Sha1> as KeyInit>::new_from_slice(&dk[32..64]).expect("any key length");
        let mut header = [0u8; HEADER_LEN];
        header[..SALT_LEN].copy_from_slice(&salt);
        header[SALT_LEN..].copy_from_slice(&dk[64..66]);
        Self { cipher, mac, header }
    }

    pub fn header(&self) -> &[u8; HEADER_LEN] {
        &self.header
    }

    pub fn encrypt(&mut self, buf: &mut [u8]) {
        self.cipher.apply_keystream(buf);
        self.mac.update(buf);
    }

    pub fn finish(self) -> [u8; TRAILER_LEN] {
        let tag = self.mac.finalize().into_bytes();
        tag[..TRAILER_LEN].try_into().unwrap()
    }

    /// Encrypt a complete entry in one go, returning header + ciphertext + MAC.
    pub fn seal(password: &[u8], mut data: Vec<u8>) -> Vec<u8> {
        let mut enc = Self::new(password);
        enc.encrypt(&mut data);
        let mut out = Vec::with_capacity(data.len() + OVERHEAD as usize);
        out.extend_from_slice(enc.header());
        out.extend_from_slice(&data);
        out.extend_from_slice(&enc.finish());
        out
    }
}
