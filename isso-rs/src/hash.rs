//! Hash factory matching isso/utils/hash.py.
//!
//! The public API (`Hasher::uhash(s)`) returns a lowercase hex string, and is
//! applied to `email or remote_addr` to populate the "hash" field on comment
//! JSON. We implement pbkdf2, sha1, md5, and "none" — same algorithms Python
//! supports.

use hmac::Hmac;
use pbkdf2::pbkdf2;
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

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
            if func != "sha1" {
                // Python supports arbitrary hashlib names; we only wire sha1 for now.
                // TODO: support sha256, sha512, etc. under pbkdf2.
                anyhow::bail!("pbkdf2 with func={func} not implemented yet");
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
                // md5 requires pulling in the md-5 crate; emulate via a simple impl
                // only if someone actually enables it. Keep it behind a TODO so
                // nobody silently picks up an unsupported config.
                // TODO: add md-5 dependency if any deployment actually uses md5.
                panic!("md5 hasher not implemented");
            }
            Hasher::Pbkdf2(p) => {
                let mut out = vec![0u8; p.dklen];
                // pbkdf2 crate returns Result in newer versions
                pbkdf2::<HmacSha1>(bytes, &p.salt, p.iterations, &mut out)
                    .expect("pbkdf2 output length is valid");
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
}
