//! The per-run context: the loaded profile, the mode switch, the owned channel
//! set, the AT session, and the baseline the run summary is measured against.

use crate::at::AtSession;
use crate::channel::{ChannelMetrics, SerialChannel};
use crate::probes::{self, CounterDelta, NetCounters};
use crate::profile::Profile;
use crate::telemetry::{iso8601, Event, RunSummary, UrcStats};
use anyhow::{Context as _, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The vendor daemon still does the work; we only observe and record.
    Vendor,
    /// Ours: we own the channel and drive the modem.
    Native,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "vendor" => Ok(Mode::Vendor),
            "native" => Ok(Mode::Native),
            other => anyhow::bail!("unknown mode {other:?}: expected vendor or native"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Vendor => "vendor",
            Mode::Native => "native",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Baseline {
    mailbox_irq: Option<i64>,
    cp_asserts: Option<i64>,
    net: Option<NetCounters>,
}

pub struct Context {
    pub profile: Profile,
    pub mode: Mode,
    pub runs_dir: PathBuf,
    pub verbose: bool,
    pub argv: Vec<String>,
    pub events: Vec<Event>,
    pub notes: Vec<String>,
    channels: BTreeMap<String, Arc<SerialChannel>>,
    session: Option<Arc<AtSession>>,
    started_at: f64,
    baseline: Baseline,
}

impl Context {
    pub fn new(
        profile: Profile,
        mode: Mode,
        runs_dir: PathBuf,
        lock_dir: PathBuf,
        verbose: bool,
        argv: Vec<String>,
    ) -> Self {
        let baseline = Baseline {
            mailbox_irq: probes::mailbox_irq_count(&profile.mailbox.irq_match).map(|v| v as i64),
            cp_asserts: probes::kernel_log_matches(&profile.telemetry.assert_pattern)
                .map(|v| v as i64),
            net: profile
                .data
                .interface(None)
                .as_deref()
                .and_then(probes::net_counters),
        };
        let mut channels = BTreeMap::new();
        channels.insert(
            "cmd".to_string(),
            Arc::new(SerialChannel::new(
                profile.channels.cmd.clone(),
                "cmd",
                lock_dir.clone(),
                Duration::from_secs_f64(profile.at.reopen_backoff.max(0.0)),
                true,
            )),
        );
        if let Some(urc) = &profile.channels.urc {
            channels.insert(
                "urc".to_string(),
                Arc::new(SerialChannel::new(
                    urc.clone(),
                    "urc",
                    lock_dir.clone(),
                    Duration::from_secs_f64(profile.at.reopen_backoff.max(0.0)),
                    true,
                )),
            );
        }
        Self {
            profile,
            mode,
            runs_dir,
            verbose,
            argv,
            events: Vec::new(),
            notes: Vec::new(),
            channels,
            session: None,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            baseline,
        }
    }

    /// Open the AT channels and return the session.  Idempotent.
    ///
    /// This is where "never two readers on one AT channel" is enforced: the
    /// second owner gets `ChannelBusy` and the caller must not retry blindly.
    pub fn at(&mut self) -> Result<Arc<AtSession>> {
        if let Some(s) = &self.session {
            return Ok(Arc::clone(s));
        }
        for (name, ch) in &self.channels {
            ch.open().with_context(|| format!("opening {name} channel"))?;
        }
        let cmd = Arc::clone(self.channels.get("cmd").expect("cmd channel"));
        let urc = self.channels.get("urc").cloned();
        let session = Arc::new(AtSession::new(cmd, urc, self.profile.at.clone()));
        session.start_urc_pump();
        self.session = Some(Arc::clone(&session));
        Ok(session)
    }

    pub fn channel(&self, name: &str) -> Option<&Arc<SerialChannel>> {
        self.channels.get(name)
    }

    pub fn session(&self) -> Option<&Arc<AtSession>> {
        self.session.as_ref()
    }

    pub fn event(&mut self, kind: &str, detail: impl Into<String>) {
        self.events.push(Event::new(kind, detail));
    }

    pub fn note(&mut self, detail: impl Into<String>) {
        self.notes.push(detail.into());
    }

    pub fn verbose(&self, msg: impl AsRef<str>) {
        if self.verbose {
            eprintln!("unisoc-cpd: {}", msg.as_ref());
        }
    }

    /// Sample everything after the run and build the summary.
    pub fn finish(&self, capability: &str, status: &str, exit_code: i32) -> RunSummary {
        let ended = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let mut channels: BTreeMap<String, ChannelMetrics> = BTreeMap::new();
        for (name, ch) in &self.channels {
            channels.insert(name.clone(), ch.metrics());
        }

        let at = self.session.as_ref().map(|s| s.metrics()).unwrap_or_default();
        let urc_tail = self
            .session
            .as_ref()
            .map(|s| s.urc_tail(20))
            .unwrap_or_default();

        let mailbox_after = probes::mailbox_irq_count(&self.profile.mailbox.irq_match)
            .map(|v| v as i64);
        let asserts_after = probes::kernel_log_matches(&self.profile.telemetry.assert_pattern)
            .map(|v| v as i64);
        let net_after = self
            .profile
            .data
            .interface(None)
            .as_deref()
            .and_then(probes::net_counters);

        let net_delta = match (&self.baseline.net, &net_after) {
            (Some(b), Some(a)) => Some(CounterDelta::between(b.rx_bytes_i(), a.rx_bytes_i())),
            _ => None,
        };

        RunSummary {
            tool: "unisoc-cpd".into(),
            version: VERSION.into(),
            platform: self.profile.platform_id().to_string(),
            generation: self.profile.generation.clone(),
            profile: self.profile.path.as_ref().map(|p| p.display().to_string()),
            mode: self.mode.as_str().into(),
            capability: capability.into(),
            argv: self.argv.clone(),
            started_at: iso8601(self.started_at),
            ended_at: iso8601(ended),
            duration_s: ended - self.started_at,
            status: status.into(),
            exit_code,
            at,
            channels,
            urc: UrcStats {
                lines: self.session.as_ref().map(|s| s.metrics().urc_lines).unwrap_or(0),
                max_gap_s: self.session.as_ref().map(|s| s.metrics().max_urc_gap_s).unwrap_or(0.0),
                gaps_over_threshold: self
                    .session
                    .as_ref()
                    .map(|s| s.metrics().urc_gaps_over_threshold)
                    .unwrap_or(0),
                tail: urc_tail,
            },
            mailbox_irq: Some(CounterDelta::between(self.baseline.mailbox_irq, mailbox_after)),
            cp_asserts: Some(CounterDelta::between(self.baseline.cp_asserts, asserts_after)),
            net: net_after,
            net_delta,
            events: self.events.clone(),
            notes: self.notes.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_both_spellings() {
        assert_eq!(Mode::parse("vendor").unwrap(), Mode::Vendor);
        assert_eq!(Mode::parse("NATIVE").unwrap(), Mode::Native);
        assert!(Mode::parse("mixed").is_err());
    }
}
