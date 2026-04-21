//! itsdangerous-compatible signer for cookies and moderation URLs.
//!
//! Wire-compatible with `itsdangerous >= 2.0`: tokens emitted here verify
//! and decode in Python (verified against the reference itsdangerous 2.2.0),
//! and vice-versa (Python-emitted tokens are decoded by tests in this
//! module). For raw-JSON payloads the output is byte-identical; for
//! zlib-compressed payloads the overall format is identical but the DEFLATE
//! bytes differ between flate2 and CPython's zlib — both are valid DEFLATE
//! streams, and each side decodes the other correctly.
//!
//! Token format: `payload.timestamp.signature`, all url-safe base64 without
//! padding, `.` separator.
//!
//! - **Payload**: JSON (`serde_json::to_vec`, which matches Python's compact
//!   `(',', ':')` separators). If zlib-compressing the JSON yields a strictly
//!   shorter byte string, the compressed bytes are used and a literal `.` is
//!   prepended before base64 encoding as a marker for the decoder.
//! - **Timestamp**: current Unix seconds encoded as a big-endian unsigned
//!   integer with leading zero bytes stripped, url-safe base64 without
//!   padding. (itsdangerous switched from its own 2011 epoch to Unix time in
//!   the 2.0 release.)
//! - **Signature**: `HMAC-SHA1(derived_key, "{payload}.{timestamp}")`.
//! - **Derived key**: `SHA1(salt + "signer" + secret)`. Salt defaults to
//!   `b"itsdangerous"` to match `URLSafeTimedSerializer`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha1 = Hmac<Sha1>;

/// itsdangerous default salt for `URLSafeTimedSerializer`.
pub const DEFAULT_SALT: &[u8] = b"itsdangerous";

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("malformed token")]
    Malformed,
    #[error("bad signature")]
    BadSignature,
    #[error("token expired")]
    Expired,
    #[error("decompression failed")]
    Decompression,
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("base64: {0}")]
    Base64(#[from] base64::DecodeError),
}

#[derive(Debug, Clone)]
pub struct Signer {
    derived_key: [u8; 20],
}

impl Signer {
    /// Construct a signer for the given secret, using the default
    /// itsdangerous salt.
    pub fn new(secret: &[u8]) -> Self {
        Self::with_salt(secret, DEFAULT_SALT)
    }

    pub fn with_salt(secret: &[u8], salt: &[u8]) -> Self {
        let mut h = Sha1::new();
        h.update(salt);
        h.update(b"signer");
        h.update(secret);
        let derived_key: [u8; 20] = h.finalize().into();
        Self { derived_key }
    }

    /// Encode `value` and sign it with the current Unix time. Returns the
    /// full `payload.timestamp.signature` string.
    pub fn sign<T: serde::Serialize>(&self, value: &T) -> Result<String, SignError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.sign_at(value, now)
    }

    /// Like [`Signer::sign`] but uses the caller-supplied timestamp. Used in
    /// tests and when back-dating/forward-dating tokens deterministically.
    pub fn sign_at<T: serde::Serialize>(
        &self,
        value: &T,
        now_unix: u64,
    ) -> Result<String, SignError> {
        let payload = encode_payload(value)?;
        let ts = encode_timestamp(now_unix);
        let to_sign = format!("{payload}.{ts}");
        let mac = self.hmac(to_sign.as_bytes());
        Ok(format!("{to_sign}.{}", URL_SAFE_NO_PAD.encode(mac)))
    }

    /// Verify `token` and deserialise the payload.
    ///
    /// Rejects tokens older than `max_age_secs` (measured against
    /// `now_unix`). Pass `None` to skip the age check (used for links that
    /// never expire, like unsubscribe with `max_age=2**32` in Python).
    pub fn unsign<T: serde::de::DeserializeOwned>(
        &self,
        token: &str,
        max_age_secs: Option<u64>,
        now_unix: u64,
    ) -> Result<T, SignError> {
        let (rest, sig) = token.rsplit_once('.').ok_or(SignError::Malformed)?;
        let (payload_b64, ts_b64) = rest.rsplit_once('.').ok_or(SignError::Malformed)?;

        // Verify signature over the pre-encoded "payload.timestamp" string.
        let sig_bytes = URL_SAFE_NO_PAD.decode(sig)?;
        let expected = self.hmac(rest.as_bytes());
        if sig_bytes.len() != expected.len() || !constant_time_eq(&sig_bytes, &expected) {
            return Err(SignError::BadSignature);
        }

        // Only now that the signature verified do we trust the timestamp.
        let ts_bytes = URL_SAFE_NO_PAD.decode(ts_b64)?;
        let mut ts: u64 = 0;
        for b in &ts_bytes {
            ts = (ts << 8) | (*b as u64);
        }
        if let Some(max_age) = max_age_secs {
            if now_unix.saturating_sub(ts) > max_age {
                return Err(SignError::Expired);
            }
        }

        decode_payload(payload_b64)
    }

    fn hmac(&self, data: &[u8]) -> [u8; 20] {
        let mut mac =
            HmacSha1::new_from_slice(&self.derived_key).expect("HMAC accepts any key size");
        mac.update(data);
        mac.finalize().into_bytes().into()
    }
}

/// JSON-encode with compact separators and — if it shrinks the output —
/// zlib-compress, marked with a leading literal `.` before b64 encoding.
fn encode_payload<T: serde::Serialize>(value: &T) -> Result<String, SignError> {
    let raw = serde_json::to_vec(value)?;
    let mut compressed_body = Vec::new();
    let mut enc = ZlibEncoder::new(&mut compressed_body, Compression::default());
    enc.write_all(&raw).map_err(|_| SignError::Decompression)?;
    enc.finish().map_err(|_| SignError::Decompression)?;

    // itsdangerous picks the compressed form only when it saves at least
    // two bytes: `len(compressed) < len(json) - 1`. Stay exact on that
    // threshold so tokens agree on which branch they take.
    if compressed_body.len() + 1 < raw.len() {
        let encoded = URL_SAFE_NO_PAD.encode(&compressed_body);
        Ok(format!(".{encoded}"))
    } else {
        Ok(URL_SAFE_NO_PAD.encode(&raw))
    }
}

fn decode_payload<T: serde::de::DeserializeOwned>(payload_b64: &str) -> Result<T, SignError> {
    // A leading '.' marks a zlib-compressed payload.
    let (compressed, body_b64) = match payload_b64.strip_prefix('.') {
        Some(rest) => (true, rest),
        None => (false, payload_b64),
    };
    let raw = URL_SAFE_NO_PAD.decode(body_b64)?;
    let json: Vec<u8> = if compressed {
        let mut dec = ZlibDecoder::new(&raw[..]);
        let mut out = Vec::new();
        dec.read_to_end(&mut out)
            .map_err(|_| SignError::Decompression)?;
        out
    } else {
        raw
    };
    Ok(serde_json::from_slice(&json)?)
}

fn encode_timestamp(now_unix: u64) -> String {
    // itsdangerous stores the int big-endian, stripping leading zero bytes.
    let bytes = now_unix.to_be_bytes();
    let start = bytes
        .iter()
        .position(|b| *b != 0)
        .unwrap_or(bytes.len() - 1);
    URL_SAFE_NO_PAD.encode(&bytes[start..])
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEST_SECRET: &[u8] = b"my-session-key-abcdef1234567890";

    /// Reference tokens from:
    ///   python3 -c "
    ///   import itsdangerous
    ///   s = itsdangerous.URLSafeTimedSerializer('my-session-key-abcdef1234567890')
    ///   print(s.dumps(...))"
    ///
    /// All four captured in the same second → same timestamp `1776814194`.
    const REF_TS: u64 = 1776814194;

    #[test]
    fn sign_matches_itsdangerous_list_payload() {
        let s = Signer::new(TEST_SECRET);
        let got = s.sign_at(&(23_i64, "abc123".to_string()), REF_TS).unwrap();
        assert_eq!(got, "WzIzLCJhYmMxMjMiXQ.aegIcg.d8YhkF_sn5gzqPoMeh1v6qbDB1E");
    }

    #[test]
    fn sign_matches_itsdangerous_str_list_payload() {
        let s = Signer::new(TEST_SECRET);
        let got = s
            .sign_at(&("unsubscribe", "jane@example.com"), REF_TS)
            .unwrap();
        assert_eq!(
            got,
            "WyJ1bnN1YnNjcmliZSIsImphbmVAZXhhbXBsZS5jb20iXQ.aegIcg.J98aTT5_jDIhG2whn0Fz4MqYwfk"
        );
    }

    #[test]
    fn sign_matches_itsdangerous_int_payload() {
        let s = Signer::new(TEST_SECRET);
        let got = s.sign_at(&42_i64, REF_TS).unwrap();
        assert_eq!(got, "NDI.aegIcg.1rOsch1Gsc_zA_XaAQgl7GEsDYA");
    }

    #[test]
    fn sign_matches_itsdangerous_object_payload() {
        let s = Signer::new(TEST_SECRET);
        let got = s.sign_at(&json!({"a": 1, "b": [1, 2]}), REF_TS).unwrap();
        assert_eq!(
            got,
            "eyJhIjoxLCJiIjpbMSwyXX0.aegIcg.zpKzBYkNTqmOnn5rqOjSU1ezY5E"
        );
    }

    #[test]
    fn sign_compresses_long_payload() {
        // itsdangerous and flate2 both produce valid DEFLATE streams, but
        // not the same literal bytes: the *format* (leading '.' marker,
        // b64 encoding, HMAC over the encoded token) is spec'd, but the
        // exact DEFLATE output is not. A token produced by this signer was
        // confirmed to decode correctly in Python's itsdangerous 2.2.0, so
        // wire-compat is maintained even though the bytes differ.
        //
        // Assert the structural invariants only:
        //   1. Compressed payloads start with a literal '.'
        //   2. The signer roundtrips the value through itself
        //   3. Short payloads don't take the compression branch
        let s = Signer::new(TEST_SECRET);
        let value = "aaaa".repeat(100);
        let compressed_tok = s.sign_at(&value, 1776814201).unwrap();
        assert!(
            compressed_tok.starts_with('.'),
            "expected compressed marker, got {compressed_tok}"
        );
        let decoded: String = s.unsign(&compressed_tok, None, 1776814201).unwrap();
        assert_eq!(decoded, value);

        // Force a short payload through the same code path and confirm it
        // does NOT compress (`len(compressed) + 1 < len(json)` false for
        // three bytes).
        let short_tok = s.sign_at(&"hi", 1776814201).unwrap();
        assert!(
            !short_tok.starts_with('.'),
            "short token should not compress"
        );
    }

    #[test]
    fn unsign_roundtrips_ourselves() {
        let s = Signer::new(TEST_SECRET);
        let tok = s.sign_at(&(23_i64, "abc123".to_string()), REF_TS).unwrap();
        let (id, hash): (i64, String) = s.unsign(&tok, Some(1000), REF_TS + 30).unwrap();
        assert_eq!((id, hash), (23, "abc123".to_string()));
    }

    #[test]
    fn unsign_reads_itsdangerous_token() {
        // Prove wire-compat in the other direction: a token produced by
        // Python decodes cleanly here.
        let s = Signer::new(TEST_SECRET);
        let tok = "WzIzLCJhYmMxMjMiXQ.aegIcg.d8YhkF_sn5gzqPoMeh1v6qbDB1E";
        let (id, hash): (i64, String) = s.unsign(tok, Some(u64::MAX), REF_TS).unwrap();
        assert_eq!((id, hash), (23, "abc123".to_string()));
    }

    #[test]
    fn unsign_reads_compressed_itsdangerous_token() {
        let s = Signer::new(TEST_SECRET);
        let tok = ".eJxTShwFgwooAQCJ6ZfV.aegIeQ.b1Li7eWDft2rK35UVZ4z19jaopw";
        let got: String = s.unsign(tok, Some(u64::MAX), 1776814201).unwrap();
        assert_eq!(got, "aaaa".repeat(100));
    }

    #[test]
    fn unsign_rejects_tampered_payload() {
        let s = Signer::new(TEST_SECRET);
        // Flip one character of the payload; signature must not match.
        let bad = "WzIzLCJhYmMxMzMiXQ.aegIcg.d8YhkF_sn5gzqPoMeh1v6qbDB1E";
        let err = s
            .unsign::<(i64, String)>(bad, Some(u64::MAX), REF_TS)
            .unwrap_err();
        assert!(matches!(err, SignError::BadSignature), "got {err:?}");
    }

    #[test]
    fn unsign_rejects_expired_token() {
        let s = Signer::new(TEST_SECRET);
        let tok = s.sign_at(&"payload", 1_000_000).unwrap();
        // Try to decode it 3600 seconds later with a 900-second max age.
        let err = s.unsign::<String>(&tok, Some(900), 1_003_600).unwrap_err();
        assert!(matches!(err, SignError::Expired), "got {err:?}");
    }

    #[test]
    fn unsign_without_max_age_allows_ancient_token() {
        // max_age=None should bypass the age check (Python uses this for
        // unsubscribe links with max_age=2**32).
        let s = Signer::new(TEST_SECRET);
        let tok = s.sign_at(&"ancient", 1_000).unwrap();
        let got: String = s.unsign(&tok, None, 2_000_000_000).unwrap();
        assert_eq!(got, "ancient");
    }

    #[test]
    fn encode_timestamp_strips_leading_zeros() {
        // The first few years of unix time fit in 4 bytes; itsdangerous
        // strips any leading zero bytes from the big-endian encoding.
        assert_eq!(encode_timestamp(0), "AA");
        assert_eq!(encode_timestamp(255), "_w");
        // A real modern timestamp round-trips to 4 bytes.
        assert_eq!(encode_timestamp(1776814194), "aegIcg");
    }
}
