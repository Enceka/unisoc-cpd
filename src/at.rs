//! AT layer: codec, URC demultiplexer, serialisation, pacing, retry, timeouts.
//!
//! The hard part is not sending `AT+CSQ`; it is deciding, for every line that
//! arrives, whether it belongs to the command in flight or to the modem's
//! unsolicited stream.  Getting that wrong is what made an earlier port read a
//! queued `+CGEV:` as the answer to `+CEREG?`.
//!
//! The rule here: a `+XXX:` line is a *response* only if the command in flight
//! could have produced it (the prefix is inferred from the command, or declared
//! by the caller) **and** it is not one of the generation's URC prefixes.
//! Everything else on the wire while a command is in flight is a URC and is
//! routed, never lost.

use crate::channel::SerialChannel;
use crate::profile::AtOptions;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalCode {
    Ok,
    Error,
    CmeError(String),
    CmsError(String),
    Connect(String),
    NoCarrier,
    Busy,
    NoAnswer,
    NoDialtone,
    /// No final result code arrived before the deadline.
    Timeout,
    /// The channel is gone.
    ChannelDown,
}

impl FinalCode {
    pub fn is_ok(&self) -> bool {
        matches!(self, FinalCode::Ok | FinalCode::Connect(_))
    }

    /// A final result code that the modem actually sent (not our own timeout).
    pub fn from_modem(&self) -> bool {
        !matches!(self, FinalCode::Timeout | FinalCode::ChannelDown)
    }

    pub fn as_str(&self) -> String {
        match self {
            FinalCode::Ok => "OK".into(),
            FinalCode::Error => "ERROR".into(),
            FinalCode::CmeError(t) => format!("+CME ERROR{t}"),
            FinalCode::CmsError(t) => format!("+CMS ERROR{t}"),
            FinalCode::Connect(t) => format!("CONNECT{t}"),
            FinalCode::NoCarrier => "NO CARRIER".into(),
            FinalCode::Busy => "BUSY".into(),
            FinalCode::NoAnswer => "NO ANSWER".into(),
            FinalCode::NoDialtone => "NO DIALTONE".into(),
            FinalCode::Timeout => "TIMEOUT".into(),
            FinalCode::ChannelDown => "CHANNEL DOWN".into(),
        }
    }
}

/// Does this line end the command?
pub fn classify(line: &str) -> Option<FinalCode> {
    let l = line.trim();
    if l == "OK" {
        return Some(FinalCode::Ok);
    }
    if l == "ERROR" {
        return Some(FinalCode::Error);
    }
    if let Some(rest) = l.strip_prefix("+CME ERROR") {
        return Some(FinalCode::CmeError(rest.to_string()));
    }
    if let Some(rest) = l.strip_prefix("+CMS ERROR") {
        return Some(FinalCode::CmsError(rest.to_string()));
    }
    if let Some(rest) = l.strip_prefix("CONNECT") {
        return Some(FinalCode::Connect(rest.to_string()));
    }
    match l {
        "NO CARRIER" => Some(FinalCode::NoCarrier),
        "BUSY" => Some(FinalCode::Busy),
        "NO ANSWER" => Some(FinalCode::NoAnswer),
        "NO DIALTONE" => Some(FinalCode::NoDialtone),
        _ => None,
    }
}

/// `AT+CSQ` -> `+CSQ:`.  Used to tell a response from a URC.
pub fn infer_expect(cmd: &str) -> Vec<String> {
    let c = cmd.trim();
    let c = c.strip_prefix("AT").or_else(|| c.strip_prefix("at")).unwrap_or(c);
    let name: String = c
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '^' || *ch == '$' || *ch == '+')
        .collect();
    if name.is_empty() || !(name.starts_with('+') || name.starts_with('^')) {
        return Vec::new();
    }
    vec![format!("{name}:")]
}

fn looks_like_final(line: &str) -> bool {
    classify(line).is_some()
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AtMetrics {
    pub commands: u64,
    pub ok: u64,
    pub errors: u64,
    pub timeouts: u64,
    pub retries: u64,
    pub urc_lines: u64,
    pub response_lines: u64,
    pub probes: u64,
    pub probe_failures: u64,
    pub max_urc_gap_s: f64,
    pub urc_gaps_over_threshold: u64,
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub command: String,
    /// Response lines, in order, excluding URCs and the final result code.
    pub lines: Vec<String>,
    /// URCs that arrived while this command was in flight.
    pub urcs: Vec<String>,
    pub final_code: FinalCode,
    pub elapsed: Duration,
    pub attempts: u32,
}

impl Reply {
    pub fn ok(&self) -> bool {
        self.final_code.is_ok()
    }

    pub fn text(&self) -> String {
        let mut out = self.lines.clone();
        out.push(self.final_code.as_str());
        out.join("\n")
    }

    pub fn first_with_prefix(&self, prefix: &str) -> Option<&str> {
        self.lines.iter().find(|l| l.starts_with(prefix)).map(|s| s.as_str())
    }

    pub fn line_with<'a>(&'a self, needle: &str) -> Option<&'a str> {
        self.lines.iter().find(|l| l.contains(needle)).map(|s| s.as_str())
    }
}

pub struct AtSession {
    pub cmd: Arc<SerialChannel>,
    pub urc: Option<Arc<SerialChannel>>,
    opts: AtOptions,
    gate: Mutex<()>,
    last_write: Mutex<Instant>,
    metrics: Mutex<AtMetrics>,
    in_flight: AtomicBool,
    urc_tail: Mutex<VecDeque<String>>,
    last_urc_at: Mutex<Option<Instant>>,
    sink: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    pump_stop: Arc<AtomicBool>,
    pump: Mutex<Option<JoinHandle<()>>>,
}

impl AtSession {
    pub fn new(cmd: Arc<SerialChannel>, urc: Option<Arc<SerialChannel>>, opts: AtOptions) -> Self {
        Self {
            cmd,
            urc,
            opts,
            gate: Mutex::new(()),
            last_write: Mutex::new(Instant::now() - Duration::from_secs(3600)),
            metrics: Mutex::new(AtMetrics::default()),
            in_flight: AtomicBool::new(false),
            urc_tail: Mutex::new(VecDeque::new()),
            last_urc_at: Mutex::new(None),
            sink: Mutex::new(None),
            pump_stop: Arc::new(AtomicBool::new(false)),
            pump: Mutex::new(None),
        }
    }

    pub fn metrics(&self) -> AtMetrics {
        self.metrics.lock().unwrap().clone()
    }

    pub fn urc_tail(&self, n: usize) -> Vec<String> {
        let q = self.urc_tail.lock().unwrap();
        q.iter().rev().take(n).rev().cloned().collect()
    }

    /// Install a callback for every URC line (a spool writer, a test probe).
    pub fn set_urc_sink(&self, sink: Arc<dyn Fn(&str) + Send + Sync>) {
        *self.sink.lock().unwrap() = Some(sink);
    }

    fn record_urc(&self, line: &str) {
        let now = Instant::now();
        {
            let mut tail = self.urc_tail.lock().unwrap();
            if tail.len() >= 500 {
                tail.pop_front();
            }
            tail.push_back(line.to_string());
        }
        {
            let mut last = self.last_urc_at.lock().unwrap();
            if let Some(prev) = *last {
                let gap = now.duration_since(prev).as_secs_f64();
                let mut m = self.metrics.lock().unwrap();
                if gap > m.max_urc_gap_s {
                    m.max_urc_gap_s = gap;
                }
                if gap > self.opts.urc_gap_threshold_secs() {
                    m.urc_gaps_over_threshold += 1;
                }
            }
            *last = Some(now);
        }
        {
            let mut m = self.metrics.lock().unwrap();
            m.urc_lines += 1;
        }
        if let Some(cb) = self.sink.lock().unwrap().as_ref() {
            cb(line);
        }
    }

    fn is_urc(&self, line: &str, expect: &[String]) -> bool {
        if !(line.starts_with('+') || line.starts_with('^')) {
            return false;
        }
        if expect.iter().any(|p| line.starts_with(p.as_str())) {
            return false;
        }
        self.opts
            .urc_prefixes
            .iter()
            .any(|p| line.starts_with(p.as_str()))
    }

    fn pace_wait(&self) {
        let pace = Duration::from_secs_f64(self.opts.pace_seconds.max(0.0));
        if pace.is_zero() {
            return;
        }
        let elapsed = self.last_write.lock().unwrap().elapsed();
        if elapsed < pace {
            thread::sleep(pace - elapsed);
        }
    }

    /// Send one command and collect its reply.
    ///
    /// Serialised: exactly one command is in flight, ever.  An unexpected
    /// `+XXX:` line is routed as a URC instead of ending the reply.
    pub fn command(&self, cmd: &str, timeout: Duration, expect: &[String], retries: u32) -> Reply {
        let _gate = self.gate.lock().unwrap();
        let expect: Vec<String> = if expect.is_empty() {
            infer_expect(cmd)
        } else {
            let mut v = expect.to_vec();
            v.extend(infer_expect(cmd));
            v
        };

        let started = Instant::now();
        let mut attempts = 0;
        let mut reply = Reply {
            command: cmd.to_string(),
            lines: Vec::new(),
            urcs: Vec::new(),
            final_code: FinalCode::Timeout,
            elapsed: Duration::ZERO,
            attempts: 0,
        };

        {
            let mut m = self.metrics.lock().unwrap();
            m.commands += 1;
        }

        loop {
            attempts += 1;
            if attempts > retries + 1 {
                break;
            }
            if attempts > 1 {
                let mut m = self.metrics.lock().unwrap();
                m.retries += 1;
                drop(m);
                thread::sleep(Duration::from_millis(250));
            }

            if !self.cmd.healthy() && !self.cmd.ensure_open() {
                reply.final_code = FinalCode::ChannelDown;
                reply.attempts = attempts;
                break;
            }

            self.pace_wait();

            // Whatever is queued belongs to nobody: route it, do not read it
            // as this command's response.
            self.in_flight.store(true, Ordering::SeqCst);
            for line in self.cmd.drain(Duration::from_millis(200)) {
                if self.is_urc(&line, &expect) {
                    self.record_urc(&line);
                }
            }

            if let Err(e) = self.cmd.write_line(cmd) {
                self.in_flight.store(false, Ordering::SeqCst);
                reply.final_code = FinalCode::ChannelDown;
                reply.attempts = attempts;
                eprintln!("unisoc-cpd: {cmd}: {e}");
                break;
            }
            *self.last_write.lock().unwrap() = Instant::now();

            let deadline = Instant::now() + timeout;
            let mut lines: Vec<String> = Vec::new();
            let mut urcs: Vec<String> = Vec::new();
            let mut final_code: Option<FinalCode> = None;

            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let Some(line) = self.cmd.read_line(remaining.min(Duration::from_millis(500))) else {
                    if !self.cmd.healthy() {
                        final_code = Some(FinalCode::ChannelDown);
                        break;
                    }
                    continue;
                };
                if let Some(code) = classify(&line) {
                    if looks_like_final(&line) {
                        final_code = Some(code);
                        break;
                    }
                }
                if self.is_urc(&line, &expect) {
                    self.record_urc(&line);
                    urcs.push(line);
                } else {
                    lines.push(line);
                }
            }
            self.in_flight.store(false, Ordering::SeqCst);

            reply.lines = lines;
            reply.urcs = urcs;
            reply.attempts = attempts;
            match final_code {
                Some(code) => {
                    reply.final_code = code;
                    break;
                }
                None => {
                    reply.final_code = FinalCode::Timeout;
                    if attempts > retries {
                        break;
                    }
                }
            }
        }

        reply.elapsed = started.elapsed();
        {
            let mut m = self.metrics.lock().unwrap();
            match &reply.final_code {
                FinalCode::Timeout => m.timeouts += 1,
                c if c.is_ok() => m.ok += 1,
                FinalCode::ChannelDown => {}
                _ => m.errors += 1,
            }
            m.response_lines += reply.lines.len() as u64;
        }
        reply
    }

    /// A liveness probe: an ordinary command whose answer the soak keys on.
    ///
    /// Recorded separately from `commands`, because "how often did we ask the
    /// CP whether it is alive" is a different number from "how much AT did we
    /// send", and only the first one is allowed to drive a watchdog.
    pub fn probe_with(&self, cmd: &str, timeout: Duration) -> Reply {
        let r = self.command(cmd, timeout, &[], 0);
        let mut m = self.metrics.lock().unwrap();
        m.probes += 1;
        if !r.ok() {
            m.probe_failures += 1;
        }
        r
    }

    /// `AT` with the default timeout.
    pub fn probe(&self) -> bool {
        self.probe_with("AT", Duration::from_secs_f64(self.opts.default_timeout))
            .ok()
    }

    /// Commands that wait for a `>` continuation prompt (`AT+CMGS`).
    pub fn command_prompted(
        &self,
        cmd: &str,
        prompt_timeout: Duration,
        payload: &str,
        timeout: Duration,
    ) -> Reply {
        let _gate = self.gate.lock().unwrap();
        self.pace_wait();
        self.in_flight.store(true, Ordering::SeqCst);
        let mut urcs = Vec::new();
        for line in self.cmd.drain(Duration::from_millis(200)) {
            if self.is_urc(&line, &[]) {
                self.record_urc(&line);
            }
        }
        let started = Instant::now();
        let mut reply = Reply {
            command: cmd.to_string(),
            lines: Vec::new(),
            urcs: Vec::new(),
            final_code: FinalCode::Timeout,
            elapsed: Duration::ZERO,
            attempts: 1,
        };
        if let Err(e) = self.cmd.write_line(cmd) {
            self.in_flight.store(false, Ordering::SeqCst);
            reply.final_code = FinalCode::ChannelDown;
            reply.elapsed = started.elapsed();
            eprintln!("unisoc-cpd: {cmd}: {e}");
            return reply;
        }
        *self.last_write.lock().unwrap() = Instant::now();

        let prompt_deadline = Instant::now() + prompt_timeout;
        let mut saw_prompt = false;
        let mut lines = Vec::new();
        while Instant::now() < prompt_deadline {
            let remaining = prompt_deadline.saturating_duration_since(Instant::now());
            match self.cmd.read_line(remaining.min(Duration::from_millis(300))) {
                Some(l) if l.trim_end().ends_with('>') => {
                    saw_prompt = true;
                    break;
                }
                Some(l) => {
                    if let Some(code) = classify(&l) {
                        reply.final_code = code;
                        reply.lines = lines;
                        self.in_flight.store(false, Ordering::SeqCst);
                        reply.elapsed = started.elapsed();
                        return reply;
                    }
                    lines.push(l);
                }
                None => {}
            }
        }
        if saw_prompt {
            // A Ctrl-Z (0x1A) terminates the message body.
            let body = format!("{payload}\u{1a}");
            if let Err(e) = self.cmd.write_payload(&body) {
                eprintln!("unisoc-cpd: {cmd} payload: {e}");
            }
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.cmd.read_line(remaining.min(Duration::from_millis(500))) {
                    Some(l) => {
                        if let Some(code) = classify(&l) {
                            reply.final_code = code;
                            break;
                        }
                        if self.is_urc(&l, &[]) {
                            self.record_urc(&l);
                            urcs.push(l);
                        } else {
                            lines.push(l);
                        }
                    }
                    None => {}
                }
            }
        }
        self.in_flight.store(false, Ordering::SeqCst);
        reply.lines = lines;
        reply.urcs = urcs;
        reply.elapsed = started.elapsed();
        reply
    }

    /// The always-on drain.  Runs while a command is in flight too, but only
    /// touches the URC channel then, so it can never steal a response.
    pub fn start_urc_pump(self: &Arc<Self>) {
        let mut slot = self.pump.lock().unwrap();
        if slot.is_some() {
            return;
        }
        self.pump_stop.store(false, Ordering::SeqCst);
        let me = Arc::clone(self);
        let handle = thread::Builder::new()
            .name("urc-pump".into())
            .spawn(move || {
                while !me.pump_stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(200));
                    if me.in_flight.load(Ordering::SeqCst) {
                        continue;
                    }
                    let lines = match &me.urc {
                        Some(u) => u.take_pending(),
                        None => me.cmd.take_pending(),
                    };
                    for line in lines {
                        if line.starts_with('+') || line.starts_with('^') {
                            me.record_urc(&line);
                        }
                    }
                }
            })
            .expect("spawn urc pump");
        *slot = Some(handle);
    }

    pub fn stop_urc_pump(&self) {
        self.pump_stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.pump.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl Drop for AtSession {
    fn drop(&mut self) {
        self.pump_stop.store(true, Ordering::SeqCst);
    }
}

impl AtOptions {
    fn urc_gap_threshold_secs(&self) -> f64 {
        self.urc_gap_seconds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_codes_are_recognised() {
        assert_eq!(classify("OK"), Some(FinalCode::Ok));
        assert_eq!(classify("ERROR"), Some(FinalCode::Error));
        assert_eq!(classify("+CME ERROR: 3"), Some(FinalCode::CmeError(": 3".into())));
        assert_eq!(classify("NO CARRIER"), Some(FinalCode::NoCarrier));
        assert!(matches!(classify("CONNECT"), Some(FinalCode::Connect(_))));
        assert_eq!(classify("+CSQ: 20,99"), None);
        assert_eq!(classify("+CGEV: ME PDN ACT 1"), None);
    }

    #[test]
    fn expect_prefix_is_inferred() {
        assert_eq!(infer_expect("AT+CSQ"), vec!["+CSQ:".to_string()]);
        assert_eq!(infer_expect("AT+CEREG?"), vec!["+CEREG:".to_string()]);
        assert_eq!(infer_expect("AT+CGDCONT=1,\"IPV4V6\",\"x\""), vec!["+CGDCONT:".to_string()]);
        assert!(infer_expect("AT").is_empty());
        assert!(infer_expect("ATD123;").is_empty());
    }
}
