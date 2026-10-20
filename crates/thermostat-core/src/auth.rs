//! Password hashing, WPA key derivation and web sessions.

use hmac::Hmac;
use sha1::Sha1;
use sha2::Sha256;

/// PBKDF2 rounds for new admin password hashes. Takes well under a second on
/// an ESP32 at 240 MHz and is stored per hash, so it can be raised later.
pub const DEFAULT_ITERATIONS: u32 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordHash {
    pub salt: [u8; 16],
    pub iterations: u32,
    pub hash: [u8; 32],
}

impl PasswordHash {
    pub const ENCODED_LEN: usize = 16 + 4 + 32;

    /// Hash `password` (PBKDF2-HMAC-SHA256) with a caller-provided random salt.
    pub fn new(password: &str, salt: [u8; 16], iterations: u32) -> Self {
        let mut hash = [0u8; 32];
        pbkdf2::pbkdf2::<Hmac<Sha256>>(password.as_bytes(), &salt, iterations, &mut hash)
            .expect("HMAC accepts any key length");
        Self {
            salt,
            iterations,
            hash,
        }
    }

    pub fn verify(&self, password: &str) -> bool {
        let candidate = Self::new(password, self.salt, self.iterations);
        constant_time_eq(&candidate.hash, &self.hash)
    }

    pub fn to_bytes(&self) -> [u8; Self::ENCODED_LEN] {
        let mut b = [0u8; Self::ENCODED_LEN];
        b[..16].copy_from_slice(&self.salt);
        b[16..20].copy_from_slice(&self.iterations.to_le_bytes());
        b[20..].copy_from_slice(&self.hash);
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != Self::ENCODED_LEN {
            return None;
        }
        Some(Self {
            salt: b[..16].try_into().ok()?,
            iterations: u32::from_le_bytes(b[16..20].try_into().ok()?),
            hash: b[20..].try_into().ok()?,
        })
    }
}

/// WPA2-Personal PSK: PBKDF2-HMAC-SHA1(passphrase, ssid, 4096, 32).
pub fn wpa_psk(passphrase: &str, ssid: &str) -> [u8; 32] {
    let mut psk = [0u8; 32];
    pbkdf2::pbkdf2::<Hmac<Sha1>>(passphrase.as_bytes(), ssid.as_bytes(), 4096, &mut psk)
        .expect("HMAC accepts any key length");
    psk
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub const TOKEN_LEN: usize = 16;
const MAX_SESSIONS: usize = 4;
pub const SESSION_TTL_MS: u64 = 60 * 60 * 1000;

/// In-memory login sessions. Rebooting logs everyone out.
#[derive(Default)]
pub struct Sessions {
    slots: [Option<([u8; TOKEN_LEN], u64)>; MAX_SESSIONS],
}

impl Sessions {
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_SESSIONS],
        }
    }

    /// Register a fresh random token; evicts the oldest session if full.
    pub fn create(&mut self, token: [u8; TOKEN_LEN], now_ms: u64) {
        let slot = self
            .slots
            .iter()
            .position(|s| s.is_none_or(|(_, exp)| exp <= now_ms))
            .unwrap_or_else(|| {
                (0..MAX_SESSIONS)
                    .min_by_key(|&i| self.slots[i].map_or(0, |(_, exp)| exp))
                    .unwrap()
            });
        self.slots[slot] = Some((token, now_ms + SESSION_TTL_MS));
    }

    /// Check a token; a valid session has its expiry extended.
    pub fn validate(&mut self, token: &[u8], now_ms: u64) -> bool {
        for slot in self.slots.iter_mut() {
            if let Some((t, exp)) = slot
                && *exp > now_ms
                && constant_time_eq(t, token)
            {
                *exp = now_ms + SESSION_TTL_MS;
                return true;
            }
        }
        false
    }

    pub fn remove(&mut self, token: &[u8]) {
        for slot in self.slots.iter_mut() {
            if slot.is_some_and(|(t, _)| constant_time_eq(&t, token)) {
                *slot = None;
            }
        }
    }

    pub fn clear(&mut self) {
        self.slots = [None; MAX_SESSIONS];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_verify() {
        let h = PasswordHash::new("hunter2", [3; 16], 100);
        assert!(h.verify("hunter2"));
        assert!(!h.verify("hunter3"));
        assert_eq!(PasswordHash::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn wpa_psk_test_vector() {
        // IEEE 802.11i-2004, Annex H.4.
        let psk = wpa_psk("password", "IEEE");
        assert_eq!(
            crate::hex::encode::<64>(&psk).as_str(),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
    }

    #[test]
    fn sessions_expire_and_evict() {
        let mut s = Sessions::new();
        s.create([1; 16], 0);
        assert!(s.validate(&[1; 16], 1000));
        assert!(!s.validate(&[2; 16], 1000));
        assert!(!s.validate(&[1; 16], 1000 + SESSION_TTL_MS + 1));
        for i in 0..5u8 {
            s.create([10 + i; 16], 10 * i as u64);
        }
        assert!(!s.validate(&[10; 16], 100), "oldest evicted");
        assert!(s.validate(&[14; 16], 100));
        s.remove(&[14; 16]);
        assert!(!s.validate(&[14; 16], 100));
    }
}
