//! itsdangerous-compatible signer for cookies and moderation URLs.
//!
//! Reproduces `itsdangerous.URLSafeTimedSerializer(secret).dumps(obj)`:
//! 1. JSON-serialise the payload (sort_keys=False, separators=(',', ':')) and
//!    UTF-8 encode.
//! 2. Compress via zlib and prepend `.` if that shrinks the payload (matches
//!    itsdangerous URLSafeSerializer).
//! 3. URL-safe base64 encode without padding.
//! 4. Append `.{timestamp}` (seconds since itsdangerous EPOCH 2011-01-01).
//! 5. Sign the result with HMAC-SHA1 keyed by `derive_key(secret, salt)` and
//!    append `.{signature}` (URL-safe b64 without padding).
//!
//! Default salt: `"itsdangerous.Signer"`.
//! Default key derivation: `sha1(salt + "signer" + secret)`.
//!
//! See https://itsdangerous.palletsprojects.com/ for the exact algorithm.

// TODO: full implementation. The signer is only consumed by comment cookies
// and moderation URLs, so stub for now.
pub struct Signer {
    #[allow(dead_code)]
    secret: Vec<u8>,
}

impl Signer {
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
        }
    }
}
