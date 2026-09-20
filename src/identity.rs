//! Device identity: the NV items that hold IMEI values, and the diag-channel
//! exchange that reads them.
//!
//! Provenance: **measured on the unit, Android side, 2026-09-20** — the read
//! frame, the record marker and the BCD-nibble layout answered as captured,
//! and the decoded IMEI Luhn-checks.  The shapes below remain the contract to
//! re-verify on a new platform: `imei read` reports what actually came back
//! (contracts §8).  Nothing in this module writes anything.

use anyhow::{bail, Context as _, Result};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

/// The record header that precedes an IMEI payload in a diag NV-read reply.
pub const IMEI_RECORD_MARKER: &[u8] = &[0x74, 0x00, 0x5E, 0x01];

/// The frame that asks the diag channel for one NV item: a fixed header, the
/// item id as two raw bytes, a fixed trailer.  The meaning of everything
/// except the item bytes is not established; the template is kept
/// byte-for-byte as captured, and `imei read` treats a reply as data, not as
/// a promise.
pub fn nv_read_frame(item_hex: &str) -> Result<Vec<u8>> {
    let hex = item_hex
        .trim()
        .trim_start_matches("0x")
        .to_ascii_lowercase();
    if hex.len() != 4 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("NV item id must be 4 hex digits, got {item_hex:?}");
    }
    let hi = u8::from_str_radix(&hex[..2], 16)?;
    let lo = u8::from_str_radix(&hex[2..], 16)?;
    Ok(vec![
        0x7E, 0x00, 0x00, 0x00, 0x00, 0x0A, 0x00, hi, lo, 0x00, 0x00, 0x7E,
    ])
}

/// One request/response round trip on a diag character node, with an overall
/// deadline.  The reply is whatever arrived between the leading 0x7E and the
/// next one (or everything seen, when the frame never closes).
pub fn diag_exchange(path: &Path, request: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening diag node {}", path.display()))?;
    dev.write_all(request)
        .with_context(|| format!("writing the request to {}", path.display()))?;

    let deadline = Instant::now() + timeout;
    let mut acc: Vec<u8> = Vec::with_capacity(256);
    let mut buf = [0u8; 512];
    loop {
        let now = Instant::now();
        if now >= deadline {
            if acc.is_empty() {
                bail!("diag {}: no reply within {timeout:?}", path.display());
            }
            return Ok(acc);
        }
        let mut fds = [libc::pollfd {
            fd: dev.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
        if ready < 0 {
            bail!("diag {}: poll failed", path.display());
        }
        if ready == 0 {
            continue; // re-check the deadline at the top
        }
        let n = dev.read(&mut buf)?;
        if n == 0 {
            if acc.is_empty() {
                bail!("diag {}: node returned EOF", path.display());
            }
            return Ok(acc);
        }
        acc.extend_from_slice(&buf[..n]);
        if let Some(i) = acc[1..].iter().position(|&b| b == 0x7E) {
            return Ok(acc[..i + 2].to_vec());
        }
        if acc.len() > 4096 {
            bail!("diag {}: oversized reply (> 4096 bytes)", path.display());
        }
    }
}

/// Nibble-decode a BCD payload: within each byte the low nibble is the first
/// digit and the high nibble the second; nibble 0xA is the odd-length filler.
pub fn decode_bcd_digits(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        for d in [b & 0x0F, b >> 4] {
            s.push(if d < 10 { (b'0' + d) as char } else { 'a' });
        }
    }
    s
}

/// Extract the 15 IMEI digits that follow the record marker in a reply.
/// Filler nibbles are dropped wherever they sit; anything that does not yield
/// exactly 15 digits is None, and the caller says so instead of guessing.
pub fn extract_imei(reply: &[u8], marker: &[u8]) -> Option<String> {
    let pos = reply.windows(marker.len()).position(|w| w == marker)?;
    let after = &reply[pos + marker.len()..];
    let end = after.iter().position(|&b| b == 0x7E).unwrap_or(after.len());
    let take = end.min(16); // 8 bytes hold 15 digits plus the filler
    let digits: String = decode_bcd_digits(&after[..take])
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(15)
        .collect();
    (digits.len() == 15).then_some(digits)
}

/// The Luhn check digit of the first 14 digits of an IMEI.
pub fn imei_check_digit(first14: &str) -> Option<u32> {
    let b = first14.as_bytes();
    if b.len() != 14 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut sum: u32 = 0;
    for (i, ch) in b.iter().rev().enumerate() {
        let mut v = (ch - b'0') as u32;
        if i % 2 == 0 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    Some((10 - sum % 10) % 10)
}

/// True when `imei` is 15 ASCII digits and its check digit satisfies Luhn.
pub fn luhn_valid(imei: &str) -> bool {
    let b = imei.as_bytes();
    if b.len() != 15 || !b.iter().all(u8::is_ascii_digit) {
        return false;
    }
    imei_check_digit(&imei[..14]) == Some((b[14] - b'0') as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_frame_matches_the_captured_template() {
        assert_eq!(
            nv_read_frame("5e81").unwrap(),
            vec![0x7E, 0, 0, 0, 0, 0x0A, 0x00, 0x5E, 0x81, 0x00, 0x00, 0x7E]
        );
        assert_eq!(nv_read_frame("0x5E90").unwrap()[7], 0x5E);
        assert!(nv_read_frame("5e8").is_err());
        assert!(nv_read_frame("zz81").is_err());
    }

    #[test]
    fn luhn_accepts_the_canonical_imei() {
        assert!(luhn_valid("490154203237518"));
        assert!(!luhn_valid("490154203237519"));
        assert_eq!(imei_check_digit("49015420323751"), Some(8));
        assert!(!luhn_valid("49015420323751")); // 14 digits
        assert!(!luhn_valid("49015420323751x")); // not all digits
    }

    #[test]
    fn bcd_decode_is_low_nibble_first_with_filler() {
        assert_eq!(decode_bcd_digits(&[0x4A, 0x09]), "a490");
        assert_eq!(decode_bcd_digits(&[0x21]), "12");
    }

    #[test]
    fn extract_pulls_fifteen_digits_out_of_a_reply_frame() {
        let mut reply = vec![0x7E, 0x00, 0x00, 0x74, 0x00, 0x5E, 0x01];
        reply.extend_from_slice(&[0x4A, 0x09, 0x51, 0x24, 0x30, 0x32, 0x57, 0x81]);
        reply.push(0x7E);
        assert_eq!(
            extract_imei(&reply, IMEI_RECORD_MARKER).as_deref(),
            Some("490154203237518")
        );
        assert!(luhn_valid(
            &extract_imei(&reply, IMEI_RECORD_MARKER).unwrap()
        ));
    }

    #[test]
    fn extract_returns_none_on_garbage() {
        assert_eq!(extract_imei(&[0x7E, 0x00, 0x7E], IMEI_RECORD_MARKER), None);
        assert_eq!(extract_imei(&[], IMEI_RECORD_MARKER), None);
    }
}
