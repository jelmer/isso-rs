//! Hash factory matching isso/utils/hash.py.
//!
//! The public API (`Hasher::uhash(s)`) returns a lowercase hex string, and is
//! applied to `email or remote_addr` to populate the "hash" field on comment
//! JSON. We implement pbkdf2, sha1, md5, and "none" — same algorithms Python
//! supports.

use hmac::Hmac;
use md5::Md5;
use pbkdf2::pbkdf2;
use sha1::Sha1;
use sha2::{Sha224, Sha256, Sha384, Sha512};

type HmacSha1 = Hmac<Sha1>;
type HmacMd5 = Hmac<Md5>;
type HmacSha224 = Hmac<Sha224>;
type HmacSha256 = Hmac<Sha256>;
type HmacSha384 = Hmac<Sha384>;
type HmacSha512 = Hmac<Sha512>;

#[derive(Debug, Clone)]
pub enum Hasher {
    None,
    Sha1,
    Md5,
    Pbkdf2(Pbkdf2Params),
}

#[derive(Debug, Clone)]
pub struct Pbkdf2Params {
    pub salt: Vec<u8>,
    pub iterations: u32,
    pub dklen: usize,
    pub func: String,
}

impl Hasher {
    /// Parse the same `algorithm` + `salt` pair that isso/utils/hash.py::new accepts.
    ///
    /// Recognised:
    /// - `"none"` → no hashing (returns the input)
    /// - `"sha1"` / `"md5"` → unsalted digest
    /// - `"pbkdf2[:iterations[:dklen[:func]]]"` → PBKDF2-HMAC
    pub fn from_config(algorithm: &str, salt: &str) -> anyhow::Result<Self> {
        if algorithm == "none" {
            return Ok(Hasher::None);
        }
        if algorithm == "sha1" {
            return Ok(Hasher::Sha1);
        }
        if algorithm == "md5" {
            return Ok(Hasher::Md5);
        }
        if let Some(tail) = algorithm.strip_prefix("pbkdf2") {
            let mut parts = tail.split(':');
            let _ = parts.next(); // consume empty head after prefix strip
            let iterations: u32 = match parts.next() {
                Some(v) if !v.is_empty() => v.parse()?,
                _ => 1000,
            };
            let dklen: usize = match parts.next() {
                Some(v) if !v.is_empty() => v.parse()?,
                _ => 6,
            };
            let func: String = match parts.next() {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => "sha1".to_string(),
            };
            if !matches!(
                func.as_str(),
                "sha1" | "md5" | "sha224" | "sha256" | "sha384" | "sha512"
            ) {
                // Python accepts any hashlib name; we wire the digests hashlib
                // actually ships (sha1/sha224/sha256/sha384/sha512 + md5).
                anyhow::bail!("pbkdf2 with func={func} not implemented");
            }
            return Ok(Hasher::Pbkdf2(Pbkdf2Params {
                salt: salt.as_bytes().to_vec(),
                iterations,
                dklen,
                func,
            }));
        }
        anyhow::bail!("unknown hash algorithm: {algorithm}")
    }

    /// Hash a UTF-8 string, returning a lowercase hex string.
    pub fn uhash(&self, val: &str) -> String {
        let bytes = val.as_bytes();
        let out = match self {
            Hasher::None => bytes.to_vec(),
            Hasher::Sha1 => {
                use sha1::Digest;
                sha1::Sha1::digest(bytes).to_vec()
            }
            Hasher::Md5 => {
                use md5::Digest as _;
                Md5::digest(bytes).to_vec()
            }
            Hasher::Pbkdf2(p) => {
                let mut out = vec![0u8; p.dklen];
                let r = match p.func.as_str() {
                    "md5" => pbkdf2::<HmacMd5>(bytes, &p.salt, p.iterations, &mut out),
                    "sha224" => pbkdf2::<HmacSha224>(bytes, &p.salt, p.iterations, &mut out),
                    "sha256" => pbkdf2::<HmacSha256>(bytes, &p.salt, p.iterations, &mut out),
                    "sha384" => pbkdf2::<HmacSha384>(bytes, &p.salt, p.iterations, &mut out),
                    "sha512" => pbkdf2::<HmacSha512>(bytes, &p.salt, p.iterations, &mut out),
                    // Default + explicit "sha1": HMAC-SHA1 (Python's default).
                    _ => pbkdf2::<HmacSha1>(bytes, &p.salt, p.iterations, &mut out),
                };
                r.expect("pbkdf2 output length is valid");
                out
            }
        };
        hex::encode(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values captured from a live Python 3 session:
    ///
    /// ```text
    /// python3 -c "
    ///   import hashlib
    ///   print(hashlib.pbkdf2_hmac('sha1', b'jane@example.com', b'Eech7co8Ohloopo9Ol6baimi', 1000, 6).hex())
    /// "
    /// 2a70e20083cc
    /// ```
    #[test]
    fn default_pbkdf2_matches_python() {
        let h = Hasher::from_config("pbkdf2", "Eech7co8Ohloopo9Ol6baimi").unwrap();
        assert_eq!(h.uhash("jane@example.com"), "2a70e20083cc");
        assert_eq!(h.uhash("192.168.1.0"), "9f0076fd038d");
        assert_eq!(h.uhash(""), "42476aafe2e4");
    }

    #[test]
    fn pbkdf2_length_follows_dklen() {
        let h = Hasher::from_config("pbkdf2:1000:6:sha1", "salt").unwrap();
        assert_eq!(h.uhash("x").len(), 12); // 6 bytes -> 12 hex chars
    }

    #[test]
    fn unsalted_sha1_matches_python_hashlib() {
        let h = Hasher::from_config("sha1", "unused").unwrap();
        use sha1::Digest;
        let expected = hex::encode(sha1::Sha1::digest(b"hello"));
        assert_eq!(h.uhash("hello"), expected);
    }

    #[test]
    fn none_returns_input_hex() {
        let h = Hasher::from_config("none", "").unwrap();
        assert_eq!(h.uhash("abc"), hex::encode(b"abc"));
    }

    #[test]
    fn md5_matches_python_hashlib() {
        // python3 -c 'import hashlib; print(hashlib.md5(b"hello").hexdigest())'
        let h = Hasher::from_config("md5", "unused").unwrap();
        assert_eq!(h.uhash("hello"), "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn pbkdf2_with_md5_matches_python() {
        // python3 -c 'import hashlib;
        //   print(hashlib.pbkdf2_hmac("md5", b"hello", b"salt", 16, 2).hex())'
        // -> 3551
        let h = Hasher::from_config("pbkdf2:16:2:md5", "salt").unwrap();
        assert_eq!(h.uhash("hello"), "3551");
    }

    #[test]
    fn pbkdf2_with_sha256_matches_python() {
        // python3 -c 'import hashlib;
        //   print(hashlib.pbkdf2_hmac("sha256", b"hello", b"salt", 16, 8).hex())'
        let h = Hasher::from_config("pbkdf2:16:8:sha256", "salt").unwrap();
        assert_eq!(h.uhash("hello"), "e5666033fc0f2ee8");
    }

    #[test]
    fn pbkdf2_with_sha512_matches_python() {
        // python3 -c 'import hashlib;
        //   print(hashlib.pbkdf2_hmac("sha512", b"hello", b"salt", 16, 8).hex())'
        let h = Hasher::from_config("pbkdf2:16:8:sha512", "salt").unwrap();
        assert_eq!(h.uhash("hello"), "d4688bd434afd8d9");
    }

    #[test]
    fn pbkdf2_with_unknown_func_errors() {
        assert!(Hasher::from_config("pbkdf2:1000:6:whirlpool", "s").is_err());
    }

    #[test]
    fn unknown_algorithm_errors() {
        // Python raises ValueError via the factory; we bail with anyhow.
        assert!(Hasher::from_config("foo", "s").is_err());
    }
}
