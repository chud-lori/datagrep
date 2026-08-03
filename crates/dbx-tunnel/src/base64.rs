//! Minimal standard-alphabet base64 (RFC 4648 §4, with padding).
//!
//! Used only to render/parse the known-hosts file's `host:port base64-key`
//! entries (design §3.8). Not a dependency-worthy amount of code, and the
//! crate's dependency list is deliberately pinned (see crate root docs), so
//! this stays hand-rolled rather than pulling in a `base64` crate.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();

        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0b0000_0011) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        match b1 {
            Some(b1) => {
                out.push(
                    ALPHABET[(((b1 & 0b0000_1111) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
                );
            }
            None => out.push('='),
        }
        match b2 {
            Some(b2) => out.push(ALPHABET[(b2 & 0b0011_1111) as usize] as char),
            None => out.push('='),
        }
    }
    out
}

fn decode_char(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub(crate) fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    if s.is_empty() {
        return Some(Vec::new());
    }
    if !s.bytes().all(|b| decode_char(b).is_some()) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 3);
    let mut bits: u32 = 0;
    let mut n_bits = 0u32;
    for b in s.bytes() {
        let v = decode_char(b)? as u32;
        bits = (bits << 6) | v;
        n_bits += 6;
        if n_bits >= 8 {
            n_bits -= 8;
            out.push((bits >> n_bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_arbitrary_lengths() {
        for len in 0..=64usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 5) as u8).collect();
            let encoded = encode(&bytes);
            let decoded = decode(&encoded).expect("valid base64");
            assert_eq!(decoded, bytes, "round-trip failed at len {len}");
        }
    }

    #[test]
    fn matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(decode("not valid base64!!").is_none());
    }
}
