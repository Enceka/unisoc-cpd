//! The CP generation's own AT extensions, as executable contract.
//!
//! The standard 27.007 commands (`+CEREG`, `+CSQ`, `+CGACT`, ...) are the same
//! everywhere; these are the ones a Unisoc baseband adds, and they are the
//! reason the plan can promise "the AP-side contract is a property of the
//! baseband generation".  Keeping the bit-mask tables and the encoders here
//! means a second platform of the same generation inherits them, and a
//! different generation replaces this one module.
//!
//! Provenance: the band/cell/5G/VoLTE commands and the NR band tables below
//! were found by researching this CP generation's own AT surface.  Nothing
//! here writes NV or an identity.

/// 5G NR bands addressable by the first mask word of `AT+SPLBAND=2`.
pub const NR_BAND_VALUE1: &[u32] = &[1, 2, 3, 5, 7, 8, 12, 20, 25, 28, 66, 70, 71, 74];
/// 5G NR bands addressed by the third mask word.
pub const NR_BAND_VALUE3: &[u32] = &[34, 38, 39, 40, 41, 50, 51, 77, 78, 79];
/// The "super band" table, most of them supplementary uplink / carrier
/// aggregation partners (n75/n76 are SUL for n41/n78, and so on).
pub const NR_SUPER_BAND: &[u32] = &[75, 76, 80, 81, 82, 83, 84, 86];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rat {
    Lte,
    Nr,
}

impl Rat {
    pub fn parse(s: &str) -> Option<Rat> {
        match s.trim().to_ascii_uppercase().as_str() {
            "LTE" | "4G" => Some(Rat::Lte),
            "NR" | "5G" => Some(Rat::Nr),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Rat::Lte => "LTE",
            Rat::Nr => "NR",
        }
    }

    /// The RAT selector `AT+SPFORCEFRQ` takes (12 = LTE, 16 = NR).
    pub fn force_freq_prefix(self) -> u32 {
        match self {
            Rat::Lte => 12,
            Rat::Nr => 16,
        }
    }
}

/// Every integer in the payload of a one-line response, ignoring the header,
/// the final result code and the framing.
pub fn payload_ints(line: &str) -> Vec<i64> {
    let body = match line.split_once(':') {
        Some((_, rest)) => rest,
        None => line,
    };
    let body = match body.find("OK") {
        Some(i) => &body[..i],
        None => body,
    };
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in body.chars() {
        if ch.is_ascii_digit() || (ch == '-' && current.is_empty()) {
            current.push(ch);
        } else {
            if !current.is_empty() && current != "-" {
                if let Ok(v) = current.parse::<i64>() {
                    out.push(v);
                }
            }
            current.clear();
        }
    }
    if !current.is_empty() && current != "-" {
        if let Ok(v) = current.parse::<i64>() {
            out.push(v);
        }
    }
    out
}

// ------------------------------------------------------------------ LTE bands

/// `AT+SPLBAND=1,<49-64>,<33-48>,<17-32>,<1-16>,<65-80>`
pub fn lte_band_lock_command(bands: &[u32]) -> String {
    let mut g49 = 0u32;
    let mut g33 = 0u32;
    let mut g17 = 0u32;
    let mut g1 = 0u32;
    let mut g65 = 0u32;
    for band in dedup(bands) {
        match band {
            1..=16 => g1 |= 1 << (band - 1),
            17..=32 => g17 |= 1 << (band - 17),
            33..=48 => g33 |= 1 << (band - 33),
            49..=64 => g49 |= 1 << (band - 49),
            65..=80 => g65 |= 1 << (band - 65),
            _ => {}
        }
    }
    format!("AT+SPLBAND=1,{g49},{g33},{g17},{g1},{g65}")
}

/// `AT+SPLBAND=0` answers the same five mask words.
pub fn parse_lte_bands(line: &str) -> Vec<u32> {
    let values = payload_ints(line);
    let mut out = Vec::new();
    for (group, mask) in values.iter().take(5).enumerate() {
        let base: u32 = match group {
            0 => 49,
            1 => 33,
            2 => 17,
            3 => 1,
            4 => 65,
            _ => continue,
        };
        for bit in 0..16u32 {
            if mask & (1i64 << bit) != 0 {
                out.push(base + bit);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ------------------------------------------------------------------- NR bands

/// `AT+SPLBAND=2,<value1>,0,<value3>,<super>`
pub fn nr_band_lock_command(bands: &[u32]) -> String {
    let mut v1 = 0u32;
    let mut v3 = 0u32;
    let mut sup = 0u32;
    for band in dedup(bands) {
        if let Some(i) = NR_BAND_VALUE1.iter().position(|b| *b == band) {
            v1 |= 1 << i;
        } else if let Some(i) = NR_BAND_VALUE3.iter().position(|b| *b == band) {
            v3 |= 1 << i;
        } else if let Some(i) = NR_SUPER_BAND.iter().position(|b| *b == band) {
            sup |= 1 << i;
        }
    }
    format!("AT+SPLBAND=2,{v1},0,{v3},{sup}")
}

/// `AT+SPLBAND=3` answers four words; words 0, 2 and 3 are the masks.
pub fn parse_nr_bands(line: &str) -> Vec<u32> {
    let values = payload_ints(line);
    let mut out = Vec::new();
    if let Some(mask) = values.first() {
        for (i, band) in NR_BAND_VALUE1.iter().enumerate() {
            if mask & (1i64 << i) != 0 {
                out.push(*band);
            }
        }
    }
    if let Some(mask) = values.get(2) {
        for (i, band) in NR_BAND_VALUE3.iter().enumerate() {
            if mask & (1i64 << i) != 0 {
                out.push(*band);
            }
        }
    }
    if let Some(mask) = values.get(3) {
        for (i, band) in NR_SUPER_BAND.iter().enumerate() {
            if mask & (1i64 << i) != 0 {
                out.push(*band);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// `AT+SPLBAND=<n>` reads the lock back: 0 = LTE, 3 = NR.
pub fn band_query_command(rat: Rat) -> &'static str {
    match rat {
        Rat::Lte => "AT+SPLBAND=0",
        Rat::Nr => "AT+SPLBAND=3",
    }
}

/// Clearing every mask word is the documented "no band lock".
pub fn band_unlock_command(rat: Rat) -> &'static str {
    match rat {
        Rat::Lte => "AT+SPLBAND=1,0,0,0,0,0",
        Rat::Nr => "AT+SPLBAND=2,0,0,0,0",
    }
}

pub fn parse_locked_bands(line: &str, rat: Rat) -> Vec<u32> {
    match rat {
        Rat::Lte => parse_lte_bands(line),
        Rat::Nr => parse_nr_bands(line),
    }
}

// --------------------------------------------------------------- cell locking

/// `AT+SPFORCEFRQ=<rat>,6,<freq>,<pci>`; `<rat>,4` releases it, `<rat>,3` reads it.
pub fn cell_lock_command(rat: Rat, freq: u32, pci: u32) -> String {
    format!("AT+SPFORCEFRQ={},6,{freq},{pci}", rat.force_freq_prefix())
}

pub fn cell_unlock_command(rat: Rat) -> String {
    format!("AT+SPFORCEFRQ={},4", rat.force_freq_prefix())
}

pub fn cell_query_command(rat: Rat) -> String {
    format!("AT+SPFORCEFRQ={},3", rat.force_freq_prefix())
}

/// `+SPFORCEFRQ: <rat>,<action>,<freq1>,<pci1>,<freq2>,<pci2>...`
pub fn parse_locked_cells(line: &str, rat: Rat) -> Vec<(u32, u32)> {
    let values = payload_ints(line);
    if values.len() < 4 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 2usize;
    while i + 1 < values.len() {
        let (freq, pci) = (values[i], values[i + 1]);
        if freq > 0 && pci >= 0 && freq <= u32::MAX as i64 && pci <= u32::MAX as i64 {
            let entry = (freq as u32, pci as u32);
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
        i += 2;
    }
    let _ = rat;
    out
}

// ---------------------------------------------------------------- 5G / VoLTE

/// `AT+SP5GRAN?` -> `+SP5GRAN: 1` when the modem is allowed to camp on NR SA.
pub fn parse_5g_sa(line: &str) -> Option<u32> {
    payload_ints(line).first().map(|v| *v as u32)
}

/// `AT+CAVIMS?` -> `+CAVIMS: 1` when VoLTE is enabled.
pub fn parse_volte(line: &str) -> Option<u32> {
    payload_ints(line).first().map(|v| *v as u32)
}

fn dedup(bands: &[u32]) -> Vec<u32> {
    let mut v: Vec<u32> = bands.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_ints_reads_a_query_answer() {
        assert_eq!(payload_ints("+SPLBAND: 0,0,0,0,0"), vec![0, 0, 0, 0, 0]);
        assert_eq!(payload_ints("+SPLBAND: 1,2,3"), vec![1, 2, 3]);
        assert!(payload_ints("OK").is_empty());
    }

    #[test]
    fn lte_band_lock_round_trips() {
        let cmd = lte_band_lock_command(&[1, 3, 41, 78]);
        // band 1 -> 1..16 bit 0, band 3 -> bit 2, band 41 -> 33..48 bit 8,
        // band 78 -> 65..80 bit 13.
        assert_eq!(cmd, "AT+SPLBAND=1,0,256,0,5,8192");
        let readback = parse_lte_bands("+SPLBAND: 0,256,0,5,8192");
        assert_eq!(readback, vec![1, 3, 41, 78]);
    }

    #[test]
    fn lte_bands_outside_every_group_are_ignored() {
        // 81 is above the last LTE group, so it contributes no bit at all.
        assert_eq!(lte_band_lock_command(&[81]), "AT+SPLBAND=1,0,0,0,0,0");
    }

    #[test]
    fn lte_unlock_is_all_zero_words() {
        assert_eq!(band_unlock_command(Rat::Lte), "AT+SPLBAND=1,0,0,0,0,0");
        assert!(parse_lte_bands("+SPLBAND: 0,0,0,0,0").is_empty());
    }

    #[test]
    fn nr_band_lock_round_trips() {
        // n1 -> value1 index 0, n78 -> value3 index 8 (bit value 256),
        // n80 -> super index 2 (bit value 4); word 1 is the unused slot.
        let cmd = nr_band_lock_command(&[1, 78, 80]);
        assert_eq!(cmd, "AT+SPLBAND=2,1,0,256,4");

        let readback = parse_nr_bands("+SPLBAND: 1,0,256,4");
        assert_eq!(readback, vec![1, 78, 80]);
    }

    #[test]
    fn nr_unlock_is_all_zero_words() {
        assert_eq!(band_unlock_command(Rat::Nr), "AT+SPLBAND=2,0,0,0,0");
        assert!(parse_nr_bands("+SPLBAND: 0,0,0,0").is_empty());
    }

    #[test]
    fn cell_lock_uses_the_rat_prefix() {
        assert_eq!(
            cell_lock_command(Rat::Lte, 100, 88),
            "AT+SPFORCEFRQ=12,6,100,88"
        );
        assert_eq!(
            cell_lock_command(Rat::Nr, 627264, 5),
            "AT+SPFORCEFRQ=16,6,627264,5"
        );
        assert_eq!(cell_unlock_command(Rat::Lte), "AT+SPFORCEFRQ=12,4");
        assert_eq!(cell_query_command(Rat::Nr), "AT+SPFORCEFRQ=16,3");
    }

    #[test]
    fn locked_cells_are_read_back_in_pairs() {
        let cells = parse_locked_cells("+SPFORCEFRQ: 12,3,100,88,150,7", Rat::Lte);
        assert_eq!(cells, vec![(100, 88), (150, 7)]);
        assert!(parse_locked_cells("+SPFORCEFRQ: 12,3", Rat::Lte).is_empty());
    }

    #[test]
    fn rat_and_state_parsers() {
        assert_eq!(Rat::parse("5g"), Some(Rat::Nr));
        assert_eq!(Rat::parse("lte"), Some(Rat::Lte));
        assert_eq!(Rat::parse("3g"), None);
        assert_eq!(parse_5g_sa("+SP5GRAN: 1"), Some(1));
        assert_eq!(parse_volte("+CAVIMS: 0"), Some(0));
    }
}
