//! IP address anonymization matching isso/utils/__init__.py::anonymize.
//!
//! IPv4 → zero final octet ("1.2.3.4" -> "1.2.3.0").
//! IPv6 → zero the last 5 segments, use exploded form with 4-hex-digit groups.
//! Invalid addresses → "0.0.0.0".

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub fn anonymize(remote_addr: &str) -> String {
    match remote_addr.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => anonymize_v4(v4),
        Ok(IpAddr::V6(v6)) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                anonymize_v4(mapped)
            } else {
                anonymize_v6(v6)
            }
        }
        Err(_) => "0.0.0.0".into(),
    }
}

fn anonymize_v4(v4: Ipv4Addr) -> String {
    let [a, b, c, _] = v4.octets();
    format!("{a}.{b}.{c}.0")
}

fn anonymize_v6(v6: Ipv6Addr) -> String {
    // Python's exploded form: "xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx" (32 hex chars + 7 colons).
    // Keep the first 3 groups, zero the last 5.
    let segs = v6.segments();
    format!(
        "{:04x}:{:04x}:{:04x}:0000:0000:0000:0000:0000",
        segs[0], segs[1], segs[2]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_zeroes_last_octet() {
        assert_eq!(anonymize("192.168.1.42"), "192.168.1.0");
        assert_eq!(anonymize("1.2.3.4"), "1.2.3.0");
    }

    #[test]
    fn ipv6_zeroes_last_five_groups() {
        assert_eq!(
            anonymize("2001:db8:85a3::8a2e:370:7334"),
            "2001:0db8:85a3:0000:0000:0000:0000:0000"
        );
    }

    #[test]
    fn ipv4_mapped_is_treated_as_ipv4() {
        assert_eq!(anonymize("::ffff:192.168.1.42"), "192.168.1.0");
    }

    #[test]
    fn invalid_becomes_zero() {
        assert_eq!(anonymize("not an ip"), "0.0.0.0");
    }
}
