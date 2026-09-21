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

/// `AT+CIREG?` -> `+CIREG: <n>,<reg_state>` (27.007 §8.69): 1 = IMS
/// registered, 0 = not registered.  This is the gate a VoLTE call waits on;
/// provenance researched, the generation's own shape still to be measured.
pub fn parse_ims_reg(line: &str) -> Option<u32> {
    let mut ints = payload_ints(line);
    if ints.len() < 2 {
        return None;
    }
    Some(ints.remove(1) as u32)
}

fn dedup(bands: &[u32]) -> Vec<u32> {
    let mut v: Vec<u32> = bands.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

// ------------------------------------------------- measurement (`AT+SPENGMD`)

/// `AT+SPENGMD=0,<group>,<index>` — the vendor measurement tree this generation
/// exposes, as the Android-side helper for it spells the queries.
pub fn engmd(group: u32, index: u32) -> String {
    format!("AT+SPENGMD=0,{group},{index}")
}

/// The LTE serving cell: band, EARFCN, PCI, RSRP/RSRQ, bandwidth, cell id.
pub const ENGMD_LTE_SERVING: (u32, u32) = (6, 0);
/// The LTE neighbour list.
pub const ENGMD_LTE_NEIGHBORS: (u32, u32) = (6, 6);
/// The NR serving cell.
pub const ENGMD_NR_SERVING: (u32, u32) = (14, 1);
/// The NR neighbour list.
pub const ENGMD_NR_NEIGHBORS: (u32, u32) = (14, 2);

/// One serving cell, as far as the answer could be read.
///
/// Every field is an `Option` because the point of this struct is to keep
/// "the modem did not report it" separable from a value: the W5 rule applied
/// to a measurement.  A parser that filled in a default here would put an
/// invented number in front of someone deciding where to point an antenna.
#[derive(Debug, Clone, PartialEq)]
pub struct Serving {
    pub band: Option<String>,
    pub earfcn: Option<u32>,
    pub pci: Option<u32>,
    /// dBm, from a value the CP reports in hundredths.
    pub rsrp: Option<f64>,
    /// dB, likewise.
    pub rsrq: Option<f64>,
    /// dB, NR only.
    pub sinr: Option<f64>,
    /// As reported: an LTE code is decoded to a width, an NR one is verbatim,
    /// because the NR field's units are not measured here.
    pub bandwidth: Option<String>,
    /// The cell identity the answer carried, verbatim.
    pub cell: Option<String>,
}

impl Serving {
    /// Whether the answer carried a cell at all.  EARFCN 0 is how a CP that is
    /// not camped answers, so a zero is "no cell", not "cell number zero".
    pub fn is_cell(&self) -> bool {
        self.earfcn.is_some_and(|e| e > 0)
    }
}

/// One neighbour cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    pub band: Option<String>,
    pub earfcn: u32,
    pub pci: u32,
    pub rsrp: f64,
    pub rsrq: f64,
    pub sinr: Option<f64>,
}

/// The groups of an `AT+SPENGMD` answer.
///
/// The shape is the one the Android-side helper for this generation reads: the
/// first payload line, with the mangled minus signs restored (`,-` -> `,+`,
/// `--` -> `-+`), split on `-` into groups that are themselves comma lists.
///
/// **This is a hypothesis, not a measurement.**  It is the shape the helper's
/// own indexing implies, and it has not been captured on this handset yet;
/// that is why every reading below is optional and a shape that does not fit
/// produces "not reported" rather than a number (W5).
fn engmd_groups(lines: &[String]) -> Option<Vec<Vec<String>>> {
    let line = lines
        .iter()
        .find(|l| l.to_ascii_uppercase().contains("SPENGMD"))?;
    let payload = line
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(line.as_str());
    let payload = payload.split("OK").next().unwrap_or(payload);
    let fixed = payload.replace(",-", ",+").replace("--", "-+");
    let groups: Vec<Vec<String>> = fixed
        .split('-')
        .map(|group| group.split(',').map(|v| v.trim().to_string()).collect())
        .filter(|group: &Vec<String>| group.first().is_some_and(|v| !v.is_empty()))
        .collect();
    if groups.is_empty() {
        None
    } else {
        Some(groups)
    }
}

/// One field per group, under whichever of the two shapes the answer is in.
///
/// The helper indexes the split as a list of fields, so the answer is either
/// one group per field (the dash form) or -- when it carries no separator at
/// all -- one comma list of fields (the flat form).  Those are the only two
/// readings accepted; anything else is `None`, because a partial index into an
/// unread shape is exactly how an invented RSRP gets onto a screen.
fn engmd_view(lines: &[String]) -> Option<Vec<Vec<String>>> {
    let groups = engmd_groups(lines)?;
    match groups.len() {
        // The dash form: the fields we care about are at their own indices.
        n if n >= 5 => Some(groups),
        // The flat form: the same indices, into the comma list.
        1 if groups[0].len() >= 5 => Some(groups[0].iter().map(|v| vec![v.clone()]).collect()),
        _ => None,
    }
}

/// Field `i` of the answer, with the restored sign.  `+` is how the CP's
/// mangled minus arrives, so it is put back before the number is read.
fn engmd_field(groups: &[Vec<String>], i: usize) -> Option<String> {
    groups
        .get(i)?
        .first()
        .map(|v| v.replace('+', "-"))
        .filter(|v| !v.is_empty())
}

fn number_i64(value: Option<String>) -> Option<i64> {
    value?.trim().parse().ok()
}

/// Hundredths of a unit, which is how this CP reports RSRP/RSRQ/SINR.
fn hundredths(value: Option<String>) -> Option<f64> {
    number_i64(value).map(|v| v as f64 / 100.0)
}

/// An LTE bandwidth code, as 27.007-ish tables number them on this generation.
fn lte_bandwidth(code: &str) -> Option<String> {
    Some(
        match code.trim().parse::<u32>().ok()? {
            0 => "1.4M",
            1 => "3M",
            2 => "5M",
            3 => "10M",
            4 => "15M",
            5 => "20M",
            _ => return None,
        }
        .to_string(),
    )
}

/// `AT+SPENGMD=0,6,0`: band, EARFCN, PCI, RSRP, RSRQ, …, bandwidth, …, enb, cell.
///
/// `None` when the answer does not carry a cell at all -- which is the honest
/// outcome on a CP that refuses or renumbers this sub-command, and is what the
/// caller must report instead of a guess.
pub fn parse_lte_serving(lines: &[String]) -> Option<Serving> {
    let fields = engmd_view(lines)?;
    let serving = Serving {
        band: engmd_field(&fields, 0),
        earfcn: number_i64(engmd_field(&fields, 1)).and_then(|v| u32::try_from(v).ok()),
        pci: number_i64(engmd_field(&fields, 2)).and_then(|v| u32::try_from(v).ok()),
        rsrp: hundredths(engmd_field(&fields, 3)),
        rsrq: hundredths(engmd_field(&fields, 4)),
        sinr: None,
        bandwidth: engmd_field(&fields, 7).and_then(|c| lte_bandwidth(&c)),
        cell: engmd_field(&fields, 11),
    };
    serving.is_cell().then_some(serving)
}

/// `AT+SPENGMD=0,14,1`: band, EARFCN, PCI, RSRP, RSRQ, …, bandwidth, gNB, cell,
/// …, SINR.
pub fn parse_nr_serving(lines: &[String]) -> Option<Serving> {
    let fields = engmd_view(lines)?;
    let serving = Serving {
        band: engmd_field(&fields, 0),
        earfcn: number_i64(engmd_field(&fields, 1)).and_then(|v| u32::try_from(v).ok()),
        pci: number_i64(engmd_field(&fields, 2)).and_then(|v| u32::try_from(v).ok()),
        rsrp: hundredths(engmd_field(&fields, 3)),
        rsrq: hundredths(engmd_field(&fields, 4)),
        sinr: hundredths(engmd_field(&fields, 15)),
        // The NR width field's units are not measured on this generation, so
        // it is carried verbatim rather than dressed up as a bandwidth.
        bandwidth: engmd_field(&fields, 7),
        cell: engmd_field(&fields, 9),
    };
    serving.is_cell().then_some(serving)
}

/// `AT+SPENGMD=0,6,6`: one `earfcn,pci,rsrp,rsrq` record per neighbour, as
/// groups.  The all-zero record the CP pads the list with is dropped, and so
/// is any record too short to be a cell.
pub fn parse_lte_neighbors(lines: &[String]) -> Vec<Neighbor> {
    let Some(groups) = engmd_groups(lines) else {
        return Vec::new();
    };
    groups
        .iter()
        .filter_map(|group| {
            if group.len() < 4 {
                return None;
            }
            let earfcn = group[0].parse::<u32>().ok()?;
            let pci = group[1].parse::<u32>().ok()?;
            let rsrp = hundredths(Some(group[2].replace('+', "-")))?;
            let rsrq = hundredths(Some(group[3].replace('+', "-")))?;
            if earfcn == 0 && pci == 0 && rsrp == 0.0 && rsrq == 0.0 {
                return None;
            }
            Some(Neighbor {
                band: lte_band_from_earfcn(earfcn).map(|b| b.to_string()),
                earfcn,
                pci,
                rsrp,
                rsrq,
                sinr: None,
            })
        })
        .collect()
}

/// `AT+SPENGMD=0,14,2`: the NR neighbour list arrives column-wise — one group
/// per field, every group a comma list of the same length.
pub fn parse_nr_neighbors(lines: &[String]) -> Vec<Neighbor> {
    let Some(groups) = engmd_groups(lines) else {
        return Vec::new();
    };
    let column = |i: usize| -> Vec<String> { groups.get(i).cloned().unwrap_or_default() };
    let (bands, arfcns, pcis, rsrps, rsrqs, sinrs) = (
        column(0),
        column(1),
        column(2),
        column(3),
        column(4),
        column(5),
    );
    let count = [&bands, &arfcns, &pcis, &rsrps, &rsrqs, &sinrs]
        .iter()
        .map(|c| c.len())
        .min()
        .unwrap_or(0);
    (0..count)
        .filter_map(|i| {
            let earfcn = arfcns[i].parse::<u32>().ok()?;
            let pci = pcis[i].parse::<u32>().ok()?;
            let rsrp = hundredths(Some(rsrps[i].replace('+', "-")))?;
            let rsrq = hundredths(Some(rsrqs[i].replace('+', "-")))?;
            if earfcn == 0 && pci == 0 && rsrp == 0.0 && rsrq == 0.0 {
                return None;
            }
            Some(Neighbor {
                band: Some(bands[i].replace('+', "-")).filter(|b| !b.is_empty()),
                earfcn,
                pci,
                rsrp,
                rsrq,
                sinr: hundredths(Some(sinrs[i].replace('+', "-"))),
            })
        })
        .collect()
}

/// The LTE band an EARFCN belongs to, by the 36.101 ranges.  A frequency the
/// table does not cover is `None`: the neighbour is still reported, its band
/// is not invented.
pub fn lte_band_from_earfcn(earfcn: u32) -> Option<u32> {
    Some(match earfcn {
        0..=599 => 1,
        600..=1199 => 2,
        1200..=1949 => 3,
        1950..=2399 => 4,
        2400..=2649 => 5,
        2650..=2749 => 6,
        2750..=3449 => 7,
        3450..=3799 => 8,
        3800..=4149 => 9,
        4150..=4749 => 10,
        4750..=4949 => 11,
        5010..=5179 => 12,
        5180..=5279 => 13,
        5280..=5379 => 14,
        5730..=5849 => 17,
        5850..=5999 => 18,
        6000..=6149 => 19,
        6150..=6449 => 20,
        6450..=6599 => 21,
        6600..=7399 => 22,
        7500..=7699 => 23,
        7700..=8039 => 24,
        8040..=8689 => 25,
        8690..=9039 => 26,
        9040..=9209 => 27,
        9210..=9659 => 28,
        9660..=9769 => 29,
        9770..=9869 => 30,
        9870..=9919 => 31,
        9920..=10359 => 32,
        36000..=36199 => 33,
        36200..=36349 => 34,
        36350..=36949 => 35,
        36950..=37549 => 36,
        37550..=37749 => 37,
        37750..=38249 => 38,
        38250..=38649 => 39,
        38650..=39649 => 40,
        39650..=41589 => 41,
        41590..=43589 => 42,
        43590..=45589 => 43,
        45590..=46589 => 44,
        46590..=46789 => 45,
        46790..=54539 => 46,
        54540..=55239 => 47,
        55240..=56739 => 48,
        56740..=58239 => 49,
        58240..=59089 => 50,
        59090..=59139 => 51,
        59140..=60139 => 52,
        65536..=66435 => 65,
        66436..=67335 => 66,
        67336..=67535 => 67,
        67536..=67835 => 68,
        67836..=68335 => 69,
        68336..=68585 => 70,
        68586..=68935 => 71,
        68936..=68985 => 72,
        68986..=69035 => 73,
        69036..=69465 => 74,
        69466..=70315 => 75,
        70316..=70365 => 76,
        70366..=70545 => 85,
        70546..=70595 => 87,
        70596..=70645 => 88,
        _ => return None,
    })
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

    #[test]
    fn ims_reg_reads_the_state_field_not_the_reporting_flag() {
        assert_eq!(parse_ims_reg("+CIREG: 0,1"), Some(1));
        assert_eq!(parse_ims_reg("+CIREG: 0,0"), Some(0));
        // a one-field answer carries no state to read
        assert_eq!(parse_ims_reg("+CIREG: 0"), None);
    }
}

/// The `AT+SPENGMD` readings.
///
/// These tests pin the *hypothesis* the parser is built on -- the reading the
/// Android-side helper for this generation implies -- and not a measurement on
/// this handset: no answer from this CP has been captured yet, which is why
/// every one of these functions is allowed to return "nothing read".  The
/// samples below are constructed to that hypothesis, with placeholder values.
#[cfg(test)]
mod engmd_tests {
    use super::*;

    fn lines(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn serving_reads_the_dash_form() {
        // one field per group
        let answer = lines(&["+SPENGMD: 3-1650-88-+8500-+1000-0-0-5-0-0-12345-67890"]);
        let s = parse_lte_serving(&answer).expect("a cell");
        assert_eq!(s.band.as_deref(), Some("3"));
        assert_eq!(s.earfcn, Some(1650));
        assert_eq!(s.pci, Some(88));
        assert_eq!(s.rsrp, Some(-85.0));
        assert_eq!(s.rsrq, Some(-10.0));
        assert_eq!(s.bandwidth.as_deref(), Some("20M"));
        assert_eq!(s.cell.as_deref(), Some("67890"));
        assert_eq!(s.sinr, None, "LTE serving has no SINR field here");
    }

    #[test]
    fn serving_reads_the_flat_form() {
        // the same fields as one comma list, which is the other shape the
        // helper's indexing implies
        let answer = lines(&["+SPENGMD: 3,1650,88,+8500,+1000,0,0,5,0,0,12345,67890", "OK"]);
        let s = parse_lte_serving(&answer).expect("a cell");
        assert_eq!(s.band.as_deref(), Some("3"));
        assert_eq!(s.earfcn, Some(1650));
        assert_eq!(s.pci, Some(88));
        assert_eq!(s.rsrp, Some(-85.0));
        assert_eq!(s.rsrq, Some(-10.0));
        assert_eq!(s.bandwidth.as_deref(), Some("20M"));
    }

    #[test]
    fn nr_serving_reads_its_own_field_positions() {
        let mut fields = vec!["78"; 16];
        fields[1] = "627264";
        fields[2] = "5";
        fields[3] = "+9500";
        fields[4] = "+1200";
        fields[7] = "100";
        fields[9] = "4321";
        fields[15] = "+1500";
        let answer = lines(&[&format!("+SPENGMD: {}", fields.join("-"))]);
        let s = parse_nr_serving(&answer).expect("a cell");
        assert_eq!(s.band.as_deref(), Some("78"));
        assert_eq!(s.earfcn, Some(627264));
        assert_eq!(s.pci, Some(5));
        assert_eq!(s.rsrp, Some(-95.0));
        assert_eq!(s.rsrq, Some(-12.0));
        assert_eq!(s.sinr, Some(-15.0));
        assert_eq!(s.bandwidth.as_deref(), Some("100"));
        assert_eq!(s.cell.as_deref(), Some("4321"));
    }

    /// The one that matters: an answer this build cannot read must read as
    /// nothing, not as a cell at zero.
    #[test]
    fn an_unreadable_answer_reads_as_nothing() {
        assert_eq!(parse_lte_serving(&lines(&["ERROR"])), None);
        assert_eq!(parse_lte_serving(&lines(&["+SPENGMD: 0,0"])), None);
        assert_eq!(parse_lte_serving(&lines(&["+SPENGMD: 0-0-0-0-0"])), None);
        assert_eq!(parse_lte_serving(&lines(&[])), None);
        assert_eq!(parse_nr_serving(&lines(&["ERROR"])), None);
        // and a serving cell with EARFCN 0 is "not camped", not "band 1"
        assert_eq!(parse_lte_serving(&lines(&["+SPENGMD: 1,0,88,+8500,+1000"])), None);
    }

    #[test]
    fn lte_neighbours_are_one_record_per_group() {
        let answer = lines(&[
            "+SPENGMD: 1650,88,+9500,+1200-3000,7,+8800,+900-0,0,0,0",
            "OK",
        ]);
        let cells = parse_lte_neighbors(&answer);
        assert_eq!(cells.len(), 2, "the all-zero padding record is not a cell");
        assert_eq!(cells[0].band.as_deref(), Some("3"), "EARFCN 1650 is band 3");
        assert_eq!((cells[0].earfcn, cells[0].pci), (1650, 88));
        assert_eq!(cells[0].rsrp, -95.0);
        assert_eq!(cells[0].rsrq, -12.0);
        assert_eq!(cells[1].band.as_deref(), Some("7"), "EARFCN 3000 is band 7");
        assert_eq!((cells[1].earfcn, cells[1].pci), (3000, 7));
    }

    #[test]
    fn nr_neighbours_arrive_column_wise() {
        let answer = lines(&[
            "+SPENGMD: 78,41-627264,650000-5,6-+9500,+8800-+1200,+900-100,120",
        ]);
        let cells = parse_nr_neighbors(&answer);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].band.as_deref(), Some("78"));
        assert_eq!((cells[0].earfcn, cells[0].pci), (627264, 5));
        assert_eq!((cells[0].rsrp, cells[0].rsrq), (-95.0, -12.0));
        assert_eq!(cells[0].sinr, Some(1.0));
        assert_eq!(cells[1].band.as_deref(), Some("41"));
        assert_eq!((cells[1].earfcn, cells[1].pci), (650000, 6));
    }

    #[test]
    fn no_neighbour_answer_is_an_empty_list() {
        assert!(parse_lte_neighbors(&lines(&["ERROR"])).is_empty());
        assert!(parse_nr_neighbors(&lines(&["+SPENGMD: 0-0-0-0-0-0"])).is_empty());
    }

    #[test]
    fn earfcn_maps_to_a_band_where_the_table_covers_it() {
        assert_eq!(lte_band_from_earfcn(1650), Some(3));
        assert_eq!(lte_band_from_earfcn(3000), Some(7));
        assert_eq!(lte_band_from_earfcn(38675), Some(40));
        // outside every documented range: reported as unknown, not guessed
        assert_eq!(lte_band_from_earfcn(999_999), None);
    }

    #[test]
    fn the_probe_commands_are_the_ones_the_helper_uses() {
        assert_eq!(engmd(ENGMD_LTE_SERVING.0, ENGMD_LTE_SERVING.1), "AT+SPENGMD=0,6,0");
        assert_eq!(
            engmd(ENGMD_LTE_NEIGHBORS.0, ENGMD_LTE_NEIGHBORS.1),
            "AT+SPENGMD=0,6,6"
        );
        assert_eq!(engmd(ENGMD_NR_SERVING.0, ENGMD_NR_SERVING.1), "AT+SPENGMD=0,14,1");
        assert_eq!(
            engmd(ENGMD_NR_NEIGHBORS.0, ENGMD_NR_NEIGHBORS.1),
            "AT+SPENGMD=0,14,2"
        );
    }
}
