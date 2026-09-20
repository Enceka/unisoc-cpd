//! Per-run telemetry.
//!
//! Every run writes one JSON file plus an append-only `summary.json`, so a
//! `vendor` run and a `native` run of the same capability on two platforms are
//! directly comparable — which is the whole point of the acceptance matrix.

use crate::at::AtMetrics;
use crate::channel::ChannelMetrics;
use crate::probes::{CounterDelta, NetCounters};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub at: String,
    pub kind: String,
    pub detail: String,
}

impl Event {
    pub fn new(kind: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            at: iso8601(now_secs()),
            kind: kind.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UrcStats {
    pub lines: u64,
    pub max_gap_s: f64,
    pub gaps_over_threshold: u64,
    pub tail: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub tool: String,
    pub version: String,
    pub platform: String,
    pub generation: String,
    pub profile: Option<String>,
    pub mode: String,
    pub capability: String,
    pub argv: Vec<String>,
    pub started_at: String,
    pub ended_at: String,
    pub duration_s: f64,
    pub status: String,
    pub exit_code: i32,
    pub at: AtMetrics,
    pub channels: BTreeMap<String, ChannelMetrics>,
    pub urc: UrcStats,
    pub mailbox_irq: Option<CounterDelta>,
    pub cp_asserts: Option<CounterDelta>,
    pub net: Option<NetCounters>,
    pub net_delta: Option<CounterDelta>,
    pub events: Vec<Event>,
    pub notes: Vec<String>,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Days since 1970-01-01 -> civil (year, month, day), Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// RFC 3339 in UTC, without pulling a date library into a 72 h daemon.
pub fn iso8601(secs: f64) -> String {
    let total = secs.floor() as i64;
    let millis = ((secs - secs.floor()) * 1000.0).round() as i64;
    let days = total.div_euclid(86_400);
    let sod = total.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        millis
    )
}

impl RunSummary {
    pub fn date_dir(&self) -> String {
        self.started_at[..10].to_string()
    }

    /// `runs/<platform>/<date>/<capability>.<mode>.<HHMMSS>.json`, and the same
    /// record appended to `runs/<platform>/<date>/summary.json`.
    pub fn write(&self, runs_root: &Path) -> Result<PathBuf> {
        let day = self.date_dir();
        let dir = runs_root.join(&self.platform).join(&day);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create run directory {}", dir.display()))?;

        let hhmmss = self.started_at[11..19].replace(':', "");
        let file = dir.join(format!("{}.{}.{}.json", self.capability, self.mode, hhmmss));
        std::fs::write(&file, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("cannot write {}", file.display()))?;

        let index = dir.join("summary.json");
        let mut records: Vec<serde_json::Value> = if index.is_file() {
            let text = std::fs::read_to_string(&index).unwrap_or_default();
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            Vec::new()
        };
        records.push(serde_json::to_value(self)?);
        std::fs::write(&index, serde_json::to_string_pretty(&records)?)
            .with_context(|| format!("cannot write {}", index.display()))?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_formats_known_instants() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00.000Z");
        // 2026-09-20T00:00:00Z
        assert_eq!(iso8601(1_789_862_400.0), "2026-09-20T00:00:00.000Z");
        assert_eq!(iso8601(1_789_862_400.5), "2026-09-20T00:00:00.500Z");
    }

    #[test]
    fn civil_from_days_handles_leap_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
