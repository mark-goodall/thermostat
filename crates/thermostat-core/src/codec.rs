//! Small encodings: hex, base64 (decode only) and COBS framing.

pub mod hex {
    use heapless::String;

    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode<const N: usize>(bytes: &[u8]) -> String<N> {
        let mut s = String::new();
        for b in bytes {
            let _ = s.push(DIGITS[(b >> 4) as usize] as char);
            let _ = s.push(DIGITS[(b & 15) as usize] as char);
        }
        s
    }

    pub fn decode(s: &str, out: &mut [u8]) -> Option<()> {
        let s = s.as_bytes();
        if s.len() != out.len() * 2 {
            return None;
        }
        for (i, o) in out.iter_mut().enumerate() {
            *o = (nibble(s[2 * i])? << 4) | nibble(s[2 * i + 1])?;
        }
        Some(())
    }

    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
}

pub mod base64 {
    /// Decode standard base64 into `out`; returns the decoded length.
    pub fn decode(s: &str, out: &mut [u8]) -> Option<usize> {
        let mut acc = 0u32;
        let mut bits = 0;
        let mut n = 0;
        for c in s.bytes() {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => break,
                _ => return None,
            };
            acc = (acc << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                *out.get_mut(n)? = (acc >> bits) as u8;
                n += 1;
            }
        }
        Some(n)
    }
}

/// Consistent Overhead Byte Stuffing: frames contain no zero bytes, so a zero
/// delimits frames on a byte stream shared with log text.
pub mod cobs {
    /// Worst-case encoded size of `n` bytes.
    pub const fn max_encoded_len(n: usize) -> usize {
        n + n / 254 + 1
    }

    /// Encode `src` into `dst`; returns the encoded length (without delimiter).
    pub fn encode(src: &[u8], dst: &mut [u8]) -> usize {
        let mut code_at = 0;
        let mut out = 1;
        let mut code = 1u8;
        for &b in src {
            if b == 0 {
                dst[code_at] = code;
                code_at = out;
                out += 1;
                code = 1;
            } else {
                dst[out] = b;
                out += 1;
                code += 1;
                if code == 0xFF {
                    dst[code_at] = code;
                    code_at = out;
                    out += 1;
                    code = 1;
                }
            }
        }
        dst[code_at] = code;
        out
    }

    /// Decode in place; returns the decoded length.
    pub fn decode_in_place(buf: &mut [u8]) -> Option<usize> {
        let mut read = 0;
        let mut write = 0;
        while read < buf.len() {
            let code = buf[read] as usize;
            if code == 0 || read + code > buf.len() {
                return None;
            }
            read += 1;
            for _ in 1..code {
                buf[write] = buf[read];
                write += 1;
                read += 1;
            }
            if code < 0xFF && read < buf.len() {
                buf[write] = 0;
                write += 1;
            }
        }
        Some(write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let s = hex::encode::<8>(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(s.as_str(), "deadbeef");
        let mut out = [0u8; 4];
        hex::decode("DEADbeef", &mut out).unwrap();
        assert_eq!(out, [0xde, 0xad, 0xbe, 0xef]);
        assert!(hex::decode("zz", &mut [0u8; 1]).is_none());
    }

    #[test]
    fn base64_decode() {
        let mut out = [0u8; 32];
        let n = base64::decode("YWRtaW46c2VjcmV0", &mut out).unwrap();
        assert_eq!(&out[..n], b"admin:secret");
        let n = base64::decode("YQ==", &mut out).unwrap();
        assert_eq!(&out[..n], b"a");
    }

    #[test]
    fn cobs_roundtrip() {
        let cases: [&[u8]; 6] = [
            &[],
            &[0],
            &[1, 2, 3],
            &[0, 0, 1, 0],
            &[0x11; 300],
            &[0; 600],
        ];
        for case in cases {
            let mut enc = [0u8; 1024];
            let n = cobs::encode(case, &mut enc);
            assert!(n <= cobs::max_encoded_len(case.len()));
            assert!(!enc[..n].contains(&0));
            let m = cobs::decode_in_place(&mut enc[..n]).unwrap();
            assert_eq!(&enc[..m], case);
        }
        let mut long = std::vec::Vec::new();
        for i in 0..1000u32 {
            long.push((i % 7) as u8);
        }
        let mut enc = [0u8; 1100];
        let n = cobs::encode(&long, &mut enc);
        let m = cobs::decode_in_place(&mut enc[..n]).unwrap();
        assert_eq!(&enc[..m], &long[..]);
    }

    #[test]
    fn cobs_rejects_garbage() {
        assert!(cobs::decode_in_place(&mut [5, 1]).is_none());
        assert!(cobs::decode_in_place(&mut [0, 1]).is_none());
    }
}
