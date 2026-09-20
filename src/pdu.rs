//! GSM 03.40 SMS-SUBMIT encoding, the MO half of A5.
//!
//! The daemon sends in **PDU mode**, not text mode, for two measured reasons:
//! this CP's own text-mode submit answers `+CMS ERROR: 313` (while the same
//! SIM receives just fine), and text mode cannot carry Chinese at all under a
//! GSM character set.  PDU is also what the vendor RIL does — which is why it
//! leaves the TE character set at `HEX` and the daemon has to re-arm it.
//!
//! The body is always encoded as UCS2 (DCS 0x08).  That costs the 70-character
//! single-message limit and skips GSM 7-bit packing, and buys one code path
//! that carries ASCII and 中文 alike.  Concatenated submits are a later,
//! separately-tested increment.

use anyhow::{bail, Result};

/// The digits of a phone number.  The `+` is not a digit and must not reach
/// the semi-octet packer — it only decides the type-of-address elsewhere.
fn digits(number: &str) -> String {
    number.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// 27.005 §4.1.2.1 / 03.40 §9.1.2.5: the address digits as semi-octets — the
/// first digit in the *low* nibble, the second in the high, and `F` padding
/// the high nibble of the last byte when the count is odd (`13000000000` ends
/// `…76 F3`, never `…76 3F`).
fn semioctet_bytes(digits: &str) -> Vec<u8> {
    let d: Vec<char> = digits.chars().collect();
    d.chunks(2)
        .map(|pair| {
            let low = pair[0].to_digit(16).unwrap_or(0) as u8;
            let high = pair
                .get(1)
                .map(|c| c.to_digit(16).unwrap_or(0xF) as u8)
                .unwrap_or(0xF);
            (high << 4) | low
        })
        .collect()
}

/// The destination address field: length in digits, type-of-address, then the
/// semi-octets.  A leading `+` means international (`0x91`); anything else is
/// the "unknown type, ISDN plan" `0x81`, which is what a national number like
/// `13000000000` wants.
fn address_field(number: &str) -> Vec<u8> {
    let d = digits(number);
    let international = number.trim_start().starts_with('+');
    let toa: u8 = if international { 0x91 } else { 0x81 };
    let mut out = vec![d.len() as u8, toa];
    out.extend(semioctet_bytes(&d));
    out
}

/// The service-centre field.  `None` (or an empty string) encodes as a
/// zero-length field, which tells the CP to use its own default — the one
/// `AT+CSCA?` names.
fn smsc_field(smsc: Option<&str>) -> Result<Vec<u8>> {
    let Some(smsc) = smsc.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(vec![0x00]);
    };
    let d = digits(smsc);
    let international = smsc.starts_with('+');
    if d.is_empty() {
        bail!("SMSC {smsc:?} carries no digits");
    }
    // One length octet counts the *address* octets after the type octet.
    let octets = (d.len() + 1) / 2 + 1;
    let mut out = vec![octets as u8, if international { 0x91 } else { 0x81 }];
    out.extend(semioctet_bytes(&d));
    Ok(out)
}

/// UTF-16 code units of `text`, big-endian — UCS2 as the PDU carries it.
fn ucs2_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
}

/// The complete SMS-SUBMIT: the PDU as uppercase hex (what goes to the modem
/// after the `>` prompt) and the TPDU octet count (the `AT+CMGS=<length>`
/// argument, which excludes the service-centre field).
///
/// `smsc` names the service centre to submit through; `None` uses the CP's
/// default.
pub fn encode_submit(smsc: Option<&str>, destination: &str, text: &str) -> Result<(String, usize)> {
    if text.is_empty() {
        bail!("an empty message is nothing to send");
    }
    let d = digits(destination);
    if d.is_empty() {
        bail!("destination {destination:?} carries no digits");
    }
    let ud = ucs2_bytes(text);
    if ud.len() > 140 {
        bail!(
            "{} UCS2 characters is {} octets; a single message carries 70",
            text.chars().count(),
            ud.len()
        );
    }

    let mut tpdu: Vec<u8> = Vec::new();
    // SMS-SUBMIT, no validity period, no status report.  The first octet is
    // 0x01 and NOT 0x11: VPF=00 means the TP-VP field is absent, and 0x11
    // (VPF=relative) promises an octet this encoder does not carry -- the CP
    // then reads the whole PDU one field off and refuses it.  Measured against
    // the vendor stack's own submit (radio log `AT> 0001000B…`), which uses
    // exactly this octet.
    tpdu.push(0x01);
    tpdu.push(0x00); // TP-MR: the CP numbers the message itself
    tpdu.extend(address_field(destination));
    tpdu.push(0x00); // TP-PID: short message
    tpdu.push(0x08); // TP-DCS: UCS2
    tpdu.push(ud.len() as u8); // TP-UDL: octets for UCS2
    tpdu.extend(ud);

    let smsc = smsc_field(smsc)?;
    let octets = tpdu.len();
    let mut pdu = smsc;
    pdu.extend(tpdu);

    Ok((pdu.iter().map(|b| format!("{b:02X}")).collect(), octets))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semioctets_put_the_first_digit_low_and_pad_f_high() {
        assert_eq!(
            semioctet_bytes("13000000000"),
            [0x31, 0x00, 0x00, 0x00, 0x00, 0xF0]
        );
        assert_eq!(
            semioctet_bytes("<smsc>"),
            [0x68, 0x91, 0x02, 0x77, 0x00, 0x05]
        );
    }

    #[test]
    fn a_national_destination_uses_toa_81() {
        let mut want = vec![0x0B, 0x81];
        want.extend([0x31, 0x00, 0x00, 0x00, 0x00, 0xF0]);
        assert_eq!(address_field("13000000000"), want);
    }

    #[test]
    fn an_international_destination_uses_toa_91() {
        assert_eq!(address_field("+8613800138000")[1], 0x91);
    }

    /// The whole submit, hand-checked against 03.40: TP-MR, the address, the
    /// UCS2 DCS, and a user-data length in octets.
    #[test]
    fn a_submit_encodes_end_to_end() {
        let (hex, octets) = encode_submit(None, "13000000000", "test2").unwrap();
        assert_eq!(
            hex,
            "0001000B813100000000F000080A\
             00740065007300740032"
        );
        // Everything after the one-octet (empty) service-centre field.
        assert_eq!(octets, 23);
    }

    #[test]
    fn an_smsc_is_carried_when_named() {
        // <smsc> -> length 7, TOA 0x91, semi-octets 68 91 02 77 00 05.
        let (hex, octets) = encode_submit(Some("<smsc>"), "1234", "a").unwrap();
        assert!(hex.starts_with("0791689102770005"), "{hex}");
        // TPDU: MTI, MR, a 4-byte address field, PID, DCS, UDL, 2 bytes of UD.
        assert_eq!(octets, 11);
    }

    #[test]
    fn a_seventy_character_message_is_the_limit() {
        let text = "测".repeat(70);
        assert!(encode_submit(None, "1234", &text).is_ok());
        let text = "测".repeat(71);
        assert!(encode_submit(None, "1234", &text).is_err());
    }

    #[test]
    fn empty_inputs_are_refused() {
        assert!(encode_submit(None, "1234", "").is_err());
        assert!(encode_submit(None, "abc", "hi").is_err());
    }
}
