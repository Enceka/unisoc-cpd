//! The resident owner: the seat the RIL used to hold.
//!
//! Android's RIL opens `/dev/stty_nr1` and `/dev/stty_nr0` at start-up, holds
//! them for the lifetime of the boot, reads the unsolicited stream continuously,
//! and every other component asks *it* for the modem.  That is the shape G2 has
//! to reproduce, and not out of mimicry: the SIPC channel is a **queue** whose
//! two rules — one reader, never closed (`docs/BASEBAND-CONTRACTS.md` §2) — can
//! only be held by a process that stays alive.  A capability that opened the
//! channel per invocation would satisfy neither, and could not see a `+CMTI:`
//! that arrived while nobody was asking.
//!
//! So `serve` is the owner and the capabilities become its verbs: it holds the
//! session, decodes the unsolicited stream into events (`core/urc.rs`), probes
//! the CP when nothing else is happening, and answers requests on a unix socket.
//!
//! What is asked for over that socket is a **capability**, never a raw AT
//! string.  Raw AT over a socket would move the ownership rule out of the
//! process that enforces it, and the red line would become a convention again.

use super::control::{parse_cmgr, TextMessage};
use super::{flag_value, Capability, Outcome, Status};
use crate::at::AtMetrics;
use crate::channel::ChannelMetrics;
use crate::context::Context;
use crate::urc::{self, UrcEvent};
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where the socket lives when `--socket` is not given.
pub const DEFAULT_SOCKET: &str = "cmd.sock";

/// How many decoded URCs are kept for `state` and `urc` to answer with.
const KEEP_URCS: usize = 200;

/// How many decoded URCs `state` (and an `urc` request with no `limit`) returns.
const DEFAULT_EVENT_TAIL: usize = 20;

/// How many read messages are kept for `state` and `messages` to answer with.
const KEEP_MESSAGES: usize = 50;

pub struct Serve;

impl Capability for Serve {
    fn name(&self) -> &'static str {
        "serve"
    }

    fn summary(&self) -> &'static str {
        "resident owner (G2): hold the AT/URC channels for the whole boot and answer requests on a unix socket"
    }

    fn native_only(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let socket = match flag_value(args, "--socket") {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => ctx.state_dir.join(DEFAULT_SOCKET),
        };
        let seconds = match flag_value(args, "--seconds") {
            Some(v) => v
                .parse::<f64>()
                .with_context(|| format!("--seconds {v:?} is not a number of seconds"))?,
            // No deadline: the daemon's natural end is the end of the boot.
            None => 0.0,
        };
        serve(ctx, &socket, seconds)
    }
}

// ------------------------------------------------------------------ the owner

fn serve(ctx: &mut Context, socket: &Path, seconds: f64) -> Result<Outcome> {
    // A daemon that is already serving is refused *first*: an owner that holds
    // the channel must not even see the next one reach for the lock.
    let listener = bind(socket)?;
    let _guard = SocketGuard(socket.to_path_buf());
    listener.set_nonblocking(true)?;

    // The one owner.  A second process that got this far is refused here, with
    // `ChannelBusy` and exit code 3 — loudly, before anything is written.
    let session = ctx.at()?;

    let log = Arc::new(UrcLog::default());
    let inbox = Arc::new(MessageInbox::default());

    // Text mode is what makes a `+CMTI:` worth acting on: without it `CMGR`
    // hands back PDU hex, which this daemon does not decode and will not
    // pretend to.
    let default_timeout = Duration::from_secs_f64(ctx.profile.at.default_timeout.max(1.0));
    let surface = configure_sms_surface(&session, default_timeout);
    inbox.text_mode.store(surface.text_mode, Ordering::SeqCst);
    inbox
        .charset_gsm
        .store(surface.charset_gsm, Ordering::SeqCst);
    inbox
        .mt_indication
        .store(surface.mt_indication, Ordering::SeqCst);
    if !surface.text_mode {
        eprintln!(
            "unisoc-cpd: AT+CMGF=1 was not accepted; incoming messages are \
             announced but not read"
        );
    }
    if !surface.mt_indication {
        eprintln!(
            "unisoc-cpd: +CMTI could not be enabled; new messages will sit in \
             storage unannounced"
        );
    }

    {
        let log = Arc::clone(&log);
        let inbox = Arc::clone(&inbox);
        session.set_urc_sink(Arc::new(move |line: &str| {
            log.record(line);
            if let Some(urc::Urc::NewMessage { storage, index }) = urc::classify(line) {
                inbox.announced(&storage, index);
            }
        }));
    }

    let mut rt = Runtime::new(socket);
    let idle = Duration::from_secs_f64(ctx.profile.at.idle_probe_seconds.max(1.0));
    let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));

    loop {
        if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
            break;
        }

        match listener.accept() {
            Ok((conn, _)) => {
                rt.requests += 1;
                let mark = log.mark();
                let answered_before = answered(ctx);
                if let Err(e) = handle(ctx, &log, &inbox, &mut rt, mark, conn) {
                    eprintln!("unisoc-cpd: connection: {e:#}");
                }
                if answered(ctx) > answered_before {
                    rt.last_ok = Some(Instant::now());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("unisoc-cpd: accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }

        read_announced(&inbox, &session, default_timeout);

        // A probe is a real command, not a liveness ping: "is the CP still
        // answering AT" is the question, and the only one allowed to drive a
        // watchdog (the plan's A1; `at.rs` counts probes separately).
        if rt.last_activity.elapsed() >= idle {
            rt.last_activity = Instant::now();
            rt.probes += 1;
            if session.probe() {
                rt.last_ok = Some(Instant::now());
            } else {
                rt.probe_failures += 1;
                eprintln!(
                    "unisoc-cpd: idle probe on {} failed ({} so far)",
                    socket.display(),
                    rt.probe_failures
                );
            }
        }
    }

    let lines = log.lines.load(Ordering::SeqCst);
    let decoded = log.decoded.load(Ordering::SeqCst);
    Ok(Outcome::pass(vec![
        format!("served on {} for {:.1} s", socket.display(), rt.uptime()),
        format!("requests {}", rt.requests),
        format!("idle probes {} ({} failed)", rt.probes, rt.probe_failures),
        format!("URC lines {lines}, decoded {decoded}"),
        format!(
            "messages announced {}, read {} ({} unparsed)",
            inbox.announced.load(Ordering::SeqCst),
            inbox.reads.load(Ordering::SeqCst),
            inbox.read_failures.load(Ordering::SeqCst)
        ),
    ]))
}

/// The MT half: read every message the modem announced, between requests.
///
/// This runs on the same single thread that answers requests, which is the
/// whole trick: a `+CMTI:` may arrive *inside* a command's reply loop (the
/// URC sink fires there too), and the one thing it must never do is send AT
/// from inside that loop — the session's gate is held by the command in
/// flight.  So the sink only queues, and the queue is drained here, where
/// taking the gate is safe.
fn read_announced(inbox: &MessageInbox, session: &crate::at::AtSession, timeout: Duration) {
    let text_mode = inbox.text_mode.load(Ordering::SeqCst);
    for (storage, index) in inbox.drain() {
        let cmd = format!("AT+CMGR={index}");
        if !text_mode {
            eprintln!("unisoc-cpd: {storage}[{index}] announced but text mode is off; not read");
            continue;
        }
        inbox.reads.fetch_add(1, Ordering::SeqCst);
        let reply = session.command(&cmd, timeout, &["+CMGR:".to_string()], 0);
        match parse_cmgr(index, &reply.lines) {
            Some(mut message) => {
                message.storage = storage.clone();
                eprintln!(
                    "unisoc-cpd: message from {} in {}[{}]: {}",
                    message.from, message.storage, message.index, message.text
                );
                inbox.deliver(message);
            }
            None => {
                inbox.read_failures.fetch_add(1, Ordering::SeqCst);
                eprintln!(
                    "unisoc-cpd: the reply to {cmd} is not a text-mode message: {:?}",
                    reply.lines
                );
            }
        }
    }
}

/// Bind the socket, refusing to steal one that a live daemon is serving.
fn bind(socket: &Path) -> Result<UnixListener> {
    if let Some(parent) = socket.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
    }
    if socket.exists() {
        // A stale file is normal (SIGTERM does not unwind, so the guard that
        // removes the socket never runs); a file something is *listening* on is
        // a second daemon.  Connecting is the only honest way to tell them
        // apart, and two daemons on one socket would be two owners of the
        // channel.
        if UnixStream::connect(socket).is_ok() {
            bail!(
                "another daemon is already serving on {}; stop it first",
                socket.display()
            );
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("cannot remove the stale socket {}", socket.display()))?;
    }
    UnixListener::bind(socket).with_context(|| format!("cannot listen on {}", socket.display()))
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn answered(ctx: &Context) -> u64 {
    ctx.session().map(|s| s.metrics().ok).unwrap_or(0)
}

// ------------------------------------------------------------- urc book-keeping

/// What the unsolicited stream has carried.
///
/// Raw counts and decoded events are kept apart: "URC lines 4120, decoded 87"
/// is exactly the sentence that says the decoder has a hole in it.
#[derive(Default)]
struct UrcLog {
    lines: AtomicU64,
    decoded: AtomicU64,
    emitted: AtomicU64,
    events: Mutex<VecDeque<UrcEvent>>,
}

impl UrcLog {
    fn record(&self, line: &str) {
        self.lines.fetch_add(1, Ordering::SeqCst);
        let Some(urc) = urc::classify(line) else {
            return;
        };
        self.decoded.fetch_add(1, Ordering::SeqCst);
        // `emitted` is bumped under the same lock the readers take, so a mark
        // taken before a request and `since` after it can never disagree.
        let mut events = self.events.lock().unwrap();
        if events.len() >= KEEP_URCS {
            events.pop_front();
        }
        events.push_back(UrcEvent::new(urc));
        self.emitted.fetch_add(1, Ordering::SeqCst);
    }

    /// A mark to hand to `since` before something runs.
    fn mark(&self) -> u64 {
        self.emitted.load(Ordering::SeqCst)
    }

    /// The events recorded after `mark`, oldest first.
    fn since(&self, mark: u64) -> Vec<UrcEvent> {
        let events = self.events.lock().unwrap();
        let fresh = self.emitted.load(Ordering::SeqCst).saturating_sub(mark) as usize;
        let skip = events.len().saturating_sub(fresh);
        events.iter().skip(skip).cloned().collect()
    }

    /// The last `limit` events, oldest first.
    fn last(&self, limit: usize) -> Vec<UrcEvent> {
        let events = self.events.lock().unwrap();
        let skip = events.len().saturating_sub(limit);
        events.iter().skip(skip).cloned().collect()
    }
}

// ------------------------------------------------------------- the MT half

/// The text-mode surface the daemon needs before a `+CMTI:` means anything.
#[derive(Debug, Default)]
struct SmsSurface {
    /// `AT+CMGF=1` accepted: `CMGR` answers text, not PDU.
    text_mode: bool,
    /// `AT+CSCS="GSM"` accepted.  Measured on the device: the RIL leaves the
    /// TE character set at `HEX`, under which `CMGS="<number>"` is not a phone
    /// number at all and the send answers `+CMS ERROR: 302`.
    charset_gsm: bool,
    /// `AT+CNMI=2,1,0,0,0` accepted: a new message becomes a `+CMTI` on the
    /// unsolicited stream.  Without it the message lands in storage silently
    /// and the MT path never starts.
    mt_indication: bool,
}

fn configure_sms_surface(session: &crate::at::AtSession, timeout: Duration) -> SmsSurface {
    SmsSurface {
        text_mode: session.command("AT+CMGF=1", timeout, &[], 0).ok(),
        charset_gsm: session.command("AT+CSCS=\"GSM\"", timeout, &[], 0).ok(),
        mt_indication: session.command("AT+CNMI=2,1,0,0,0", timeout, &[], 0).ok(),
    }
}

/// Messages the modem has announced, and the ones we have read.
///
/// The announcement and the read are deliberately two steps: a `+CMTI:` can
/// arrive inside another command's reply loop, where sending AT is forbidden,
/// so it is queued here and read by the serve loop between requests.
#[derive(Default)]
struct MessageInbox {
    /// Whether `AT+CMGF=1` was accepted at start-up: text mode is what makes a
    /// `+CMTI:` readable, and its absence is reported rather than worked
    /// around (PDU is not decoded here).
    text_mode: AtomicBool,
    /// Whether the daemon got the TE character set onto GSM (`AT+CSCS="GSM"`).
    charset_gsm: AtomicBool,
    /// Whether `+CMTI` indications were enabled (`AT+CNMI=2,1,0,0,0`).
    mt_indication: AtomicBool,
    /// How many `+CMTI:` this daemon has seen.
    announced: AtomicU64,
    /// How many `AT+CMGR` reads it has made for them.
    reads: AtomicU64,
    read_failures: AtomicU64,
    pending: Mutex<VecDeque<(String, u32)>>,
    delivered: Mutex<VecDeque<TextMessage>>,
}

impl MessageInbox {
    /// A `+CMTI: "<storage>",<index>`.  The same slot announced twice before
    /// it has been read is one read, not two.
    fn announced(&self, storage: &str, index: u32) {
        self.announced.fetch_add(1, Ordering::SeqCst);
        let mut pending = self.pending.lock().unwrap();
        if pending.iter().any(|(s, i)| s == storage && *i == index) {
            return;
        }
        pending.push_back((storage.to_string(), index));
    }

    /// Announcements to read now, oldest first.
    fn drain(&self) -> Vec<(String, u32)> {
        self.pending.lock().unwrap().drain(..).collect()
    }

    /// A message that has been read; the newest `KEEP_MESSAGES` are kept.
    fn deliver(&self, message: TextMessage) {
        let mut delivered = self.delivered.lock().unwrap();
        if delivered.len() >= KEEP_MESSAGES {
            delivered.pop_front();
        }
        delivered.push_back(message);
    }

    /// The last `limit` messages, oldest first.
    fn messages(&self, limit: usize) -> Vec<TextMessage> {
        let delivered = self.delivered.lock().unwrap();
        let skip = delivered.len().saturating_sub(limit);
        delivered.iter().skip(skip).cloned().collect()
    }
}

// --------------------------------------------------------------- the protocol

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Request {
    /// `run` (the default), `state`, `urc` or `messages`.
    action: String,
    capability: String,
    args: Vec<String>,
    limit: Option<usize>,
}

#[derive(Debug, Default, Serialize)]
struct Response {
    ok: bool,
    action: String,
    status: String,
    exit_code: i32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    output: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<State>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    urcs: Vec<UrcEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    messages: Vec<TextMessage>,
}

impl Response {
    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            status: "error".into(),
            exit_code: 2,
            error: Some(message.into()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Serialize)]
struct State {
    pid: u32,
    socket: String,
    uptime_s: f64,
    requests: u64,
    probes: u64,
    probe_failures: u64,
    /// Age of the last command the CP actually answered — the number a watchdog
    /// keys on, because "the channel is open" and "the CP is answering" are two
    /// different facts (`docs/FINDINGS.md` §12).
    last_ok_age_s: Option<f64>,
    at: AtMetrics,
    channels: BTreeMap<String, ChannelMetrics>,
    urc_lines: u64,
    urc_decoded: u64,
    urc_undecoded: u64,
    /// Text mode is what makes a `+CMTI:` readable; its absence is reported,
    /// never silently worked around.
    text_mode: bool,
    /// The TE character set the daemon left the modem on.
    charset_gsm: bool,
    /// Whether new messages are announced at all.
    mt_indication: bool,
    /// Announcements seen / reads made / reads that were not a text message.
    messages_announced: u64,
    messages_read: u64,
    message_read_failures: u64,
    /// How many read messages are kept in the inbox.
    messages_kept: usize,
    events: Vec<UrcEvent>,
}

struct Runtime {
    socket: PathBuf,
    started: Instant,
    requests: u64,
    probes: u64,
    probe_failures: u64,
    last_ok: Option<Instant>,
    last_activity: Instant,
}

impl Runtime {
    fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
            started: Instant::now(),
            requests: 0,
            probes: 0,
            probe_failures: 0,
            last_ok: None,
            last_activity: Instant::now(),
        }
    }

    fn uptime(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

fn handle(
    ctx: &mut Context,
    log: &UrcLog,
    inbox: &MessageInbox,
    rt: &mut Runtime,
    mark: u64,
    conn: UnixStream,
) -> Result<()> {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = conn.set_write_timeout(Some(Duration::from_secs(30)));
    let mut writer = conn.try_clone()?;

    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line)?;

    // One request per connection, one JSON object per line: the shape the
    // sibling port's AT daemon already proves on this modem family.
    let response = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => dispatch(ctx, log, inbox, rt, mark, &request),
        Err(e) => Response::error(format!("invalid request: {e}")),
    };

    let mut body = serde_json::to_string(&response)?;
    body.push('\n');
    writer.write_all(body.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn dispatch(
    ctx: &mut Context,
    log: &UrcLog,
    inbox: &MessageInbox,
    rt: &mut Runtime,
    mark: u64,
    request: &Request,
) -> Response {
    match request.action.as_str() {
        "" | "run" => {
            // Only a run touches the modem, so only a run resets the heartbeat:
            // a client polling `state` must not be able to postpone the idle
            // probe — keeping the link warm is the daemon's job, not a
            // side-effect of being watched.
            rt.last_activity = Instant::now();
            run_capability(ctx, log, &inbox, mark, request)
        }
        "state" => Response {
            ok: true,
            action: "state".into(),
            status: "pass".into(),
            state: Some(state_of(ctx, log, inbox, rt)),
            ..Default::default()
        },
        "urc" => Response {
            ok: true,
            action: "urc".into(),
            status: "pass".into(),
            urcs: log.last(request.limit.unwrap_or(DEFAULT_EVENT_TAIL).min(KEEP_URCS)),
            ..Default::default()
        },
        "messages" => Response {
            ok: true,
            action: "messages".into(),
            status: "pass".into(),
            messages: inbox.messages(request.limit.unwrap_or(KEEP_MESSAGES)),
            ..Default::default()
        },
        other => Response::error(format!("unknown action {other:?} (run|state|urc|messages)")),
    }
}

fn run_capability(
    ctx: &mut Context,
    log: &UrcLog,
    inbox: &MessageInbox,
    mark: u64,
    request: &Request,
) -> Response {
    let Some(capability) = super::find(&request.capability) else {
        return Response::error(format!(
            "unknown capability {:?}; try `unisoc-cpd capabilities`",
            request.capability
        ));
    };
    if capability.name() == "serve" {
        return Response::error(
            "serve is the daemon itself and cannot be asked for over its own socket".to_string(),
        );
    }

    // A fresh diagnostic scope: one Context answers every request of a boot, and
    // a summary carrying the previous request's events would be a lie.
    ctx.begin_run();

    let outcome = match capability.run(ctx, &request.args) {
        Ok(outcome) => outcome,
        Err(e) => return Response::error(format!("{e:#}")),
    };

    // Whatever the network said while we were asking belongs to this run.
    for event in log.since(mark) {
        ctx.event(event.urc.kind(), event.urc.detail());
    }

    // A power cycle wipes the SMS surface this daemon armed at start
    // (measured: +CNMI drops back to 0,0,0,1,0 through a cold cycle, and a
    // +CMTI nobody announces never starts the MT path).  Re-arm after every
    // passing cfun so the resident owner keeps its ears.
    if capability.name() == "cfun" && outcome.passed() {
        let timeout = Duration::from_secs_f64(ctx.profile.at.default_timeout.max(1.0));
        if let Ok(session) = ctx.at() {
            let surface = configure_sms_surface(&session, timeout);
            inbox.text_mode.store(surface.text_mode, Ordering::SeqCst);
            inbox.charset_gsm.store(surface.charset_gsm, Ordering::SeqCst);
            inbox
                .mt_indication
                .store(surface.mt_indication, Ordering::SeqCst);
            if !surface.mt_indication {
                eprintln!(
                    "unisoc-cpd: +CMTI could not be re-armed after cfun; new \
                     messages will sit in storage unannounced"
                );
            }
        }
    }

    let status = match outcome.status {
        Status::Pass => "pass",
        Status::Fail => "fail",
    };
    let exit_code = if outcome.passed() { 0 } else { 1 };

    if ctx.telemetry {
        let summary = ctx.finish(capability.name(), status, exit_code);
        if let Err(e) = summary.write(&ctx.runs_dir) {
            eprintln!("unisoc-cpd: could not write the run summary: {e:#}");
        }
    }

    println!(
        "unisoc-cpd: {} -> {status} ({} line(s) of output)",
        capability.name(),
        outcome.output.len()
    );
    if ctx.verbose {
        for line in &outcome.output {
            println!("  {line}");
        }
    }

    Response {
        ok: outcome.passed(),
        action: "run".into(),
        status: status.into(),
        exit_code,
        output: outcome.output,
        notes: ctx.notes.clone(),
        ..Default::default()
    }
}

fn state_of(ctx: &Context, log: &UrcLog, inbox: &MessageInbox, rt: &Runtime) -> State {
    let mut channels = BTreeMap::new();
    for name in ["cmd", "urc"] {
        if let Some(channel) = ctx.channel(name) {
            channels.insert(name.to_string(), channel.metrics());
        }
    }
    let lines = log.lines.load(Ordering::SeqCst);
    let decoded = log.decoded.load(Ordering::SeqCst);
    State {
        pid: std::process::id(),
        socket: rt.socket.display().to_string(),
        uptime_s: rt.uptime(),
        requests: rt.requests,
        probes: rt.probes,
        probe_failures: rt.probe_failures,
        last_ok_age_s: rt.last_ok.map(|t| t.elapsed().as_secs_f64()),
        at: ctx.session().map(|s| s.metrics()).unwrap_or_default(),
        channels,
        urc_lines: lines,
        urc_decoded: decoded,
        urc_undecoded: lines.saturating_sub(decoded),
        text_mode: inbox.text_mode.load(Ordering::SeqCst),
        charset_gsm: inbox.charset_gsm.load(Ordering::SeqCst),
        mt_indication: inbox.mt_indication.load(Ordering::SeqCst),
        messages_announced: inbox.announced.load(Ordering::SeqCst),
        messages_read: inbox.reads.load(Ordering::SeqCst),
        message_read_failures: inbox.read_failures.load(Ordering::SeqCst),
        messages_kept: inbox.messages(usize::MAX).len(),
        events: log.last(DEFAULT_EVENT_TAIL),
    }
}

// ------------------------------------------------------------------- the client

/// Ask a running daemon, and print what it answered.
///
/// `capability` is a capability name — in which case this is the same command a
/// direct run would be, except that the daemon owns the channel instead of this
/// process — or one of the daemon's own questions, `state`, `urc` and
/// `messages`.
///
/// Exit codes follow the README's convention: the capability's own `0`/`1`, `2`
/// for a request the daemon rejected, and `3` when there is no daemon to ask,
/// which is an environment problem rather than a failure of the request.
pub fn client(socket: &Path, capability: &str, args: &[String]) -> Result<i32> {
    let request = match capability {
        "state" => serde_json::json!({ "action": "state" }),
        "urc" => serde_json::json!({
            "action": "urc",
            "limit": flag_value(args, "--limit").and_then(|v| v.parse::<usize>().ok()),
        }),
        "messages" => serde_json::json!({
            "action": "messages",
            "limit": flag_value(args, "--limit").and_then(|v| v.parse::<usize>().ok()),
        }),
        other => serde_json::json!({ "action": "run", "capability": other, "args": args }),
    };

    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!(
                "unisoc-cpd: cannot reach the daemon on {}: {e}",
                socket.display()
            );
            eprintln!("unisoc-cpd: is `unisoc-cpd serve` running?");
            return Ok(3);
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let mut writer = stream.try_clone()?;
    let mut body = request.to_string();
    body.push('\n');
    writer.write_all(body.as_bytes())?;
    writer.flush()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: serde_json::Value = serde_json::from_str(line.trim()).with_context(|| {
        format!("the daemon answered with something that is not JSON: {line:?}")
    })?;

    if let Some(error) = response.get("error").and_then(|v| v.as_str()) {
        eprintln!("unisoc-cpd: {error}");
    }
    if let Some(state) = response.get("state") {
        println!("{}", serde_json::to_string_pretty(state)?);
    }
    for event in arr(&response, "urcs") {
        let at = event.get("at").and_then(|v| v.as_str()).unwrap_or("");
        let urc = event.get("urc").cloned().unwrap_or_default();
        println!("{at}  {urc}");
    }
    for message in arr(&response, "messages") {
        let field = |key: &str| {
            message
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        println!(
            "{}  ({}, {} index {}, {})",
            field("from"),
            field("status"),
            field("storage"),
            message.get("index").and_then(|v| v.as_u64()).unwrap_or(0),
            field("timestamp")
        );
        for line in field("text").lines() {
            println!("  {line}");
        }
    }
    for line in arr(&response, "output") {
        println!("{}", line.as_str().unwrap_or_default());
    }
    for note in arr(&response, "notes") {
        println!("note: {}", note.as_str().unwrap_or_default());
    }
    // A run prints its status the way a direct run does; a query has nothing to
    // pass or fail.
    if !matches!(capability, "state" | "urc" | "messages") {
        if let Some(status) = response.get("status").and_then(|v| v.as_str()) {
            println!("status: {status}");
        }
    }
    Ok(response
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(1) as i32)
}

fn arr<'a>(value: &'a serde_json::Value, key: &str) -> &'a [serde_json::Value] {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_defaults_to_running_a_capability() {
        let request: Request = serde_json::from_str(r#"{"capability":"sim"}"#).unwrap();
        assert_eq!(request.action, "");
        assert_eq!(request.capability, "sim");
        assert!(request.args.is_empty());
        assert_eq!(request.limit, None);
        let request: Request =
            serde_json::from_str(r#"{"capability":"sms","args":["read","3"]}"#).unwrap();
        assert_eq!(request.args, vec!["read".to_string(), "3".to_string()]);
    }

    /// The number `serve` hands a client, and the half that makes A2/A5/A9
    /// possible: what the network said while the request was in flight.
    #[test]
    fn the_urc_log_hands_back_only_what_arrived_after_the_mark() {
        let log = UrcLog::default();
        log.record("+CSQ: 20,99");
        let mark = log.mark();
        log.record("+CMTI: \"SM\",1");
        log.record("+CLIP: \"+8613800138000\",129");

        let fresh = log.since(mark);
        assert_eq!(fresh.len(), 2);
        assert_eq!(fresh[0].urc.kind(), "urc-new-message");
        assert_eq!(fresh[1].urc.kind(), "urc-caller-id");
        assert_eq!(log.last(10).len(), 3);
        assert_eq!(log.since(log.mark()), Vec::new());
    }

    /// "URC lines 4, decoded 2" is the sentence that says the decoder has a
    /// hole in it, so the two counters must not be conflated.
    #[test]
    fn undecoded_lines_are_counted_but_not_called_decoded() {
        let log = UrcLog::default();
        log.record("+CGEV: ME PDN ACT 1");
        log.record("+SPPCODATA: 1");
        log.record("OK"); // not URC material at all
        assert_eq!(log.lines.load(Ordering::SeqCst), 3);
        assert_eq!(log.decoded.load(Ordering::SeqCst), 2);
        assert_eq!(log.last(10).len(), 2);
    }

    #[test]
    fn the_event_ring_is_bounded_and_keeps_the_newest() {
        let log = UrcLog::default();
        for i in 0..(KEEP_URCS + 10) {
            log.record(&format!("+CMTI: \"SM\",{i}"));
        }
        assert_eq!(log.emitted.load(Ordering::SeqCst), KEEP_URCS as u64 + 10);
        let kept = log.last(usize::MAX);
        assert_eq!(kept.len(), KEEP_URCS);
        // The oldest ten were evicted, not the newest.
        assert!(matches!(
            &kept[0].urc,
            urc::Urc::NewMessage { index, .. } if *index == 10
        ));
    }

    /// The MT path's contract: the same slot announced twice before it has
    /// been read is one read, and a read message is kept for the client.
    #[test]
    fn the_inbox_deduplicates_announcements_and_keeps_what_was_read() {
        let inbox = MessageInbox::default();
        inbox.announced("SM", 7);
        inbox.announced("SM", 7); // the modem may repeat itself
        inbox.announced("SM", 8);
        assert_eq!(inbox.drain().len(), 2);
        assert_eq!(inbox.drain(), Vec::new());

        inbox.deliver(TextMessage {
            storage: "SM".into(),
            index: 7,
            status: "REC UNREAD".into(),
            from: "+8613800138000".into(),
            timestamp: "26/09/20,10:00:00+32".into(),
            text: "hello".into(),
        });
        let messages = inbox.messages(10);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].storage, "SM");
        assert_eq!(messages[0].text, "hello");
    }

    #[test]
    fn the_inbox_ring_is_bounded_and_keeps_the_newest() {
        let inbox = MessageInbox::default();
        for i in 0..(KEEP_MESSAGES + 5) {
            inbox.deliver(TextMessage {
                storage: "SM".into(),
                index: i as u32,
                status: "REC READ".into(),
                from: String::new(),
                timestamp: String::new(),
                text: format!("message {i}"),
            });
        }
        let kept = inbox.messages(usize::MAX);
        assert_eq!(kept.len(), KEEP_MESSAGES);
        assert_eq!(kept[0].text, "message 5");
        assert_eq!(
            kept.last().unwrap().text,
            format!("message {}", KEEP_MESSAGES + 4)
        );
    }

    #[test]
    fn an_error_response_is_not_a_pass() {
        let response = Response::error("unknown capability");
        assert!(!response.ok);
        assert_eq!(response.status, "error");
        assert_eq!(response.exit_code, 2);
    }
}
