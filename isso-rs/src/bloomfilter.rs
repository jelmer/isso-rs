//! Bloomfilter used to track voters per comment.
//!
//! Must produce byte-identical arrays to isso/utils/__init__.py::Bloomfilter
//! so that DBs written by the Python server (and vice versa) remain valid.
//!
//! Layout: 256 bytes (2048 bits), 11 probes per key, each probe derived from a
//! 256-bit SHA-256 digest via successive 11-bit shifts masked to the bit array size.

use sha2::{Digest, Sha256};

pub const ARRAY_LEN: usize = 256;
pub const BITS: usize = ARRAY_LEN * 8;
pub const MASK: usize = BITS - 1;
pub const K: usize = 11;

#[derive(Debug, Clone)]
pub struct Bloomfilter {
    pub array: [u8; ARRAY_LEN],
    pub elements: u32,
}

impl Default for Bloomfilter {
    fn default() -> Self {
        Self::new()
    }
}

impl Bloomfilter {
    pub fn new() -> Self {
        Self {
            array: [0u8; ARRAY_LEN],
            elements: 0,
        }
    }

    pub fn from_bytes(bytes: &[u8], elements: u32) -> Self {
        let mut array = [0u8; ARRAY_LEN];
        let n = bytes.len().min(ARRAY_LEN);
        array[..n].copy_from_slice(&bytes[..n]);
        Self { array, elements }
    }

    pub fn add(&mut self, key: &str) {
        for i in probes(key) {
            self.array[i / 8] |= 1 << (i % 8);
        }
        self.elements = self.elements.saturating_add(1);
    }

    pub fn contains(&self, key: &str) -> bool {
        probes(key).all(|i| self.array[i / 8] & (1 << (i % 8)) != 0)
    }
}

/// Reproduce Python's `get_probes`:
///
/// ```python
/// h = int(hashlib.sha256(key.encode()).hexdigest(), 16)
/// for _ in range(self.k):
///     yield h & self.m - 1
///     h >>= self.k
/// ```
///
/// The digest is interpreted as a **big-endian** 256-bit integer — `int(hex, 16)`
/// takes the most significant hex digits first. We mimic that by walking the
/// 32-byte digest from the low-order end with 11-bit windows.
fn probes(key: &str) -> impl Iterator<Item = usize> {
    let digest = Sha256::digest(key.as_bytes());
    // Reverse so index 0 is the least-significant byte; now a running window
    // "from the bottom" matches Python's repeated `h >>= 11`.
    let mut le: [u8; 32] = digest.into();
    le.reverse();

    (0..K).map(move |step| {
        let bit_offset = step * K;
        let byte_idx = bit_offset / 8;
        let bit_in_byte = bit_offset % 8;
        // Read up to 3 bytes (K=11 < 24) starting at byte_idx and pull out 11 bits.
        let b0 = le[byte_idx] as u32;
        let b1 = if byte_idx + 1 < 32 {
            le[byte_idx + 1] as u32
        } else {
            0
        };
        let b2 = if byte_idx + 2 < 32 {
            le[byte_idx + 2] as u32
        } else {
            0
        };
        let window = b0 | (b1 << 8) | (b2 << 16);
        ((window >> bit_in_byte) & (MASK as u32)) as usize
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_contains_after_add() {
        let mut bf = Bloomfilter::new();
        bf.add("127.0.0.1");
        assert!(bf.contains("127.0.0.1"));
    }

    #[test]
    fn unrelated_ips_dont_collide_at_random() {
        let mut bf = Bloomfilter::new();
        bf.add("127.0.0.1");
        // One of these might false-positive in principle, but for k=11/m=2048
        // this sample should be clean.
        assert!(!bf.contains("10.0.0.1"));
        assert!(!bf.contains("8.8.8.8"));
    }

    #[test]
    fn high_load_does_not_panic() {
        let mut bf = Bloomfilter::new();
        for i in 0..256u32 {
            bf.add(&format!("1.2.{i}.4"));
        }
        assert!(bf.contains("1.2.3.4"));
    }

    /// Known-answer tests: probe values captured from a live Python 3
    /// run of `Bloomfilter.get_probes`. If these drift, wire-compat with
    /// Python-written DBs is broken.
    #[test]
    fn probes_match_python_for_known_keys() {
        // python3 -c '
        //   import hashlib
        //   def probes(key):
        //       h = int(hashlib.sha256(key.encode()).hexdigest(), 16)
        //       out = []
        //       for _ in range(11):
        //           out.append(h & 2047); h >>= 11
        //       return out
        // '
        for (key, expected) in [
            (
                "127.0.0.1",
                [416, 14, 1013, 212, 896, 1670, 1177, 260, 1646, 932, 329],
            ),
            (
                "8.8.8.8",
                [245, 2018, 1381, 1446, 1363, 623, 272, 1003, 748, 1361, 865],
            ),
            (
                "hello world",
                [1513, 1529, 907, 982, 143, 1313, 59, 668, 890, 1532, 531],
            ),
        ] {
            let got: Vec<usize> = probes(key).collect();
            assert_eq!(got, expected.to_vec(), "probe mismatch for key {key:?}");
        }
    }
}
