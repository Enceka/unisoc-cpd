//! `data` — PDP context bring-up and the bearer interface.
//!
//! The AT half is the sequence captured from the vendor side; the Linux half
//! (bring the interface up, add the address the modem handed us) is generic.
//! Anything that is genuinely platform-shaped — the NAT table, a LED, a QMI
//! bridge — is a `vendor`-mode command or a profile hook, never a `#ifdef`.

use super::{emit, flag_value, positionals, Capability, Outcome};
use crate::context::Context;
use anyhow::{bail, Context as _, Result};
use std::net::Ipv4Addr;
use std::process::Command;
use std::time::Duration;

pub struct Data;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context5 {
    pub cid: u32,
    pub apn: String,
    pub address: Option<Ipv4Addr>,
    pub mask: Option<Ipv4Addr>,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
}

/// `+CGCONTRDP: cid,bearer,apn,"a.b.c.d.m.m.m.m",gw,dns1,dns2,...`
pub fn parse_cgcontrdp(line: &str) -> Option<Context5> {
    let body = line.split_once(':')?.1;
    let fields: Vec<String> = body
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .collect();
    let get = |i: usize| fields.get(i).cloned().unwrap_or_default();
    let cid = get(0).parse().unwrap_or(0);
    let addrmask = get(3);
    let mut parts = addrmask.split('.');
    let (a, b, c, d) = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    let address = Some(Ipv4Addr::new(a, b, c, d));
    let mask = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m1), Some(m2), Some(m3), Some(m4)) => Some(Ipv4Addr::new(
            m1.parse().ok()?,
            m2.parse().ok()?,
            m3.parse().ok()?,
            m4.parse().ok()?,
        )),
        _ => None,
    };
    let mut dns = Vec::new();
    for i in [5usize, 6] {
        let v = get(i);
        if !v.is_empty() && v != "0.0.0.0" {
            dns.push(v);
        }
    }
    Some(Context5 {
        cid,
        apn: get(2),
        address,
        mask,
        gateway: Some(get(4)).filter(|s| !s.is_empty()),
        dns,
    })
}

/// Dotted netmask -> prefix length.  A wrong prefix leaves the interface with
/// /32 and no on-link subnet, which is a silent, total failure of the bearer.
pub fn mask_to_prefix(mask: Ipv4Addr) -> u8 {
    u32::from(mask).count_ones() as u8
}

fn run(cmd: &str, args: &[&str]) -> (bool, String) {
    match Command::new(cmd).args(args).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        ),
        Err(e) => (false, format!("{e}")),
    }
}

/// Read `KEY=value` out of the profile's APN source (a shell-style conf file).
pub fn apn_from_source(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("APN=") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The APN a SIM's home operator is known by, keyed by MCC-MNC.  This is the
/// fallback *behind* the modem's own context and the config file: a bearer
/// resolves its APN in the order argument > /etc/e5/mobile-data.conf >
/// AT+CGDCONT? (what Android or the factory left configured) > this table >
/// nothing.  Extend it as new cards turn up; an unknown IMSI is reported, not
/// guessed.
const APN_BY_MCCMNC: &[(&str, &str)] = &[
    // China
    ("46000", "cmnet"),
    ("46002", "cmnet"),
    ("46004", "cmnet"),
    ("46007", "cmnet"),
    ("46008", "cmnet"),
    ("46001", "3gnet"),
    ("46006", "3gnet"),
    ("46009", "3gnet"),
    ("46003", "ctlte"),
    ("46011", "ctlte"),
    ("46015", "cbnet"),
    // Hong Kong, Taiwan, and a few common roaming partners
    ("45400", "mobile"),
    ("45403", "three.com.hk"),
    ("45406", "smartone"),
    ("46692", "internet"),
    ("46697", "internet"),
    ("26201", "internet"),
    ("26202", "internet"),
    ("23410", "internet"),
    ("310260", "epc.tmobile.com"),
    ("310410", "nxtgenphone"),
];

/// The APN of one context out of an AT+CGDCONT? reply: the third field, quoted.
/// An empty one means the context exists but names no APN.
pub fn cgdcont_apn(reply: &crate::at::Reply, cid: u32) -> Option<String> {
    for line in &reply.lines {
        let body = match line.trim().strip_prefix("+CGDCONT:") {
            Some(b) => b.trim(),
            None => continue,
        };
        let mut it = body.split(',');
        let got: u32 = it.next()?.trim().parse().ok()?;
        if got != cid {
            continue;
        }
        let _pdp_type = it.next();
        let apn = it.next()?.trim().trim_matches('"').to_string();
        if !apn.is_empty() {
            return Some(apn);
        }
    }
    None
}

/// One PDP context, as `AT+CGDCONT?` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApnContext {
    pub cid: u32,
    pub pdp_type: String,
    pub apn: String,
}

/// Every context in an `AT+CGDCONT?` reply:
/// `+CGDCONT: <cid>,"<pdp type>","<apn>",...`.  A context with no APN is a
/// context that exists and names nothing, which is a different fact from a
/// context that is not there -- so it is kept, with an empty name.
pub fn parse_cgdcont_all(reply: &crate::at::Reply) -> Vec<ApnContext> {
    let mut out = Vec::new();
    for line in &reply.lines {
        let Some(body) = line.trim().strip_prefix("+CGDCONT:") else {
            continue;
        };
        let fields: Vec<String> = body
            .split(',')
            .map(|f| f.trim().trim_matches('"').to_string())
            .collect();
        let Some(cid) = fields.first().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        out.push(ApnContext {
            cid,
            pdp_type: fields.get(1).cloned().unwrap_or_default(),
            apn: fields.get(2).cloned().unwrap_or_default(),
        });
    }
    out
}

/// Which contexts are up, out of `AT+CGACT?`: `+CGACT: <cid>,<state>`.
pub fn parse_cgact(reply: &crate::at::Reply) -> Vec<(u32, u32)> {
    reply
        .lines
        .iter()
        .filter_map(|line| {
            let body = line.trim().strip_prefix("+CGACT:")?;
            let mut it = body.split(',').map(|f| f.trim());
            let cid = it.next()?.parse::<u32>().ok()?;
            let state = it.next()?.parse::<u32>().ok()?;
            Some((cid, state))
        })
        .collect()
}

/// An APN as it goes into an AT command: inside quotes, which is why a value
/// carrying a quote, a comma or a space is refused rather than escaped.  An
/// APN someone meant to type never contains one of those, and a command built
/// out of one would not be the command they think they asked for.
pub fn apn_argument(raw: Option<&String>) -> anyhow::Result<String> {
    let Some(raw) = raw else {
        anyhow::bail!("this action needs an APN, e.g. `data set-apn cbnet`");
    };
    let apn = raw.trim().to_string();
    if apn.is_empty() || apn.len() > 100 {
        anyhow::bail!("an APN must be 1..100 characters, got {apn:?}");
    }
    if apn
        .chars()
        .any(|c| c == '"' || c == ',' || c.is_whitespace() || c == '\r' || c == '\n')
    {
        anyhow::bail!(
            "an APN cannot contain a quote, a comma or a space (got {apn:?}); \
             the value goes into AT+CGDCONT inside quotes"
        );
    }
    Ok(apn)
}

/// Rewrite the `APN=` line of a shell-style config file.
///
/// Everything else -- comments, other keys, blank lines, and a commented
/// `#APN=` template line -- is left exactly as it was, because that commented
/// line is the file's documented "not pinned" state and the comments are where
/// the resolution order is written down.  A second active `APN=` line would be
/// ambiguous, so it is dropped rather than left to win by position.
///
/// Pure, so the rewriting is testable without touching a filesystem.
pub fn upsert_apn(text: &str, apn: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut written = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            lines.push(line.to_string());
            continue;
        }
        if trimmed.starts_with("APN=") {
            if !written {
                lines.push(format!("APN={apn}"));
                written = true;
            }
            continue;
        }
        lines.push(line.to_string());
    }
    if !written {
        lines.push(format!("APN={apn}"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// The home operator's APN, from the SIM's IMSI: the first three digits are the
/// MCC, the rest the MNC (two or three digits, so the longest table key wins).
pub fn imsi_apn(imsi: &str) -> Option<(String, String)> {    let digits: String = imsi.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 5 {
        return None;
    }
    let mcc = &digits[0..3];
    let mnc = &digits[3..];
    for len in [3usize, 2] {
        if mnc.len() < len {
            continue;
        }
        let key = format!("{mcc}{}", &mnc[0..len]);
        if let Some((_, apn)) = APN_BY_MCCMNC.iter().find(|(k, _)| *k == key) {
            return Some((key, (*apn).to_string()));
        }
    }
    None
}

/// Resolve the APN in the order the plan wants: the command line, then the
/// config file (which is therefore the "allowed to be edited" override), then
/// what the modem already has configured, then the SIM's operator.  The second
/// return value names the source so a caller can say where an address came from.
pub fn resolve_apn(
    session: &crate::at::AtSession,
    cid: u32,
    arg: Option<String>,
    source_path: Option<&str>,
) -> (Option<String>, String) {
    if let Some(a) = arg {
        let a = a.trim().to_string();
        if !a.is_empty() {
            return (Some(a), "argument".to_string());
        }
    }
    if let Some(p) = source_path {
        if let Some(a) = apn_from_source(p) {
            return (Some(a), format!("config {p}"));
        }
    }
    let cgd = session.command("AT+CGDCONT?", Duration::from_secs(8), &[], 0);
    if let Some(a) = cgdcont_apn(&cgd, cid) {
        return (Some(a), format!("modem +CGDCONT? cid {cid}"));
    }
    let cimi = session.command("AT+CIMI", Duration::from_secs(8), &[], 0);
    let imsi = cimi
        .lines
        .iter()
        .map(|l| l.trim())
        .find(|l| l.len() >= 5 && l.chars().all(|c| c.is_ascii_digit()));
    if let Some(imsi) = imsi {
        match imsi_apn(imsi) {
            Some((key, apn)) => return (Some(apn), format!("imsi {key}")),
            None => return (None, format!("imsi {imsi} is not in the APN table")),
        }
    }
    (None, "no source".to_string())
}

/// Pass or fail, from the lines and the verdict.
fn outcome(lines: Vec<String>, ok: bool) -> Outcome {
    if ok {
        Outcome::pass(lines)
    } else {
        Outcome::fail(lines)
    }
}

/// The APN given for a management action: the positional, or `--apn`.
fn apn_raw(args: &[String], pos: &[String]) -> Option<String> {
    pos.get(1)
        .cloned()
        .or_else(|| flag_value(args, "--apn").map(|s| s.to_string()))
}

/// What the profile's override file currently says, when it says anything.
fn saved_apn(ctx: &Context) -> Option<String> {
    ctx.profile
        .data
        .apn_source
        .as_deref()
        .and_then(apn_from_source)
}

/// The resolved APN and where it came from, as the summary lines the web panel
/// reads -- plus the human-readable line the CLI prints.
fn resolved_lines(out: &mut Vec<String>, apn: Option<String>, source: &str) {
    match &apn {
        Some(apn) => {
            out.push(format!("APN {apn} (source: {source})"));
            out.push(format!("apn: {apn}"));
        }
        None => {
            out.push(format!("no APN found ({source})"));
            out.push("apn: -".to_string());
        }
    }
    out.push(format!("apn_source: {source}"));
}

/// A summary value, with `-` for "nothing here".
fn or_dash(value: Option<String>) -> String {
    value.filter(|v| !v.is_empty()).unwrap_or_else(|| "-".to_string())
}

fn or_dash_value(value: &str) -> String {
    if value.is_empty() {
        "-".to_string()
    } else {
        value.to_string()
    }
}

impl Capability for Data {
    fn name(&self) -> &'static str {
        "data"
    }

    fn summary(&self) -> &'static str {
        "PDP context and bearer: up [apn] | down | status | apn | contexts | \
         set-apn <apn> | clear-apn | save-apn <apn>"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("status");
        let cid = ctx.profile.data.cid.max(1);
        let iface = ctx.profile.data.interface(None);

        match action {
            "status" => {
                let session = ctx.at()?;
                let mut out = Vec::new();
                let mut rdp: Option<crate::at::Reply> = None;
                for cmd in [
                    "AT+CEREG?".to_string(),
                    "AT+CGATT?".to_string(),
                    "AT+CGACT?".to_string(),
                    format!("AT+CGCONTRDP={cid}"),
                ] {
                    let r = session.command(&cmd, Duration::from_secs(8), &[], 0);
                    emit(&mut out, &cmd, &r);
                    if cmd.starts_with("AT+CGCONTRDP") {
                        rdp = Some(r);
                    }
                }
                // The address the modem handed out, as a summary line: the
                // interface listing below is what the host thinks, this is
                // what the network said, and a client wants both told apart.
                let context = rdp
                    .as_ref()
                    .and_then(|r| r.first_with_prefix("+CGCONTRDP:"))
                    .and_then(parse_cgcontrdp);
                let dash = || "-".to_string();
                out.push(format!(
                    "ip: {}",
                    context
                        .as_ref()
                        .and_then(|c| c.address)
                        .map(|a| a.to_string())
                        .unwrap_or_else(dash)
                ));
                out.push(format!(
                    "apn: {}",
                    context
                        .as_ref()
                        .map(|c| c.apn.clone())
                        .filter(|a| !a.is_empty())
                        .unwrap_or_else(dash)
                ));
                out.push(format!(
                    "dns: {}",
                    context
                        .as_ref()
                        .map(|c| c.dns.join(","))
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(dash)
                ));
                if let Some(iface) = &iface {
                    let (_, addr) = run("ip", &["-br", "addr", "show", iface]);
                    out.push(format!("interface {iface}: {addr}"));
                    let (_, route) = run("ip", &["route", "show", "dev", iface]);
                    out.push(format!("routes: {}", route.replace('\n', " | ")));
                }
                Ok(Outcome::pass(out))
            }
            "apn" => {
                // Reports what a bearer would use *and* where it came from, so
                // "it picked the wrong APN" is answerable without a packet
                // capture: argument > config file > modem's own context > SIM.
                let session = ctx.at()?;
                let (apn, source) = resolve_apn(
                    &session,
                    cid,
                    pos.get(1).cloned(),
                    ctx.profile.data.apn_source.as_deref(),
                );
                let mut out = Vec::new();
                resolved_lines(&mut out, apn, &source);
                out.push(format!("saved_apn: {}", or_dash(saved_apn(ctx))));
                Ok(Outcome::pass(out))
            }
            // ------------------------------------------------------- management
            //
            // Three places can hold an APN, and they are not the same place:
            // the modem's own context (what the bearer actually uses), this
            // profile's override file (what the resolution order reads before
            // the modem), and the SIM/table fallback (which is nobody's to
            // write).  The panel reads all three and writes the two that can be
            // written -- each with a read-back, because a write that did not
            // take must not look like one.
            "contexts" => {
                let session = ctx.at()?;
                let mut out = Vec::new();
                let cgd = session.command("AT+CGDCONT?", Duration::from_secs(8), &[], 0);
                let act = session.command("AT+CGACT?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+CGDCONT?", &cgd);
                emit(&mut out, "AT+CGACT?", &act);

                let contexts = parse_cgdcont_all(&cgd);
                let active = parse_cgact(&act);
                for context in &contexts {
                    let state = active.iter().find(|(id, _)| *id == context.cid);
                    out.push(format!(
                        "context: {},{},{},{}",
                        context.cid,
                        or_dash_value(&context.pdp_type),
                        or_dash_value(&context.apn),
                        match state.map(|(_, s)| *s) {
                            Some(1) => "active",
                            Some(_) => "inactive",
                            None => "unknown",
                        }
                    ));
                }
                out.push(format!("contexts: {}", contexts.len()));

                let (apn, source) = resolve_apn(&session, cid, None, ctx.profile.data.apn_source.as_deref());
                resolved_lines(&mut out, apn, &source);
                out.push(format!("saved_apn: {}", or_dash(saved_apn(ctx))));
                out.push(format!("cid: {cid}"));
                out.push(format!(
                    "apn_source_path: {}",
                    ctx.profile.data.apn_source.clone().unwrap_or_else(|| "-".to_string())
                ));
                Ok(outcome(out, cgd.ok()))
            }
            "set-apn" => {
                let apn = apn_argument(apn_raw(args, &pos).as_ref())?;
                let cid = flag_value(args, "--cid")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(cid);
                let session = ctx.at()?;
                let mut out = Vec::new();
                let cmd = format!("AT+CGDCONT={cid},\"IPV4V6\",\"{apn}\"");
                let r = session.command(&cmd, Duration::from_secs(15), &[], 0);
                emit(&mut out, &cmd, &r);
                let back = session.command("AT+CGDCONT?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+CGDCONT?", &back);
                let applied = parse_cgdcont_all(&back)
                    .into_iter()
                    .find(|c| c.cid == cid)
                    .map(|c| c.apn);
                out.push(format!("apn: {apn}"));
                out.push(format!(
                    "context_{cid}_apn: {}",
                    or_dash(applied.clone())
                ));
                let ok = r.ok() && applied.as_deref() == Some(apn.as_str());
                if ok {
                    ctx.event("apn-set", format!("cid {cid} {apn}"));
                    out.push(
                        "note: the context changed; the bearer only picks it up when it is \
                         re-established (data down, then data up)"
                            .to_string(),
                    );
                } else {
                    ctx.note(format!(
                        "context {cid} reads back as {:?}, not {apn:?}",
                        applied
                    ));
                }
                Ok(outcome(out, ok))
            }
            "clear-apn" => {
                let cid = flag_value(args, "--cid")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(cid);
                let session = ctx.at()?;
                let mut out = Vec::new();
                let cmd = format!("AT+CGDCONT={cid}");
                let r = session.command(&cmd, Duration::from_secs(15), &[], 0);
                emit(&mut out, &cmd, &r);
                let back = session.command("AT+CGDCONT?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+CGDCONT?", &back);
                // Cleared means either the context is gone or it names nothing;
                // both are "no APN here", and neither is a failure of the write.
                let still = parse_cgdcont_all(&back)
                    .into_iter()
                    .find(|c| c.cid == cid)
                    .map(|c| c.apn)
                    .unwrap_or_default();
                out.push(format!("context_{cid}_apn: -"));
                let ok = r.ok() && still.is_empty();
                if ok {
                    ctx.event("apn-clear", format!("cid {cid}"));
                } else {
                    ctx.note(format!("context {cid} still reads back as {still:?}"));
                }
                Ok(outcome(out, ok))
            }
            "save-apn" => {
                let apn = apn_argument(apn_raw(args, &pos).as_ref())?;
                let mut out = Vec::new();
                let Some(path) = ctx.profile.data.apn_source.clone() else {
                    bail!(
                        "profile {:?} names no [data].apn_source, so there is nowhere to \
                         save an APN",
                        ctx.profile.name
                    );
                };
                let existing = std::fs::read_to_string(&path).unwrap_or_default();
                std::fs::write(&path, upsert_apn(&existing, &apn))
                    .with_context(|| format!("writing {path}"))?;
                // Read it back through the same reader the bearer resolves
                // with: the file is only "saved" if that reader sees it.
                let back = apn_from_source(&path);
                out.push(format!("saved APN {apn} to {path}"));
                out.push(format!("saved_apn: {}", or_dash(back.clone())));
                let ok = back.as_deref() == Some(apn.as_str());
                if ok {
                    ctx.event("apn-save", apn.clone());
                } else {
                    ctx.note(format!("{path} reads back as {back:?}, not {apn:?}"));
                }
                Ok(outcome(out, ok))
            }
            "up" => {
                let session = ctx.at()?;
                let (apn, source) = resolve_apn(
                    &session,
                    cid,
                    pos.get(1).cloned(),
                    ctx.profile.data.apn_source.as_deref(),
                );
                let mut out = Vec::new();

                match &apn {
                    // The modem's own context is already what it would be told.
                    Some(a) if source.starts_with("modem") => {
                        out.push(format!("apn {a} (source: {source}, already configured)"));
                    }
                    Some(a) => {
                        let cmd = format!("AT+CGDCONT={cid},\"IPV4V6\",\"{a}\"");
                        let r = session.command(&cmd, Duration::from_secs(15), &[], 0);
                        emit(&mut out, &cmd, &r);
                        out.push(format!("apn {a} (source: {source})"));
                    }
                    None => out.push(format!("! no APN found ({source})")),
                }

                let active = session.command("AT+CGACT?", Duration::from_secs(8), &[], 0);
                let already = active.line_with(&format!("+CGACT:{cid},1")).is_some();
                emit(&mut out, "AT+CGACT?", &active);
                if !already {
                    let cmd = format!("AT+CGACT=1,{cid}");
                    let r = session.command(&cmd, Duration::from_secs(30), &[], 0);
                    emit(&mut out, &cmd, &r);
                }

                let rdp = session.command(
                    &format!("AT+CGCONTRDP={cid}"),
                    Duration::from_secs(10),
                    &[],
                    0,
                );
                emit(&mut out, &format!("AT+CGCONTRDP={cid}"), &rdp);
                let parsed = rdp
                    .first_with_prefix("+CGCONTRDP:")
                    .and_then(parse_cgcontrdp);

                let connect = session.command(
                    &format!("AT+CGDATA=\"M-ETHER\",{cid}"),
                    Duration::from_secs(20),
                    &["CONNECT".to_string()],
                    0,
                );
                emit(&mut out, "AT+CGDATA=\"M-ETHER\",cid", &connect);

                let mut ok = true;
                match (parsed, &iface) {
                    (Some(c5), Some(iface)) => {
                        let Some(addr) = c5.address else {
                            bail!("+CGCONTRDP carried no IPv4 address");
                        };
                        let prefix = c5.mask.map(mask_to_prefix).unwrap_or(8);
                        let (_, _) = run("ip", &["link", "set", iface, "up"]);
                        // the sipa driver hands up wrong hardware checksums, and
                        // turning only rx off is not enough: with tx-checksumming
                        // (and its TSO/GSO children) still on, every packet the
                        // bearer sends is dropped -- +CGCONTRDP looks perfect and
                        // ping gets nothing.
                        let _ = run(
                            "ethtool",
                            &["-K", iface, "rx", "off", "tx", "off", "tso", "off", "gso", "off"],
                        );
                        let _ = run("ip", &["addr", "flush", "dev", iface]);
                        let (aok, aerr) = run(
                            "ip",
                            &["addr", "add", &format!("{addr}/{prefix}"), "dev", iface],
                        );
                        out.push(format!(
                            "ip addr add {addr}/{prefix} dev {iface}: {aok} {aerr}"
                        ));
                        let (rok, rerr) = run(
                            "ip",
                            &["route", "replace", "default", "dev", iface, "metric", "100"],
                        );
                        out.push(format!(
                            "ip route replace default dev {iface}: {rok} {rerr}"
                        ));
                        ok &= aok && rok;
                        if !c5.dns.is_empty() {
                            out.push(format!("dns {}", c5.dns.join(" ")));
                        }
                        out.push(format!("apn {}", c5.apn));
                    }
                    (_, None) => {
                        out.push("! profile has no data interface name".into());
                        ok = false;
                    }
                    (None, Some(_)) => {
                        out.push("! no +CGCONTRDP: the context never reported an address".into());
                        ok = false;
                    }
                }

                if ok {
                    ctx.event(
                        "data",
                        format!("bearer up on {}", iface.clone().unwrap_or_default()),
                    );
                }
                Ok(if ok {
                    Outcome::pass(out)
                } else {
                    Outcome::fail(out)
                })
            }
            "down" => {
                let session = ctx.at()?;
                let mut out = Vec::new();
                let cmd = format!("AT+CGACT=0,{cid}");
                let r = session.command(&cmd, Duration::from_secs(10), &[], 0);
                emit(&mut out, &cmd, &r);
                if let Some(iface) = &iface {
                    let _ = run("ip", &["route", "del", "default", "dev", iface]);
                    let _ = run("ip", &["addr", "flush", "dev", iface]);
                    let (dok, _) = run("ip", &["link", "set", iface, "down"]);
                    out.push(format!("{iface} down: {dok}"));
                }
                Ok(Outcome::pass(out))
            }
            other => bail!(
                "data: unknown action {other:?} \
                 (up [apn]|down|status|apn|contexts|set-apn <apn>|clear-apn|save-apn <apn>)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgcontrdp_is_parsed() {
        let line = "+CGCONTRDP: 1,5,\"3gnet\",\"10.105.136.142.255.0.0.0\",\"10.0.0.1\",\"58.240.57.33\",\"221.6.4.66\"";
        let c = parse_cgcontrdp(line).unwrap();
        assert_eq!(c.cid, 1);
        assert_eq!(c.apn, "3gnet");
        assert_eq!(c.address, Some(Ipv4Addr::new(10, 105, 136, 142)));
        assert_eq!(c.mask, Some(Ipv4Addr::new(255, 0, 0, 0)));
        assert_eq!(c.dns, vec!["58.240.57.33", "221.6.4.66"]);
        assert_eq!(mask_to_prefix(c.mask.unwrap()), 8);
    }

    #[test]
    fn mask_to_prefix_covers_the_common_widths() {
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 255, 0)), 24);
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 254, 0)), 23);
        assert_eq!(mask_to_prefix(Ipv4Addr::new(255, 255, 255, 255)), 32);
    }

    fn reply(lines: &[&str]) -> crate::at::Reply {
        crate::at::Reply {
            command: "AT".to_string(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            urcs: Vec::new(),
            final_code: crate::at::FinalCode::Ok,
            elapsed: std::time::Duration::from_millis(1),
            attempts: 1,
        }
    }

    #[test]
    fn cgdcont_lines_are_read_with_their_empty_contexts() {
        let answer = reply(&[
            "+CGDCONT: 1,\"IPV4V6\",\"3gnet\",\"0.0.0.0.0.0.0.0\",0,0,0,0",
            "+CGDCONT: 2,\"IPV4V6\",\"\",\"0.0.0.0.0.0.0.0\",0,0,0,0",
            "OK",
        ]);
        let contexts = parse_cgdcont_all(&answer);
        assert_eq!(contexts.len(), 2);
        assert_eq!(contexts[0].cid, 1);
        assert_eq!(contexts[0].pdp_type, "IPV4V6");
        assert_eq!(contexts[0].apn, "3gnet");
        // a context that exists and names nothing is still a context
        assert_eq!(contexts[1].cid, 2);
        assert_eq!(contexts[1].apn, "");
    }

    #[test]
    fn cgact_answers_which_contexts_are_up() {
        let answer = reply(&["+CGACT: 1,1", "+CGACT: 2,0", "OK"]);
        assert_eq!(parse_cgact(&answer), vec![(1, 1), (2, 0)]);
        assert!(parse_cgact(&reply(&["ERROR"])).is_empty());
    }

    /// The file's comments are where the resolution order is written down, and
    /// its commented `#APN=` line is the documented "not pinned" state: both
    /// have to survive a save.
    #[test]
    fn saving_an_apn_leaves_the_rest_of_the_file_alone() {
        let template = "# the override\n#\n#APN=cbnet\nOTHER=1\n";
        let saved = upsert_apn(template, "cbnet");
        assert!(saved.contains("# the override\n"), "{saved}");
        assert!(saved.contains("#APN=cbnet"), "{saved}");
        assert!(saved.contains("OTHER=1"), "{saved}");
        assert!(saved.ends_with("APN=cbnet\n"), "{saved}");
        assert!(!saved.contains("\n\n\n"), "blank lines stay as they were: {saved}");

        // an active line is replaced in place, and a second one is dropped
        let with_value = "# c\nAPN=3gnet\nAPN=old\nOTHER=1\n";
        let saved = upsert_apn(with_value, "cmnet");
        assert_eq!(saved, "# c\nAPN=cmnet\nOTHER=1\n");
    }

    /// An APN goes into an AT command inside quotes, so a value that carries a
    /// quote or a comma is refused rather than escaped: it would not be the APN
    /// anyone meant to type.
    #[test]
    fn an_apn_argument_is_checked_before_it_reaches_at() {
        let ok = "cbnet".to_string();
        assert_eq!(apn_argument(Some(&ok)).unwrap(), "cbnet");
        assert!(apn_argument(None).is_err());
        for bad in ["", " ", "cbn,et", "cb\"net", "cb net", "cb\nnet"] {
            let bad = bad.to_string();
            assert!(
                apn_argument(Some(&bad)).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn apn_source_reader_skips_comments() {
        let dir = std::env::temp_dir().join("unisoc-cpd-apn-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("mobile-data.conf");
        std::fs::write(&f, "# comment\nAPN=3gnet\nOTHER=1\n").unwrap();
        assert_eq!(
            apn_from_source(f.to_str().unwrap()).as_deref(),
            Some("3gnet")
        );
    }
}
