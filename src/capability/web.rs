//! W7: the web face of the resident owner.
//!
//! The web server is a **socket client**, never a channel owner: every button
//! and every poll becomes the same line-JSON request the CLI makes, and the
//! single-threaded serve loop stays the only thing that touches the CP.  That
//! is why this capability refuses to run without a socket path.

use super::{flag_value, positionals, Capability, Outcome};
use crate::context::Context;
use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the page listens when no address is given.
///
/// The systemd unit names its own interface (`0.0.0.0`, so the page is
/// reachable over the USB gadget network rather than only from the phone
/// itself), so the *port* is the part that has to stay in one place: the test
/// at the bottom of this file fails if the unit and this constant drift apart,
/// which is exactly how a deployment ends up serving a port the docs do not
/// name.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7887";

pub struct Web;

impl Capability for Web {
    fn name(&self) -> &'static str {
        "web"
    }

    fn summary(&self) -> &'static str {
        "W7 web UI: a browser face over a running serve (web [ADDR:PORT] --socket PATH; runs until killed)"
    }

    fn native_only(&self) -> bool {
        true
    }

    fn run(&self, _ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let Some(socket) = flag_value(args, "--socket").map(PathBuf::from) else {
            bail!("web needs --socket: it drives a running serve, never the channels directly");
        };
        let listen = positionals(args)
            .first()
            .cloned()
            .unwrap_or_else(|| DEFAULT_LISTEN.into());
        let listener =
            TcpListener::bind(&listen).with_context(|| format!("cannot listen on {listen}"))?;
        println!("unisoc-cpd web: http://{listen} (daemon on {})", socket.display());
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let socket = socket.clone();
            std::thread::spawn(move || {
                let _ = serve_conn(stream, &socket);
            });
        }
        unreachable!("the listener never ends");
    }
}

// ----------------------------------------------------------------- requests

fn serve_conn(mut stream: TcpStream, socket: &Path) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = String::new();
    if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf)?;
        body = String::from_utf8_lossy(&buf).into_owned();
    }
    let (path, _query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    route(&mut stream, &method, &path, &body, socket);
    Ok(())
}

fn ask(socket: &Path, request: &Value) -> Result<Value> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    let mut writer = stream.try_clone()?;
    writer.write_all(request.to_string().as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}

fn run_cap(socket: &Path, capability: &str, args: &[&str]) -> Value {
    let request = json!({ "action": "run", "capability": capability, "args": args });
    ask(socket, &request).unwrap_or_else(|e| json!({ "error": format!("{e}") }))
}

fn route(stream: &mut TcpStream, method: &str, path: &str, body: &str, socket: &Path) {
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(stream, "200 OK", "text/html; charset=utf-8", PAGE.to_string());
        }
        ("GET", "/api/state") => pass(stream, socket, &json!({ "action": "state" })),
        ("GET", "/api/urc") => pass(stream, socket, &json!({ "action": "urc", "limit": 40 })),
        ("GET", "/api/messages") => {
            pass(stream, socket, &json!({ "action": "messages", "limit": 50 }))
        }
        ("GET", "/api/status") => {
            let answer = json!({
                "register": run_cap(socket, "register", &["status"]),
                "signal": run_cap(socket, "signal", &[]),
                "ims": run_cap(socket, "ims", &["status"]),
            });
            respond_json(stream, &answer);
        }
        ("GET", "/api/info") => respond_json(stream, &api_info(socket)),
        ("GET", "/api/metrics") => respond_json(stream, &api_metrics(socket)),
        ("GET", "/api/identity") => respond_json(stream, &api_identity(socket)),
        ("GET", "/api/network") => respond_json(stream, &api_network(socket)),
        ("GET", "/api/apn") => respond_json(stream, &api_apn(socket)),
        ("POST", "/api/apn-set") => match apn_request(body) {
            Ok((apn, cid)) => {
                let cid = cid.to_string();
                respond_json(
                    stream,
                    &run_cap(socket, "data", &["set-apn", apn.as_str(), "--cid", cid.as_str()]),
                );
            }
            Err(e) => respond_json(stream, &json!({ "ok": false, "status": "error", "error": e })),
        },
        ("POST", "/api/apn-save") => match apn_request(body) {
            Ok((apn, _)) => respond_json(stream, &run_cap(socket, "data", &["save-apn", apn.as_str()])),
            Err(e) => respond_json(stream, &json!({ "ok": false, "status": "error", "error": e })),
        },
        ("POST", "/api/apn-clear") => {
            let cid = form_value(body, "cid").unwrap_or_default();
            let cid = cid.trim().to_string();
            if cid.is_empty() || !cid.chars().all(|c| c.is_ascii_digit()) {
                respond_json(stream, &json!({ "ok": false, "status": "error", "error": "cid must be a number" }));
            } else {
                respond_json(
                    stream,
                    &run_cap(socket, "data", &["clear-apn", "--cid", cid.as_str()]),
                );
            }
        }
        ("GET", "/api/bands") => respond_json(stream, &api_band_state(socket)),
        ("GET", "/api/cells") => respond_json(stream, &api_band_state(socket)),
        ("POST", "/api/band-lock") => match band_request(body) {
            Ok((rat, bands)) => {
                let numbers: Vec<String> = bands.iter().map(|b| b.to_string()).collect();
                let mut args: Vec<&str> = vec!["lock", rat.as_str()];
                args.extend(numbers.iter().map(|s| s.as_str()));
                respond_json(stream, &run_cap(socket, "band", &args));
            }
            Err(e) => respond_json(stream, &json!({ "ok": false, "status": "error", "error": e })),
        },
        ("POST", "/api/band-unlock") => {
            // No RAT means "both", which is what the panel's single button does.
            let rat = form_value(body, "rat").unwrap_or_default().trim().to_lowercase();
            let rat = if rat.is_empty() { "all".to_string() } else { rat };
            respond_json(stream, &run_cap(socket, "band", &["unlock", rat.as_str()]));
        }
        ("POST", "/api/cell-lock") => match cell_request(body) {
            Ok((rat, freq, pci)) => {
                let freq = freq.to_string();
                let pci = pci.to_string();
                respond_json(
                    stream,
                    &run_cap(socket, "band", &["cell-lock", rat.as_str(), &freq, &pci]),
                );
            }
            Err(e) => respond_json(stream, &json!({ "ok": false, "status": "error", "error": e })),
        },
        ("POST", "/api/cell-unlock") => {
            let rat = form_value(body, "rat").unwrap_or_default().trim().to_lowercase();
            let rat = if rat.is_empty() { "all".to_string() } else { rat };
            respond_json(stream, &run_cap(socket, "band", &["cell-unlock", rat.as_str()]));
        }
        ("POST", "/api/imei-write") => match imei_write_args(body) {
            Ok(args) => {
                let borrowed: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                respond_json(stream, &run_cap(socket, "imei", &borrowed));
            }
            Err(e) => respond_json(stream, &json!({ "ok": false, "status": "error", "error": e })),
        },
        ("POST", "/api/at") => {
            // The console is a thin front-end over the daemon's own `at`
            // capability: the command is parsed and sent by the process that
            // owns the channel, so the one-reader rule is not weakened by a
            // browser being able to type.  Nothing is sanitised beyond an
            // empty check -- the whole point is a raw console -- but what it
            // can reach is exactly what `unisoc-cpd at` can reach.
            let cmd = form_value(body, "cmd").unwrap_or_default();
            let cmd = cmd.trim().to_string();
            if cmd.is_empty() {
                respond_json(stream, &json!({ "ok": false, "status": "error", "error": "empty AT command" }));
            } else {
                respond_json(stream, &run_cap(socket, "at", &[cmd.as_str()]));
            }
        }
        ("POST", "/api/sms-delete") => {
            // `AT+CMGD=<index>` deletes out of whichever storage `+CPMS`
            // currently selects, so the index alone is what the AT surface
            // takes.  The message's storage is shown next to the button
            // precisely because the two can differ, and the list is re-read
            // afterwards rather than assumed.
            let index = form_value(body, "index").unwrap_or_default().trim().to_string();
            if index.is_empty()
                || !(index.chars().all(|c| c.is_ascii_digit()) || index.eq_ignore_ascii_case("all"))
            {
                respond_json(stream, &json!({ "ok": false, "status": "error", "error": "an index (or ALL) is required" }));
            } else {
                respond_json(stream, &run_cap(socket, "sms", &["delete", index.as_str()]));
            }
        }
        ("POST", "/api/send") => {
            let to = form_value(body, "to").unwrap_or_default();
            let text = form_value(body, "text").unwrap_or_default();
            respond_json(stream, &run_cap(socket, "sms", &["send", &to, &text]));
        }
        ("POST", "/api/dial") => {
            let number = form_value(body, "number").unwrap_or_default();
            respond_json(stream, &run_cap(socket, "call", &["dial", &number]));
        }
        ("POST", "/api/answer") => {
            respond_json(stream, &run_cap(socket, "call", &["answer"]));
        }
        ("POST", "/api/hangup") => {
            respond_json(stream, &run_cap(socket, "call", &["hangup"]));
        }
        _ => respond(stream, "404 Not Found", "text/plain; charset=utf-8", "not found\n".into()),
    }
}

fn pass(stream: &mut TcpStream, socket: &Path, request: &Value) {
    let answer = ask(socket, request).unwrap_or_else(|e| json!({ "error": format!("{e}") }));
    respond_json(stream, &answer);
}

// ------------------------------------------------------- the information panels

/// A capability's output lines, out of the daemon's JSON answer.
fn output_lines(answer: &Value) -> Vec<String> {
    answer
        .get("output")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// One `key: value` summary line a capability emitted, or `None`.
///
/// The convention the capabilities follow: after the raw AT echo (which is
/// indented, and prefixed `> CMD`), a capability that has data for a client
/// appends `key: value` lines at column zero.  Reading those is reading the
/// daemon's own answer — not re-parsing AT framing, which would make this
/// process a second, silent AT decoder.
///
/// `-` is the capabilities' "the modem did not report this", and is `None`
/// here for the same reason `decode_cesq` refuses to turn 255 into a number:
/// "absent" must not be rendered as a value.
fn summary(output: &[String], key: &str) -> Option<String> {
    summary_all(output, key).into_iter().next()
}

/// Every `key: value` line under one key.  The neighbour list repeats its key,
/// so "all of them" is a shape a panel needs and "the first one" is not.
fn summary_all(output: &[String], key: &str) -> Vec<String> {
    let prefix = format!("{key}:");
    output
        .iter()
        .filter_map(|line| {
            if line.starts_with(' ') || line.starts_with('>') {
                return None;
            }
            line.strip_prefix(prefix.as_str())
                .map(|v| v.trim().to_string())
                // `-` and `not reported` are the capabilities' two ways of
                // saying "the modem did not tell us", and neither is a value.
                .filter(|v| !v.is_empty() && v != "-" && v != "not reported")
        })
        .collect()
}

/// `+CSQ: 23,99` -> the RSSI in dBm.  37.003 maps the index as
/// `-113 + 2*index`, and 99 is "not known", not a very bad signal.
fn csq_rssi(output: &[String]) -> Option<i32> {
    let line = output
        .iter()
        .find(|l| l.trim_start().starts_with("+CSQ:"))?;
    let index: i32 = line.split_once(':')?.1.split(',').next()?.trim().parse().ok()?;
    (index <= 31).then(|| -113 + 2 * index)
}

/// A value out of the `decoded:` line `signal status` prints, e.g. `RSRP -80
/// dBm` or `SINR not reported`.  "not reported" is `None` -- `decode_cesq` went
/// to the trouble of keeping it apart from a reading, and it would be wasted
/// here.
fn decoded_field(line: Option<&String>, key: &str) -> Option<String> {
    let body = line?.split_once("decoded:")?.1;
    for piece in body.split(", ") {
        let Some(rest) = piece.trim().strip_prefix(&format!("{key} ")) else {
            continue;
        };
        let value = rest.split_whitespace().next().unwrap_or("");
        if value.is_empty() || value == "not" {
            return None;
        }
        return Some(value.to_string());
    }
    None
}

/// One `neighbor: LTE,band=3,earfcn=1650,…` line, as an object.
fn neighbor_entry(line: &str) -> Value {
    let mut fields = line.split(',');
    let mut map = serde_json::Map::new();
    map.insert(
        "rat".to_string(),
        json!(fields.next().unwrap_or_default().trim()),
    );
    for field in fields {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        // A band is a label (`78`, `n78`), not an amount: it stays text so a
        // panel never renders it as `78.0`.
        let value = match (key, value.parse::<f64>()) {
            ("band", _) => json!(value),
            (_, Ok(number)) => json!(number),
            (_, Err(_)) => json!(value),
        };
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

/// The APN panel: what the bearer would use, where that came from, what the
/// override file says, and the modem's own contexts -- which are three
/// different places, which is why they are reported side by side.
fn api_apn(socket: &Path) -> Value {
    let answer = run_cap(socket, "data", &["contexts"]);
    let out = output_lines(&answer);
    let contexts: Vec<Value> = summary_all(&out, "context")
        .iter()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(|f| f.trim()).collect();
            let cid = fields.first()?.parse::<u32>().ok()?;
            let field = |i: usize| {
                fields
                    .get(i)
                    .filter(|v| !v.is_empty() && **v != "-")
                    .map(|v| v.to_string())
            };
            Some(json!({
                "cid": cid,
                "pdp_type": field(1),
                "apn": field(2),
                "state": field(3),
            }))
        })
        .collect();
    json!({
        "apn": summary(&out, "apn"),
        "apn_source": summary(&out, "apn_source"),
        "saved_apn": summary(&out, "saved_apn"),
        "apn_source_path": summary(&out, "apn_source_path"),
        "cid": summary(&out, "cid").and_then(|v| v.parse::<u32>().ok()),
        "contexts": contexts,
        "status": answer.get("status"),
        "error": answer.get("error"),
    })
}

/// The APN form: a value, and the context to write it to.
///
/// The value is validated by the capability that builds the AT command (a
/// quote or a comma would make the command something else), so what is checked
/// here is only what the page needs to have a form at all.
fn apn_request(body: &str) -> Result<(String, u32), String> {
    let apn = form_value(body, "apn").unwrap_or_default().trim().to_string();
    if apn.is_empty() {
        return Err("an APN is required".to_string());
    }
    let cid = form_value(body, "cid").unwrap_or_default().trim().to_string();
    let cid = if cid.is_empty() { "1".to_string() } else { cid };
    match cid.parse::<u32>() {
        Ok(cid) if cid > 0 => Ok((apn, cid)),
        _ => Err(format!("cid must be a positive number, got {cid:?}")),
    }
}

/// The locked bands and cells, out of `band status`.
///
/// A lock is a `+SPLBAND`/`+SPFORCEFRQ` read-back, not a memory of what a
/// button asked for: the daemon reads the CP back after every write, so what
/// this returns is the modem's own answer.
fn api_band_state(socket: &Path) -> Value {
    let answer = run_cap(socket, "band", &["status"]);
    let mut value = band_state_of(&output_lines(&answer));
    value["status"] = answer.get("status").cloned().unwrap_or(Value::Null);
    value["error"] = answer.get("error").cloned().unwrap_or(Value::Null);
    value
}

/// The locked bands and cells, as the `band status` summary lines carry them.
fn band_state_of(out: &[String]) -> Value {
    let bands = |rat: &str| -> Vec<u32> {
        summary(out, &format!("{rat}_bands"))
            .map(|v| {
                v.split(',')
                    .filter_map(|b| b.trim().parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let cells = |rat: &str| -> Vec<Value> {
        summary(out, &format!("{rat}_cells"))
            .map(|v| {
                v.split_whitespace()
                    .filter_map(|pair| {
                        let (freq, pci) = pair.split_once('/')?;
                        Some(json!({
                            "freq": freq.parse::<u32>().ok()?,
                            "pci": pci.parse::<u32>().ok()?,
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    json!({
        "lte_bands": bands("lte"),
        "nr_bands": bands("nr"),
        "lte_cells": cells("lte"),
        "nr_cells": cells("nr"),
        "sprat": summary(out, "sprat"),
    })
}

/// Comma/space separated band numbers, or the token that was not one.
///
/// A token that is not a number fails the whole request rather than being
/// dropped: "78, n78" filtered to "78" and then locked would be a lock the
/// operator did not ask for, which is the kind of quiet success this daemon
/// does not do.
fn band_tokens(raw: &str) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for token in raw.split(|c: char| c == ',' || c == ' ' || c == '\n' || c == '\t') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match token.parse::<u32>() {
            Ok(band) => out.push(band),
            Err(_) => return Err(format!("{token:?} is not a band number")),
        }
    }
    if out.is_empty() {
        return Err("no band numbers given".to_string());
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn rat_of(body: &str) -> Result<String, String> {
    let rat = form_value(body, "rat").unwrap_or_default().trim().to_lowercase();
    match rat.as_str() {
        "lte" | "nr" => Ok(rat),
        "" => Err("a RAT is required (lte or nr)".to_string()),
        other => Err(format!("unknown RAT {other:?}: expected lte or nr")),
    }
}

fn band_request(body: &str) -> Result<(String, Vec<u32>), String> {
    let rat = rat_of(body)?;
    let bands = band_tokens(&form_value(body, "bands").unwrap_or_default())?;
    Ok((rat, bands))
}

fn cell_request(body: &str) -> Result<(String, u32, u32), String> {
    let rat = rat_of(body)?;
    let number = |key: &str| -> Result<u32, String> {
        let raw = form_value(body, key).unwrap_or_default();
        raw.trim()
            .parse::<u32>()
            .map_err(|_| format!("{key} must be a number, got {raw:?}"))
    };
    Ok((rat, number("freq")?, number("pci")?))
}

/// The capability arguments for an identity write, or why nothing was sent.
///
/// This is the page's half of a two-part guard.  The capability has its own
/// (`--yes`, the profile's `[nv].readonly`, a pinned write template, a fresh
/// NV backup and an independent read-back); what a browser adds is the part a
/// capability cannot: a second, deliberate entry of the value, and an
/// acknowledgement that the operator read what it costs.
///
/// The confirmation is the *value itself*, not a boolean.  A mistyped digit
/// has to fail here, in front of the person who typed it, rather than at the
/// modem.
fn imei_write_args(body: &str) -> Result<Vec<String>, String> {
    let imei = form_value(body, "imei").unwrap_or_default().trim().to_string();
    if imei.is_empty() {
        return Err("no IMEI given".to_string());
    }
    let confirm = form_value(body, "confirm").unwrap_or_default().trim().to_string();
    if confirm != imei {
        return Err("the confirmation does not match the IMEI; nothing was sent".to_string());
    }
    if form_value(body, "acknowledged").as_deref() != Some("yes") {
        return Err("the identity warning was not acknowledged; nothing was sent".to_string());
    }
    let index = form_value(body, "index").unwrap_or_default().trim().to_string();
    let index = if index.is_empty() { "0".to_string() } else { index };
    if index.parse::<u32>().map(|i| i > 2).unwrap_or(true) {
        return Err("index must be 0, 1 or 2 (SIM 1, SIM 2, spare)".to_string());
    }

    // `--yes` is passed on, never assumed: it is the capability's own gate and
    // the run summary records it.
    let mut args = vec![
        "write".to_string(),
        imei,
        "--index".to_string(),
        index,
        "--yes".to_string(),
    ];
    if form_value(body, "allow_bad_checksum").as_deref() == Some("yes") {
        args.push("--allow-bad-checksum".to_string());
    }
    Ok(args)
}

fn api_metrics(socket: &Path) -> Value {
    let status = run_cap(socket, "signal", &[]);
    let status_out = output_lines(&status);
    let serving = run_cap(socket, "signal", &["serving"]);
    let serving_out = output_lines(&serving);
    let neighbors = run_cap(socket, "signal", &["neighbors"]);
    let neighbors_out = output_lines(&neighbors);

    let decoded = status_out.iter().find(|l| l.contains("decoded:"));
    let number = |key: &str| -> Option<f64> {
        decoded_field(decoded, key).and_then(|v| v.parse::<f64>().ok())
    };
    let serving_of = |rat: &str| -> Value {
        let key = |suffix: &str| summary(&serving_out, &format!("serving_{rat}_{suffix}"));
        json!({
            "band": key("band"),
            "earfcn": key("earfcn").and_then(|v| v.parse::<u32>().ok()),
            "pci": key("pci").and_then(|v| v.parse::<u32>().ok()),
            "rsrp_dbm": key("rsrp").and_then(|v| v.parse::<f64>().ok()),
            "rsrq_db": key("rsrq").and_then(|v| v.parse::<f64>().ok()),
            "sinr_db": key("sinr").and_then(|v| v.parse::<f64>().ok()),
            "bandwidth": key("bandwidth"),
            "cell": key("cell"),
        })
    };
    let lte = serving_of("lte");
    let nr = serving_of("nr");
    let unscanned = |value: &Value| value.get("earfcn").map(|v| v.is_null()).unwrap_or(true);
    let serving_field = |value: &Value, key: &str| value.get(key).and_then(|v| v.as_f64());

    // Measured on the device, camped on NR SA: `+CESQ` answers "not reported"
    // for RSRP and RSRQ (every field 255, only the SS-SINR present), while the
    // serving-cell query carries both.  So the panel falls back to the serving
    // cell -- and says so, because the two are not the same measurement.
    let from_cesq = number("RSRP");
    let rsrp = from_cesq
        .or_else(|| serving_field(&nr, "rsrp_dbm"))
        .or_else(|| serving_field(&lte, "rsrp_dbm"));
    let rsrq = number("RSRQ")
        .or_else(|| serving_field(&nr, "rsrq_db"))
        .or_else(|| serving_field(&lte, "rsrq_db"));

    json!({
        "rssi_dbm": csq_rssi(&status_out),
        "rsrp_dbm": rsrp,
        "rsrp_source": rsrp.map(|_| if from_cesq.is_some() { "cesq" } else { "serving" }),
        "rsrq_db": rsrq,
        "sinr_db": number("SINR").or_else(|| serving_field(&nr, "sinr_db")),
        "lte": lte,
        "nr": nr,
        // "the CP did not report it" and "it reported nothing in range" are
        // two different facts, and the page must be able to say which.
        "serving_supported": !unscanned(&lte) || !unscanned(&nr),
        "neighbors_lte": summary(&neighbors_out, "neighbors_lte").and_then(|v| v.parse::<u32>().ok()),
        "neighbors_nr": summary(&neighbors_out, "neighbors_nr").and_then(|v| v.parse::<u32>().ok()),
        "neighbors": summary_all(&neighbors_out, "neighbor").iter().map(|l| neighbor_entry(l)).collect::<Vec<_>>(),
    })
}

/// `imei0 (SIM 1, item 5e81) = <15 digits>` — or the read's own error, which is
/// a fact about the device and is passed through rather than hidden.
fn imei_entries(output: &[String]) -> Vec<Value> {
    let mut entries = Vec::new();
    for line in output {
        let Some(rest) = line.strip_prefix("imei") else {
            continue;
        };
        let Some((index, tail)) = rest.split_once(' ') else {
            continue;
        };
        let Ok(index) = index.parse::<u32>() else {
            continue;
        };
        let slot = tail
            .split_once('(')
            .and_then(|(_, r)| r.split_once(','))
            .map(|(s, _)| s.trim().to_string());
        let (value, detail) = match tail.split_once(" = ") {
            Some((_, v)) => (
                v.trim()
                    .split_whitespace()
                    .next()
                    .filter(|v| !v.is_empty())
                    .map(|v| v.to_string()),
                None,
            ),
            None => (None, tail.split_once(": ").map(|(_, d)| d.trim().to_string())),
        };
        let luhn = value.as_deref().map(crate::identity::luhn_valid);
        entries.push(json!({
            "index": index,
            "slot": slot,
            "value": value,
            "luhn": luhn,
            "detail": detail,
        }));
    }
    entries
}

/// A field of a `+CEREG`-style summary: `n,stat,"tac","ci",act`.
fn reg_field(value: Option<&String>, index: usize) -> Option<String> {
    value?
        .split(',')
        .nth(index)
        .map(|f| f.trim().trim_matches('"').to_string())
        .filter(|f| !f.is_empty())
}

fn reg_state(value: Option<&String>) -> Option<i32> {
    reg_field(value, 1)?.parse().ok()
}

/// The baseband bar: who the CP is, out of `link info`.
fn api_info(socket: &Path) -> Value {
    let answer = run_cap(socket, "link", &["info"]);
    let out = output_lines(&answer);
    json!({
        "profile": summary(&out, "profile"),
        "model": summary(&out, "model"),
        "firmware": summary(&out, "firmware"),
        "hardware": summary(&out, "hardware"),
        "status": answer.get("status"),
        "error": answer.get("error"),
    })
}

/// The advanced panel: SIM and device identity, the bearer's address, and the
/// service-centre address.  Each part fails on its own, because one absent
/// fact (a card with no MSISDN, a diag node that is not up) must not blank the
/// rest of the panel.
fn api_identity(socket: &Path) -> Value {
    let sim = run_cap(socket, "sim", &["identity"]);
    let sim_out = output_lines(&sim);
    let imei = run_cap(socket, "imei", &["read"]);
    let data = run_cap(socket, "data", &["status"]);
    let data_out = output_lines(&data);
    let sms = run_cap(socket, "sms", &["status"]);
    let sms_out = output_lines(&sms);

    let mut errors = Vec::new();
    for (what, answer) in [
        ("sim identity", &sim),
        ("imei read", &imei),
        ("data status", &data),
        ("sms status", &sms),
    ] {
        if let Some(e) = answer.get("error").and_then(|v| v.as_str()) {
            errors.push(format!("{what}: {e}"));
        } else if answer.get("status").and_then(|v| v.as_str()) == Some("fail") {
            errors.push(format!("{what}: the daemon reported a failure"));
        }
    }

    json!({
        "iccid": summary(&sim_out, "iccid"),
        "imsi": summary(&sim_out, "imsi"),
        "phone": summary(&sim_out, "phone"),
        "imei": imei_entries(&output_lines(&imei)),
        "ip": summary(&data_out, "ip"),
        "ip6": summary(&data_out, "ip6"),
        "ip6_dns": summary(&data_out, "ip6_dns"),
        "ip6_interface": summary(&data_out, "ip6_interface"),
        "apn": summary(&data_out, "apn"),
        "dns": summary(&data_out, "dns"),
        "smsc": summary(&sms_out, "smsc"),
        "errors": errors,
    })
}

/// The network panel: the operator, and which generation the UE is actually
/// registered on.  "SA" is claimed only on the evidence `+C5GREG` carries --
/// the same gate `nr status` and the Android-side helper use.
fn api_network(socket: &Path) -> Value {
    let reg = run_cap(socket, "register", &["status"]);
    let reg_out = output_lines(&reg);
    let ops = run_cap(socket, "operator", &["status"]);
    let ops_out = output_lines(&ops);

    let cereg = summary(&reg_out, "cereg");
    let c5greg = summary(&reg_out, "c5greg");
    let creg = summary(&reg_out, "creg");
    let gatt = summary(&reg_out, "gatt");

    let registered = |v: Option<&String>| matches!(reg_state(v), Some(1) | Some(5));
    let sa = registered(c5greg.as_ref());
    let ps = registered(cereg.as_ref());
    let act: Option<i32> = reg_field(cereg.as_ref(), 4).and_then(|a| a.parse().ok());
    let mode = if sa {
        "5G SA"
    } else if ps {
        match act {
            Some(7) => "4G LTE",
            Some(11) | Some(13) => "5G NSA",
            Some(10) | Some(12) => "5G",
            _ => "已注册",
        }
    } else {
        "无服务"
    };

    let numeric = summary(&ops_out, "operator_numeric");
    // The CP's own name form when it answered with one; otherwise this build's
    // MCC-MNC table, because a bare numeric is not what an operator field is
    // for.  Neither is guessed at: an unknown code stays unknown.
    let name = summary(&ops_out, "operator_name").or_else(|| {
        numeric
            .as_deref()
            .and_then(crate::capability::control::operator_name)
            .map(|s| s.to_string())
    });

    json!({
        "operator_numeric": numeric,
        "operator_name": name,
        "operator_act": summary(&ops_out, "operator_act"),
        "cereg": cereg,
        "creg": creg,
        "c5greg": c5greg,
        "gatt": gatt,
        "registered": ps || sa,
        "sa": sa,
        "mode": mode,
        "error": reg.get("error").or_else(|| ops.get("error")),
    })
}


fn respond_json(stream: &mut TcpStream, value: &Value) {
    respond(stream, "200 OK", "application/json", value.to_string());
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: String) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) =
                ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
            {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        if b[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----------------------------------------------------------------- the page

const PAGE: &str = r#"<!doctype html>
<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<meta name="color-scheme" content="light">
<meta name="theme-color" content='#FFFBFE'>
<title>unisoc-cpd</title>
<style>
/* Material You (MD3), light.  No external font or stylesheet: this page is
   served out of the daemon's own memory and read on a handset that may have no
   data path at all -- which is exactly when it is needed.  Roboto is the
   design's typeface and is present on Android; the CJK faces behind it are what
   the interface actually renders in. */
:root{
  --p:#6750A4; --on-p:#FFFFFF; --p-c:#EADDFF; --on-p-c:#21005D;
  --s-c:#E8DEF8; --on-s-c:#1D192B;
  --t-c:#FFD8E4; --on-t-c:#31111D;
  --err:#B3261E; --on-err:#FFFFFF; --err-c:#F9DEDC; --on-err-c:#410E0B;
  --bg:#FFFBFE; --surf:#FFFBFE; --sc:#F3EDF7; --sc-low:#E7E0EC;
  --on:#1C1B1F; --on-v:#49454F; --out:#79747E; --out-v:#CAC4D0;
  --ok-c:#D7F3DE; --ok-f:#0B5227; --warn-c:#FFE8C2; --warn-f:#6B4200;
  --e1:0 1px 2px rgba(28,27,31,.10),0 1px 3px rgba(28,27,31,.06);
  --e2:0 2px 6px rgba(28,27,31,.13),0 1px 3px rgba(28,27,31,.08);
  --e3:0 10px 24px rgba(28,27,31,.16),0 2px 6px rgba(28,27,31,.08);
  --r-s:12px; --r-m:16px; --r-l:24px; --r-xl:28px; --full:9999px;
  --ease:cubic-bezier(.2,0,0,1);
}
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--on);line-height:1.5;
  font-family:Roboto,"Roboto Flex","Noto Sans SC","Noto Sans CJK SC","PingFang SC","Microsoft YaHei",system-ui,sans-serif;
  padding-bottom:44px}

/* Signature MD3 atmosphere: organic blurred shapes behind the content, never
   in front of it, and never the only thing carrying meaning. */
.aura{position:fixed;inset:0;overflow:hidden;pointer-events:none;z-index:0}
.aura i{position:absolute;display:block;border-radius:50%;filter:blur(64px);opacity:.55}
.aura i:nth-child(1){width:300px;height:300px;background:var(--p-c);top:-110px;right:-80px}
.aura i:nth-child(2){width:260px;height:260px;background:var(--t-c);top:150px;left:-110px}
.aura i:nth-child(3){width:220px;height:220px;background:var(--s-c);bottom:-70px;right:-30px}

.appbar{position:sticky;top:0;z-index:5;padding:10px 16px 12px;
  background:rgba(255,251,254,.86);backdrop-filter:blur(14px);-webkit-backdrop-filter:blur(14px);
  border-bottom:1px solid var(--out-v)}
/* The bar is full-bleed, its contents are not: on a wide window the title has
   to line up with the cards below it, not with the edge of the screen. */
.appbar>.bar{max-width:760px;margin:0 auto}
.appbar h1{margin:0;font-size:1.375rem;font-weight:500;letter-spacing:0}
.hero{display:flex;flex-wrap:wrap;align-items:center;gap:6px;margin:8px 0 6px;font-size:.8125rem;color:var(--on-v)}
.hero b{font-weight:500;color:var(--on)}
.hero em{font-style:normal;background:var(--s-c);color:var(--on-s-c);border-radius:var(--full);padding:2px 10px;font-size:.75rem}

.wrap{position:relative;z-index:1;max-width:760px;margin:0 auto;padding:0 16px}
.chips{display:flex;flex-wrap:wrap;gap:6px;margin:8px 0}
.chip{display:inline-flex;align-items:center;background:var(--s-c);color:var(--on-s-c);
  border-radius:var(--full);padding:5px 12px;font-size:.75rem;font-weight:500;letter-spacing:.01em;
  transition:background-color .2s var(--ease),box-shadow .3s var(--ease)}
.chip.ok{background:var(--ok-c);color:var(--ok-f)}
.chip.bad{background:var(--err-c);color:var(--on-err-c)}
.chip.warn{background:var(--warn-c);color:var(--warn-f)}
.chip.dim{background:var(--sc-low);color:var(--on-v)}

.card{background:var(--sc);border-radius:var(--r-l);padding:4px 16px 16px;margin:14px 0;
  box-shadow:var(--e1);transition:box-shadow .3s var(--ease)}
.card:hover{box-shadow:var(--e2)}
.card[open]{box-shadow:var(--e2)}
.card>summary{list-style:none;cursor:pointer;display:flex;align-items:center;gap:10px;
  padding:15px 0;font-size:.9375rem;font-weight:500;color:var(--on)}
.card>summary::-webkit-details-marker{display:none}
.card>summary:focus-visible{outline:2px solid var(--p);outline-offset:4px;border-radius:var(--full)}
.chev{margin-left:auto;width:9px;height:9px;flex:0 0 auto;border-right:2px solid var(--on-v);
  border-bottom:2px solid var(--on-v);transform:rotate(45deg) translate(-2px,-2px);
  transition:transform .3s var(--ease)}
.card[open] .chev{transform:rotate(-135deg) translate(-2px,-2px)}
.sub{font-size:.6875rem;font-weight:400;color:var(--on-v);
  font-family:"Roboto Mono",ui-monospace,SFMono-Regular,Menlo,monospace}
h2{margin:18px 0 4px;font-size:1rem;font-weight:500}
.card h2:first-child{margin-top:18px}

.row{display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin:10px 0}
.row.tight{gap:6px;margin:6px 0}

.btn{appearance:none;border:0;cursor:pointer;font:inherit;font-size:.875rem;font-weight:500;
  letter-spacing:.01em;height:40px;padding:0 22px;border-radius:var(--full);
  display:inline-flex;align-items:center;justify-content:center;gap:8px;
  background:var(--p);color:var(--on-p);
  transition:box-shadow .3s var(--ease),background-color .2s var(--ease),transform .12s var(--ease)}
.btn:hover{box-shadow:var(--e1);background-image:linear-gradient(rgba(255,255,255,.14),rgba(255,255,255,.14))}
.btn:active{transform:scale(.95)}
.btn:focus-visible{outline:2px solid var(--p);outline-offset:2px}
.btn.tonal{background:var(--s-c);color:var(--on-s-c)}
.btn.tonal:hover{background-image:linear-gradient(rgba(29,25,43,.10),rgba(29,25,43,.10))}
.btn.out{background:transparent;color:var(--p);box-shadow:inset 0 0 0 1px var(--out)}
.btn.out:hover{background-color:rgba(103,80,164,.08);
  background-image:none;box-shadow:inset 0 0 0 1px var(--out)}
.btn.text{background:transparent;color:var(--p);padding:0 14px}
.btn.text:hover{background-color:rgba(103,80,164,.10);background-image:none;box-shadow:none}
.btn.red{background:var(--err);color:var(--on-err)}
.btn.sm{height:32px;padding:0 14px;font-size:.75rem}

input:not([type=checkbox]),select{font:inherit;font-size:.875rem;height:48px;padding:0 14px;
  border:0;border-bottom:2px solid var(--out);border-radius:var(--r-s) var(--r-s) 0 0;
  background:var(--sc-low);color:var(--on);
  transition:border-color .2s var(--ease),background-color .2s var(--ease)}
/* Width comes from the row, not from `size`: the UA sizes a field by its
   average character, and the CJK fallback behind this interface makes that
   average twice as wide as it looks, so `size="16"` was taking two thirds of
   the screen. */
.row>input:not([type=checkbox]){flex:1 1 7rem;min-width:5rem}
.row>select{flex:0 0 auto;min-width:76px}
.tag{flex:0 0 auto;font-size:.75rem;color:var(--on-v)}
/* A number field holds one number, not a paragraph: it must not claim the row
   and push the rest of it off the card.  The element is named in the selector
   because `.row>input:not(...)` is otherwise the more specific rule, and a
   losing rule is a rule that does nothing. */
.row>input.w-num,.row>select.w-num{flex:0 1 6.5rem;min-width:4.5rem}
/* A label and the field it names wrap together or not at all: a line break
   between them reads as a label for whatever comes next. */
.pair{display:inline-flex;align-items:center;gap:6px;flex:0 0 auto}
.pair>input,.pair>select{flex:0 0 auto;width:6rem}
input:not([type=checkbox]):focus,select:focus{outline:none;border-bottom-color:var(--p);background:#EFE7F3}
input::placeholder{color:var(--on-v);opacity:.7}
input[type=checkbox]{width:18px;height:18px;accent-color:var(--p);vertical-align:-3px}
label{display:inline-flex;align-items:center;gap:6px;font-size:.8125rem;color:var(--on-v);margin:2px 0}

.kv{font-size:.8125rem;line-height:1.7;color:var(--on);margin:6px 0}
.kv>div{display:flex;gap:10px;padding:4px 0;border-bottom:1px solid rgba(121,116,126,.14)}
.kv>div:last-child{border-bottom:0}
.kv b{flex:0 0 auto;min-width:98px;font-weight:500;color:var(--on-v)}
.kv .dim{color:var(--on-v);opacity:.65}
.err{color:var(--err);font-size:.75rem;padding:2px 0}
.warn{background:var(--warn-c);color:var(--warn-f);border-radius:var(--r-m);
  padding:12px 14px;margin:10px 0;font-size:.75rem;line-height:1.65}
.warn code{background:rgba(107,66,0,.14);border-radius:4px;padding:0 4px}
code{font-family:"Roboto Mono",ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.75rem}
.note{font-size:.6875rem;line-height:1.6;color:var(--on-v);margin:8px 0}

pre{background:#EDE6F4;color:var(--on);border-radius:var(--r-s);padding:12px;margin:10px 0;
  font-family:"Roboto Mono",ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.75rem;line-height:1.55;
  min-height:18px;max-height:280px;overflow:auto;white-space:pre-wrap;word-break:break-word}

.msg{background:var(--surf);border-radius:var(--r-m);padding:12px 14px;margin:8px 0;
  font-size:.8125rem;line-height:1.55;box-shadow:var(--e1);
  transition:box-shadow .3s var(--ease)}
.msg:hover{box-shadow:var(--e2)}
.msg .from{font-size:.875rem;font-weight:500;color:var(--p)}
.msg .meta{font-size:.6875rem;color:var(--on-v);margin-left:6px}
.msg .body{margin-top:6px;white-space:pre-wrap;word-break:break-word}
.msg .row{justify-content:flex-end;margin:8px 0 0}

.hist-row{display:flex;flex-wrap:wrap;align-items:center;gap:6px;margin:6px 0}
.hist{display:inline-flex;align-items:center;border:0;background:var(--sc-low);color:var(--on-v);
  border-radius:var(--full);padding:5px 11px;font-size:.6875rem;
  font-family:"Roboto Mono",ui-monospace,Menlo,monospace;cursor:pointer;
  transition:background-color .2s var(--ease),color .2s var(--ease),transform .12s var(--ease)}
.hist:hover{background:var(--p-c);color:var(--on-p-c)}
.hist:active{transform:scale(.95)}
.hist:focus-visible{outline:2px solid var(--p);outline-offset:2px}

/* The neighbour list is a table because it is one: seven numbers per row, and
   columns only mean anything if they line up.  It scrolls sideways inside its
   card rather than wrapping, because wrapping is what would cost the alignment
   the table exists for. */
.scroll{overflow-x:auto;-webkit-overflow-scrolling:touch;margin:8px 0}
.tbl{border-collapse:collapse;width:100%;min-width:470px;font-size:.75rem}
.tbl th,.tbl td{padding:7px 9px;text-align:left;white-space:nowrap;
  border-bottom:1px solid rgba(121,116,126,.16)}
.tbl th{font-size:.6875rem;font-weight:500;color:var(--on-v);background:var(--sc-low)}
.tbl th:first-child{border-top-left-radius:var(--r-s)}
.tbl th:last-child{border-top-right-radius:var(--r-s)}
.tbl th .u{display:block;font-size:.625rem;font-weight:400;opacity:.75}
.tbl .num{text-align:right;font-variant-numeric:tabular-nums}
.tbl .act{text-align:left;width:1%;padding-right:2px}
.tbl tbody tr:last-child td{border-bottom:0}
.tbl td .btn{height:28px;padding:0 12px;font-size:.6875rem}
.tbl .note{font-size:.6875rem;color:var(--on-v);padding:6px 0}

#banner{display:none;position:fixed;inset:0;z-index:20;align-items:center;justify-content:center;
  padding:24px;background:rgba(28,27,31,.44);backdrop-filter:blur(6px);-webkit-backdrop-filter:blur(6px)}
#banner.on{display:flex}
.banner-card{background:var(--surf);border-radius:var(--r-xl);padding:24px;width:100%;max-width:420px;
  text-align:center;box-shadow:var(--e3)}
.banner-card .who{font-size:1.5rem;font-weight:500;margin:2px 0 20px}
.banner-card .row{justify-content:center;margin:0}

/* A finger is not a mouse: the small variant is a dense control, and on a
   touch screen it still has to offer a target worth aiming at. */
@media (pointer: coarse){
  .btn.sm,.hist{min-height:40px}
  .btn.sm{padding:0 16px}
}
@media (prefers-reduced-motion: reduce){
  *{transition-duration:.01ms !important;animation:none !important}
  .btn:active,.hist:active{transform:none}
}
</style></head><body>
<div class="aura" aria-hidden="true"><i></i><i></i><i></i></div>

<header class="appbar"><div class="bar">
 <h1>unisoc-cpd</h1>
 <div class="hero" id="baseband">…</div>
 <div class="chips" id="radio">…</div>
</div></header>

<main class="wrap">
 <div class="chips" id="locks">…</div>
 <div class="chips" id="chips"></div>

<details class="card" id="d-sms" open><summary>短信 · inbox<span class="sub">单条删除</span><span class="chev" aria-hidden="true"></span></summary>
 <div id="msgs">…</div>
 <h2>发短信</h2>
 <div class="row"><input id="to" placeholder="+86…" autocomplete="off">
  <input id="text" placeholder="内容" autocomplete="off">
  <button class="btn" onclick="sendSms()">发送</button></div>
 <pre id="sms-out"></pre>
 <div class="note">删除按索引提交，落在 CP 当前选中的存储上（+CPMS）；每行标出的存储是这条消息
 被读到时所在的存储，两者不一致时以删除后重新列出的结果为准。</div></details>

<section class="card">
 <h2>电话</h2>
 <div class="row"><input id="num" placeholder="号码" autocomplete="off">
  <button class="btn" onclick="dial()">呼叫</button>
  <button class="btn tonal" onclick="act('answer')">接听</button>
  <button class="btn red" onclick="act('hangup')">挂断</button></div>
 <pre id="call-out"></pre></section>

<details class="card" id="d-identity"><summary>高级信息<span class="sub">ICCID · IMEI · IMSI · IP · IPv6 · SMSC</span><span class="chev" aria-hidden="true"></span></summary>
 <div class="row tight"><button class="btn tonal sm" onclick="loadIdentity()">刷新</button></div>
 <div class="kv" id="identity">展开后读取…</div></details>

<details class="card" id="d-apn"><summary>APN 管理<span class="sub">解析结果 · 上下文 · 覆盖文件</span><span class="chev" aria-hidden="true"></span></summary>
 <div class="warn">APN 有三个地方，别混：① <b>Modem 的 PDP 上下文</b>——承载真正用的是它；
 ② <b>覆盖文件</b>——解析顺序里排在 Modem 前面，重启后仍然生效；③ SIM/表的兜底——不可写。
 写入 Modem 后要重建承载才生效（<code>data down</code> 再 <code>data up</code>）；保存到覆盖文件则要重跑承载服务。</div>
 <div class="row tight"><button class="btn tonal sm" onclick="loadApn()">刷新</button></div>
 <div class="kv" id="apn-state">展开后读取…</div>
 <div class="row"><input id="apn-value" placeholder="新 APN" autocomplete="off">
  <span class="tag">CID</span><select id="apn-cid"></select>
  <button class="btn" onclick="apnSet()">写入 Modem</button>
  <button class="btn tonal" onclick="apnSave()">保存到覆盖文件</button>
  <button class="btn red" onclick="apnClear()">清除上下文</button></div>
 <pre id="apn-out">…</pre></details>

<details class="card" id="d-metrics"><summary>信号详情<span class="sub">RSSI · RSRP · RSRQ · SINR · 服务小区</span><span class="chev" aria-hidden="true"></span></summary>
 <div class="row tight"><button class="btn tonal sm" onclick="loadMetrics()">刷新</button></div>
 <div class="kv" id="metrics">展开后读取…（要探测测量类 AT，可能较慢）</div></details>

<details class="card" id="d-network"><summary>网络<span class="sub">运营商 · 5G SA/NSA · 注册状态</span><span class="chev" aria-hidden="true"></span></summary>
 <div class="row tight"><button class="btn tonal sm" onclick="loadNetwork()">刷新</button></div>
 <div class="kv" id="network">展开后读取…</div></details>

<details class="card" id="d-bands"><summary>锁频段<span class="sub">AT+SPLBAND</span><span class="chev" aria-hidden="true"></span></summary>
 <div class="warn">锁到当前网络用不到的频段会直接失去服务（一直无信号直到解锁）。下面显示的是 CP 读回的
 当前锁定，不是你刚按下的按钮——写入后守护进程会立刻读回比对。</div>
 <div class="row tight"><button class="btn tonal sm" onclick="loadLocks()">刷新</button></div>
 <div class="kv" id="bands-state">展开后读取…</div>
 <div class="row"><span class="tag">LTE</span><input id="lte-bands" placeholder="1,3,41" autocomplete="off">
  <button class="btn" onclick="bandLock('lte')">锁定</button>
  <button class="btn red" onclick="bandUnlock('lte')">解锁</button></div>
 <div id="band-quick-lte" class="hist-row"></div>
 <div class="row"><span class="tag">NR</span><input id="nr-bands" placeholder="41,78" autocomplete="off">
  <button class="btn" onclick="bandLock('nr')">锁定</button>
  <button class="btn red" onclick="bandUnlock('nr')">解锁</button></div>
 <div id="band-quick-nr" class="hist-row"></div>
 <div class="row"><button class="btn out" onclick="bandUnlock('')">LTE + NR 全部解锁</button></div>
 <pre id="bands-out">…</pre></details>

<details class="card" id="d-cells"><summary>锁基站 · 邻区<span class="sub">AT+SPFORCEFRQ</span><span class="chev" aria-hidden="true"></span></summary>
 <div class="warn">锁基站比锁频段更紧：锁到一个不可用的小区会一直无服务。下表里每一行的「锁」会把那行的参数
 填进下面的表单，但仍要你按一下才会写。</div>
 <div class="row tight"><button class="btn tonal sm" onclick="refreshCells()">刷新</button></div>
 <div class="kv" id="cells-state">展开后读取…</div>
 <div id="neighbors"><div class="note">邻区：展开后读取（要探测测量类 AT，可能较慢）</div></div>
 <h2>手动锁定</h2>
 <div class="row"><select id="cell-rat"><option value="lte">LTE</option><option value="nr">NR</option></select>
  <span class="pair"><span class="tag">EARFCN</span><input class="w-num" id="cell-freq" autocomplete="off"></span>
  <span class="pair"><span class="tag">PCI</span><input class="w-num" id="cell-pci" autocomplete="off"></span>
  <button class="btn" onclick="cellLock()">锁定</button>
  <button class="btn red" onclick="cellUnlock()">解锁</button></div>
 <pre id="cells-out">…</pre></details>

<details class="card" id="d-imei"><summary>IMEI<span class="sub">读 · 写入（危险 · 二次确认）</span><span class="chev" aria-hidden="true"></span></summary>
 <div class="row tight"><button class="btn tonal sm" onclick="loadIdentity()">刷新</button></div>
 <div class="kv" id="imei-read">展开后读取…</div>
 <div class="warn">写 IMEI 是永久改变这台设备身份的操作，且不可从这里撤销。只对你自己拥有的硬件做：
 恢复被刷坏的出厂值，或给实验机编一个。把设备伪装成另一台在很多司法辖区是犯罪，运营商也会按 IMEI 拉黑。
 守护进程那一侧还压着三道闸：profile 的 <code>[nv].readonly</code>、必须已固定的
 <code>[imei].write_command</code> 模板、写前强制 NV 备份以及写后 diag 独立读回；任何一道不过就是 fail。</div>
 <div class="row"><span class="tag">索引</span><select id="imei-index"><option value="0">0 · SIM 1</option>
  <option value="1">1 · SIM 2</option><option value="2">2 · spare</option></select></div>
 <div class="row"><input id="imei-value" placeholder="新 IMEI（15 位数字）" autocomplete="off"></div>
 <div class="row"><input id="imei-confirm" placeholder="再输入一次（确认）" autocomplete="off"></div>
 <div><label><input type="checkbox" id="imei-ack"> 我确认这是我拥有的设备，并已读完上面的警告</label></div>
 <div><label><input type="checkbox" id="imei-bad"> 允许校验位不通过（只给实验用的假值）</label></div>
 <div class="row"><button class="btn red" onclick="imeiWrite()">写入 IMEI</button></div>
 <pre id="imei-out">…</pre></details>


<section class="card">
 <h2>自定义 AT 控制台</h2>
 <div class="warn">这是直通 CP 的原始 AT 通道。查询类（<code>AT+CSQ</code>、<code>AT+CEREG?</code>）是安全的；
 但写类命令（<code>AT+SPLBAND=1,…</code>、<code>AT+SPFORCEFRQ=…</code>、<code>AT+SPIMEI=…</code>、任何厂商
 NV 命令）会立刻并可能永久改变基带配置 / NV，写错可能失联直到恢复出厂。
 命令由持有唯一 AT 通道的守护进程执行，本页只做转发。</div>
 <div class="row"><input id="at-cmd" placeholder="AT+CSQ" autocomplete="off">
  <button class="btn" onclick="sendAt()">发送</button>
  <button class="btn text" onclick="clearAtHist()">清空历史</button></div>
 <pre id="at-out">…</pre>
 <div id="at-hist" class="hist-row"></div></section>

<section class="card">
 <h2>事件流 · urc</h2>
 <div class="row tight"><button class="btn tonal sm" onclick="clearUrc()">清空显示</button>
  <button class="btn text" onclick="showAllUrc()">显示全部</button></div>
 <div class="note" id="urc-note"></div>
 <pre id="urc-out">…</pre></section>

<details class="card" id="d-stat"><summary>状态详情<span class="sub">register · signal · ims</span><span class="chev" aria-hidden="true"></span></summary>
 <pre id="stat-out">…</pre></details>
</main>

<div id="banner" role="dialog" aria-modal="true" aria-labelledby="bnr-txt">
 <div class="banner-card">
  <div class="who" id="bnr-txt">+CRING: VOICE</div>
  <div class="row"><button class="btn" onclick="act('answer')">接听</button>
   <button class="btn red" onclick="act('hangup')">挂断</button></div>
 </div></div>

<script>
function out(id, resp){ document.getElementById(id).textContent =
  (resp.output||[]).join('\n') + (resp.status ? ('\n['+resp.status+']') : '')
  + (resp.error ? ('\n[error] '+resp.error) : ''); }
async function get(u){ const r = await fetch(u); return r.json(); }
async function post(u, data){ const r = await fetch(u, {method:'POST',
  headers:{'Content-Type':'application/x-www-form-urlencoded'},
  body: new URLSearchParams(data).toString()}); return r.json(); }
function chip(list, label, cls){
  return '<span class="chip '+(cls||'')+'">'+label+'</span>'; }
async function refreshState(){ try{ const d = await get('/api/state'); const s = d.state||{};
  const a = s.at||{}; const c = (s.channels||{}).cmd||{};
  const age = (s.last_ok_age_s==null) ? 'never' : Math.round(s.last_ok_age_s)+'s';
  document.getElementById('chips').innerHTML =
    chip(0,'AT ok '+ (a.ok||0) +'/'+ (a.commands||0)) +
    chip(0,'timeout ' + (a.timeouts||0), (a.timeouts||0)>0?'bad':'ok') +
    chip(0,'opens ' + (c.opens||0)) +
    chip(0,'last_ok ' + age, (s.last_ok_age_s!=null && s.last_ok_age_s<120)?'ok':'dim');
 }catch(e){} }
async function refreshStatus(){ try{ const d = await get('/api/status');
  const ro = (d.register.output||[]).join(' ');
  const so = (d.signal.output||[]).join(' ');
  const io = (d.ims.output||[]).join(' ');
  const nr = /C5GREG:\s*\d+,1/.test(ro) || /CEREG:\s*\d+,1/.test(ro);
  let sig = '—';
  const sinr = so.match(/SINR ([0-9.]+) dB/); const rsrp = so.match(/RSRP (-[0-9]+) dB/);
  if(rsrp) sig = 'RSRP '+rsrp[1]; else if(sinr) sig = 'SINR '+sinr[1]+'dB';
  let ims = 'IMS ?';
  if(/IMS registered/.test(io)) ims = 'IMS 已注册';
  else if(/not registered/.test(io)) ims = 'IMS 未注册';
  else if(/VoLTE enabled/.test(io)) ims = 'IMS 待注';
  document.getElementById('radio').innerHTML =
    chip(0, nr ? 'NR 已注册' : '无服务', nr ? 'ok' : 'bad') +
    chip(0, '信号 '+sig, nr ? 'ok' : 'dim') +
    chip(0, ims, /已注册/.test(ims) ? 'ok' : 'bad');
  document.getElementById('stat-out').textContent =
    '-- register --\n' + (d.register.output||[]).join('\n') +
    '\n-- signal --\n' + (d.signal.output||[]).join('\n') +
    '\n-- ims --\n' + (d.ims.output||[]).join('\n');
 }catch(e){} }
function fmtUrc(u){ const o = u.urc;
  if(o==null) return u.at;
  if(typeof o==='string') return u.at+'  '+o;
  const parts=[o.kind||'urc'];
  for(const k in o){ if(k==='kind') continue;
    const v=o[k]; if(v===null||v===undefined||v==='') continue;
    parts.push(k+'='+v); }
  return u.at+'  '+parts.join(' '); }
function isRing(u){ const o=u.urc;
  if(o&&typeof o==='object') return o.kind==='urc-incoming-call';
  return typeof o==='string' && o.indexOf('+CRING')===0; }
var urcWatermark = null;
var urcLast = [];
function refreshUrcNote(){
  document.getElementById('urc-note').textContent = urcWatermark
    ? ('已清空显示：水位线 ' + urcWatermark + '（只隐藏本页，daemon 的缓冲没动）')
    : '';
}
function clearUrc(){
  // The watermark is the newest event already on screen: clearing is a
  // display filter in this page, and the daemon's ring keeps every event.
  if(urcLast.length) urcWatermark = urcLast[urcLast.length-1].at;
  refreshUrcNote(); refreshUrc();
}
function showAllUrc(){ urcWatermark = null; refreshUrcNote(); refreshUrc(); }
async function refreshUrc(){ try{ const d = await get('/api/urc'); const us = d.urcs||[];
  urcLast = us;
  const shown = urcWatermark ? us.filter(function(u){ return u.at > urcWatermark; }) : us;
  document.getElementById('urc-out').textContent = shown.map(fmtUrc).join('\n');
  // The incoming-call banner reads the unfiltered list: silencing a ringer is
  // not what "clear the log" means.
  const ring = us.filter(isRing);
  const b = document.getElementById('banner');
  if(ring.length){ if(!b.classList.contains('on')){ b.classList.add('on');
    const o = ring[ring.length-1].urc;
    document.getElementById('bnr-txt').textContent = (o&&o.kind) ? '来电 '+(o.number||'') : String(o); } }
  else { b.classList.remove('on'); }
 }catch(e){} }
async function refreshMsgs(){ try{ const d = await get('/api/messages'); const ms = d.messages||[];
  document.getElementById('msgs').innerHTML = ms.map(function(m){
   return '<div class="msg"><span class="from">'+esc(m.from)+'</span>'
    + '<span class="meta">'+esc((m.storage||'')+'['+m.index+'] '+(m.timestamp||''))+'</span>'
    + '<div class="body">'+esc(m.text||'')+'</div>'
    + '<div class="row"><button class="btn red sm"'
    + ' onclick="delSms('+m.index+')">删除</button></div></div>';}).join('')
   || '<div class="note">（空）</div>';
 }catch(e){} }
async function delSms(index){
  // A delete is the one thing here that changes what is stored on the SIM or
  // in the modem, so it asks first.
  if(!window.confirm('删除索引 '+index+' 这条短信？（落在 CP 当前选中的存储上，不可撤销）')) return;
  out('sms-out', await post('/api/sms-delete', {index: index}));
  refreshMsgs(); }
async function sendSms(){ const r = await post('/api/send',
  {to:document.getElementById('to').value, text:document.getElementById('text').value});
 out('sms-out', r); refreshMsgs(); }
async function dial(){ const r = await post('/api/dial', {number:document.getElementById('num').value});
 out('call-out', r); }
async function act(w){ const r = await post('/api/'+w, {}); out('call-out', r);
 if(w!=='hangup') setTimeout(function(){document.getElementById('banner').classList.remove('on');}, 800); }

function esc(s){ return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;'); }
function kv(label, value){
  return '<div><b>'+label+'</b>'+(value==null||value===''
    ? '<span class="dim">未上报</span>' : esc(value))+'</div>'; }
async function refreshInfo(){ try{ const d = await get('/api/info');
  // The keys are the ones `link info` actually emits -- `hardware` is the
  // board the CP reports, and reading `revision` here left that pill empty.
  document.getElementById('baseband').innerHTML =
    '<b>'+esc(d.model||'—')+'</b>'
    + '<em>'+esc(d.firmware||'—')+'</em>'
    + (d.hardware ? '<em>'+esc(d.hardware)+'</em>' : '')
    + (d.profile ? '<em>'+esc(d.profile)+'</em>' : ''); }catch(e){} }
async function loadIdentity(){
  const el = document.getElementById('identity'); el.textContent = '读取中…';
  try{ const d = await get('/api/identity'); let h = '';
    h += kv('ICCID', d.iccid); h += kv('IMSI', d.imsi); h += kv('手机号', d.phone);
    (d.imei||[]).forEach(function(e){
      h += kv('IMEI' + e.index + ' (' + (e.slot||'') + ')',
              e.value ? (e.value + (e.luhn===false ? '  ⚠ Luhn 校验不过' : '')) : (e.detail||'读取失败')); });
    h += kv('IPv4', d.ip);
    // The two v6 answers are separate facts: what the context was given, and
    // what the host ended up with.  One without the other is a real state.
    h += kv('IPv6 · 承载', d.ip6);
    h += kv('IPv6 · 接口', d.ip6_interface);
    h += kv('IPv6 DNS', d.ip6_dns);
    h += kv('APN', d.apn); h += kv('DNS', d.dns); h += kv('SMSC', d.smsc);
    (d.errors||[]).forEach(function(x){ h += '<div class="err">'+esc(x)+'</div>'; });
    el.innerHTML = h;
    const ie = document.getElementById('imei-read');
    if(ie) ie.innerHTML = (d.imei||[]).map(function(e){
      return kv('IMEI' + e.index + ' (' + (e.slot||'') + ')',
                e.value ? e.value : (e.detail||'读取失败')); }).join('')
      || '<div class="err">没有读到 IMEI（profile 未列 [nv].imei_items，或 diag 节点未起来）</div>';
  }catch(e){ el.textContent = '读取失败: '+e; } }
async function imeiWrite(){
  const v = document.getElementById('imei-value').value.trim();
  const c = document.getElementById('imei-confirm').value.trim();
  if(!v || v !== c){
    out('imei-out', {error:'两次输入不一致（或为空），没有发送任何东西'}); return; }
  if(!document.getElementById('imei-ack').checked){
    out('imei-out', {error:'请先勾选“我确认这是我拥有的设备”'}); return; }
  const idx = document.getElementById('imei-index').value;
  // The last act: a dialog that names the value and the slot, so the click
  // that writes is never the same click that filled the form in.
  if(!window.confirm('确认把 IMEI 索引 '+idx+' 写成 '+v+' ？\n这是永久改动，守护进程会先备份 NV 再写入并独立读回验证。')){
    return; }
  out('imei-out', await post('/api/imei-write', {
    imei:v, confirm:c, index:idx, acknowledged:'yes',
    allow_bad_checksum: document.getElementById('imei-bad').checked ? 'yes' : 'no' }));
  loadIdentity(); }
// The neighbour list belongs in the cell-lock card: it is where the lock
// decision comes from, and every row writes into the form below it.  As a
// table, because a list of seven numbers per row is only readable when the
// columns line up -- which is the whole reason it is not a paragraph.
function neighborTable(el, d){
  if(!el) return;
  const ns = d.neighbors || [];
  const lte = d.neighbors_lte, nr = d.neighbors_nr;
  const counts = 'LTE ' + (lte!=null ? lte+' 个' : '未上报')
    + ' · NR ' + (nr!=null ? nr+' 个' : '未上报');
  if(!ns.length && lte==null && nr==null){
    el.innerHTML = '<div class="note">CP 未上报邻区列表（SPENGMD 邻区查询无应答，或本代不支持）</div>';
    return;
  }
  // The lock button is the point of the table, so it is the first column: a
  // narrow screen scrolls the numbers sideways, and an action that scrolls out
  // of reach is an action nobody takes.
  const rows = ns.map(function(n){
    const attrs = ' data-rat="' + (n.rat==='NR' ? 'nr':'lte') + '" data-freq="' + n.earfcn
      + '" data-pci="' + n.pci + '"';
    const call = 'lockNeighbor(this.getAttribute(\'data-rat\'),'
      + 'this.getAttribute(\'data-freq\'),this.getAttribute(\'data-pci\'))';
    return '<tr><td class="act"><button class="btn tonal sm"' + attrs
      + ' onclick="' + call + '">锁</button></td>'
      + '<td>' + esc(n.rat) + '</td><td>' + esc(n.band||'—') + '</td>'
      + '<td class="num">' + n.earfcn + '</td><td class="num">' + n.pci + '</td>'
      + '<td class="num">' + n.rsrp + '</td><td class="num">' + n.rsrq + '</td>'
      + '<td class="num">' + (n.sinr==null ? '—' : n.sinr) + '</td></tr>';
  }).join('');
  const header = ns.length
    ? '<div class="scroll"><table class="tbl" aria-label="邻区列表，每行可锁定">'
      + '<thead><tr><th class="act"></th><th>RAT</th><th>band</th>'
      + '<th class="num">EARFCN</th><th class="num">PCI</th>'
      + '<th class="num">RSRP<span class="u">dBm</span></th>'
      + '<th class="num">RSRQ<span class="u">dB</span></th>'
      + '<th class="num">SINR<span class="u">dB</span></th>'
      + '</tr></thead><tbody>' + rows + '</tbody></table></div>'
    : '<div class="note">范围内没有读到邻区</div>';
  el.innerHTML = '<div class="note">' + counts + '</div>' + header;
}
async function loadMetrics(){
  const el = document.getElementById('metrics');
  const nb = document.getElementById('neighbors');
  el.textContent = '读取中…（要探测测量类 AT，可能几十秒）';
  if(nb) nb.innerHTML = '<div class="note">读取中…</div>';
  try{ const d = await get('/api/metrics'); let h = '';
    h += kv('RSSI', d.rssi_dbm!=null ? d.rssi_dbm+' dBm' : null);
    h += kv('RSRP', d.rsrp_dbm!=null
      ? d.rsrp_dbm+' dBm'+(d.rsrp_source==='serving' ? '（服务小区，CESQ 未上报）' : '')
      : null);
    h += kv('RSRQ', d.rsrq_db!=null ? d.rsrq_db+' dB' : null);
    h += kv('SINR', d.sinr_db!=null ? d.sinr_db+' dB' : null);
    function cell(tag, s){
      if(!s || s.earfcn==null) return kv(tag + ' 服务小区', null);
      return kv(tag + ' 服务小区', 'band ' + (s.band||'—') + ' · EARFCN ' + s.earfcn
        + ' · PCI ' + (s.pci!=null?s.pci:'—') + ' · 频宽 ' + (s.bandwidth||'—')
        + ' · 小区ID ' + (s.cell||'—')); }
    h += cell('LTE', d.lte); h += cell('NR', d.nr);
    if(!d.serving_supported) h += '<div class="err">CP 未上报服务小区测量（本代可能不支持 SPENGMD 测量树）</div>';
    el.innerHTML = h;
    neighborTable(nb, d);
  }catch(e){ el.textContent = '读取失败: '+e;
    if(nb) nb.innerHTML = '<div class="note">读取失败: '+esc(e)+'</div>'; } }
// The lock card shows the state and the neighbours, and both come from asking
// the CP, so one button refreshes the pair rather than one each.
function refreshCells(){ loadLocks(); loadMetrics(); }
async function loadNetwork(){
  const el = document.getElementById('network'); el.textContent = '读取中…';
  try{ const d = await get('/api/network'); let h = '';
    h += kv('运营商', ((d.operator_name||'未知') + (d.operator_numeric ? ' · ' + d.operator_numeric : '')));
    h += kv('网络', d.mode + (d.sa ? '（SA 已注册）' : ''));
    h += kv('CEREG', d.cereg); h += kv('C5GREG', d.c5greg);
    h += kv('CREG', d.creg); h += kv('GATT', d.gatt);
    if(d.error) h += '<div class="err">'+esc(d.error)+'</div>';
    el.innerHTML = h;
  }catch(e){ el.textContent = '读取失败: '+e; } }
var COMMON_LTE = [1,3,5,8,34,38,39,40,41];
var COMMON_NR = [1,28,41,77,78,79];
function quickBands(){
  function render(id, rat, bands){
    document.getElementById(id).innerHTML = '<span class="note">常用</span>' + bands.map(function(b){
      return '<span class="hist" onclick="addBand(this)" data-rat="'+rat+'" data-band="'+b+'">n'+b+'</span>';
    }).join('');
  }
  render('band-quick-lte','lte',COMMON_LTE); render('band-quick-nr','nr',COMMON_NR);
}
function addBand(el){
  var rat = el.getAttribute('data-rat'), band = el.getAttribute('data-band');
  var input = document.getElementById(rat+'-bands');
  var cur = input.value.split(/[\s,]+/).filter(Boolean);
  if(cur.indexOf(band)<0) cur.push(band);
  input.value = cur.join(',');
}
function locksChips(d){
  const lb = (d.lte_bands||[]), nb = (d.nr_bands||[]);
  const lc = (d.lte_cells||[]), nc = (d.nr_cells||[]);
  // A lock is a state that was asked for, not a fault: it gets the attention
  // tone, and the error tone stays for things that are actually wrong.
  document.getElementById('locks').innerHTML =
    chip(0, lb.length ? 'LTE 锁 band '+lb.join(',') : 'LTE 未锁频段', lb.length?'warn':'dim') +
    chip(0, nb.length ? 'NR 锁 band '+nb.join(',') : 'NR 未锁频段', nb.length?'warn':'dim') +
    chip(0, (lc.length+nc.length) ? '已锁基站 '+(lc.length+nc.length)+' 个' : '未锁基站',
         (lc.length+nc.length)?'warn':'dim');
}
function cellsText(rat, cells){
  return cells.length
    ? cells.map(function(c){ return rat.toUpperCase()+' '+c.freq+'/'+c.pci; }).join('  ')
    : rat.toUpperCase()+' 未锁基站';
}
async function loadLocks(){
  try{ const d = await get('/api/bands'); locksChips(d);
    document.getElementById('bands-state').innerHTML =
      kv('LTE 锁频段', (d.lte_bands||[]).length ? d.lte_bands.join(', ') : null) +
      kv('NR 锁频段', (d.nr_bands||[]).length ? d.nr_bands.join(', ') : null) +
      kv('SPRAT', d.sprat);
    document.getElementById('cells-state').innerHTML =
      kv('LTE 锁基站', cellsText('lte', d.lte_cells||[]).replace('LTE ','')) +
      kv('NR 锁基站', cellsText('nr', d.nr_cells||[]).replace('NR ',''));
    if(d.error) document.getElementById('bands-state').innerHTML += '<div class="err">'+esc(d.error)+'</div>';
  }catch(e){} }
async function bandLock(rat){
  var v = document.getElementById(rat+'-bands').value.trim();
  if(!v) return;
  out('bands-out', await post('/api/band-lock', {rat:rat, bands:v})); loadLocks(); }
async function bandUnlock(rat){
  out('bands-out', await post('/api/band-unlock', {rat:rat})); loadLocks(); }
async function cellLock(){
  out('cells-out', await post('/api/cell-lock', {
    rat: document.getElementById('cell-rat').value,
    freq: document.getElementById('cell-freq').value.trim(),
    pci: document.getElementById('cell-pci').value.trim() })); loadLocks(); }
async function cellUnlock(){
  out('cells-out', await post('/api/cell-unlock', {rat: document.getElementById('cell-rat').value}));
  loadLocks(); }
function lockNeighbor(rat, freq, pci){
  document.getElementById('d-cells').open = true;
  document.getElementById('cell-rat').value = rat;
  document.getElementById('cell-freq').value = freq;
  document.getElementById('cell-pci').value = pci;
}
async function loadApn(){
  const el = document.getElementById('apn-state'); el.textContent = '读取中…';
  try{ const d = await get('/api/apn'); let h = '';
    h += kv('承载会用的 APN', d.apn ? (d.apn + '（来源：' + (d.apn_source||'?') + '）') : null);
    h += kv('覆盖文件', d.saved_apn ? d.saved_apn : '未固定（文件里的 APN= 仍是注释）');
    h += kv('覆盖文件路径', d.apn_source_path);
    h += kv('默认 CID', d.cid);
    (d.contexts||[]).forEach(function(c){
      h += '<div><b>CID '+c.cid+'</b><span>'+(c.apn ? esc(c.apn) : '（无 APN）')
        + (c.pdp_type ? ' · '+esc(c.pdp_type) : '') + (c.state ? ' · '+esc(c.state) : '') + '</span></div>'; });
    el.innerHTML = h;
    // The contexts the CP actually has, so the CID is picked rather than
    // typed: the device carries more than one (data and IMS), and a typo here
    // would write an APN into a context nobody uses.
    const sel = document.getElementById('apn-cid');
    if(sel){
      const cids = (d.contexts||[]).map(function(c){ return c.cid; });
      if(d.cid!=null && cids.indexOf(d.cid)<0) cids.unshift(d.cid);
      sel.innerHTML = cids.map(function(c){ return '<option value="'+c+'">'+c+'</option>'; }).join('');
      if(d.cid!=null) sel.value = String(d.cid);
    }
    const v = document.getElementById('apn-value');
    if(!v.value && d.apn) v.value = d.apn;
  }catch(e){ el.textContent = '读取失败: '+e; } }
async function apnSet(){
  const apn = document.getElementById('apn-value').value.trim(); if(!apn) return;
  out('apn-out', await post('/api/apn-set', {apn:apn, cid:document.getElementById('apn-cid').value}));
  loadApn(); }
async function apnSave(){
  const apn = document.getElementById('apn-value').value.trim(); if(!apn) return;
  out('apn-out', await post('/api/apn-save', {apn:apn})); loadApn(); }
async function apnClear(){
  if(!window.confirm('清除 Modem 的 PDP 上下文定义？清空后承载没有 APN 可用，直到重新设置。')) return;
  out('apn-out', await post('/api/apn-clear', {cid:document.getElementById('apn-cid').value}));
  loadApn(); }
function lazyLoad(){
  quickBands();
  document.getElementById('d-identity').addEventListener('toggle', function(){ if(this.open) loadIdentity(); });
  document.getElementById('d-metrics').addEventListener('toggle', function(){ if(this.open) loadMetrics(); });
  document.getElementById('d-network').addEventListener('toggle', function(){ if(this.open) loadNetwork(); });
  document.getElementById('d-apn').addEventListener('toggle', function(){ if(this.open) loadApn(); });
  document.getElementById('d-bands').addEventListener('toggle', function(){ if(this.open) loadLocks(); });
  document.getElementById('d-cells').addEventListener('toggle', function(){ if(this.open) refreshCells(); });
  document.getElementById('d-imei').addEventListener('toggle', function(){ if(this.open) loadIdentity(); });
}

var atHist = [];
try { atHist = JSON.parse(localStorage.getItem('atHist') || '[]') || []; } catch(e) { atHist = []; }
function renderAtHist(){
  document.getElementById('at-hist').innerHTML = atHist.length
    ? atHist.map(function(c){
        var esc = c.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/"/g,'&quot;');
        return '<span class="hist" onclick="useAt(this)" data-cmd="'+esc+'">'+esc+'</span>';
      }).join('')
    : '<span class="note">（无历史）</span>';
}
function useAt(el){ document.getElementById('at-cmd').value = el.getAttribute('data-cmd'); }
function clearAtHist(){ atHist = []; localStorage.setItem('atHist','[]'); renderAtHist(); }
async function sendAt(){
  var cmd = document.getElementById('at-cmd').value.trim();
  if(!cmd) return;
  atHist = [cmd].concat(atHist.filter(function(c){ return c!==cmd; })).slice(0,12);
  localStorage.setItem('atHist', JSON.stringify(atHist)); renderAtHist();
  var r = await post('/api/at', {cmd: cmd});
  out('at-out', r);
}
function atEnter(e){ if(e.key==='Enter') sendAt(); }
document.getElementById('at-cmd').addEventListener('keydown', atEnter);
renderAtHist();
setInterval(refreshState, 5000); setInterval(refreshUrc, 2000); setInterval(refreshMsgs, 8000);
setInterval(refreshStatus, 20000); setInterval(refreshInfo, 60000);
// The lock chips are a read-back of the CP's own state, so they are polled
// like the rest of the status row -- slower, because it costs five AT
// commands to ask.
setInterval(loadLocks, 30000);
refreshState(); refreshUrc(); refreshMsgs(); refreshStatus(); refreshInfo(); lazyLoad(); loadLocks();
</script></body></html>

"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_reads_utf8_and_plus() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn form_value_picks_named_fields() {
        assert_eq!(form_value("to=123&text=hello", "text"), Some("hello".into()));
        assert_eq!(form_value("to=123", "text"), None);
    }

    fn lines(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The summary convention: column-zero `key: value` only.  The raw echo is
    /// indented and prefixed `> CMD`, so it can never be mistaken for data —
    /// and `-` is absence, not a value.
    #[test]
    fn summaries_read_only_the_column_zero_lines() {
        let out = lines(&[
            "> AT+CIMI",
            "  460011234567890",
            "  OK",
            "imsi: 460011234567890",
            "iccid: 8986012345678901234",
            "phone: -",
            "  -> imsi: this is inside a block, not a summary",
        ]);
        assert_eq!(summary(&out, "imsi").as_deref(), Some("460011234567890"));
        assert_eq!(
            summary(&out, "iccid").as_deref(),
            Some("8986012345678901234")
        );
        assert_eq!(summary(&out, "phone"), None);
        assert_eq!(summary(&out, "nope"), None);
    }

    /// The two shapes `imei read` produces: a value, and the read's own error.
    /// The second must survive into the panel — "the diag node is missing" is
    /// a fact, not an empty field.
    #[test]
    fn imei_entries_keep_the_value_and_the_failure() {
        let out = lines(&[
            "imei0 (SIM 1, item 5e81) = 490154203237518",
            "imei1 (SIM 2, item 5e82): no 15-digit identity record after the marker",
            "some other line",
        ]);
        let entries = imei_entries(&out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["index"], 0);
        assert_eq!(entries[0]["slot"], "SIM 1");
        assert_eq!(entries[0]["value"], "490154203237518");
        assert_eq!(entries[0]["luhn"], true);
        assert_eq!(entries[1]["index"], 1);
        assert!(entries[1]["value"].is_null());
        assert!(entries[1]["detail"]
            .as_str()
            .unwrap()
            .contains("no 15-digit identity"));
    }

    /// A check-digit failure is reported, not silently rounded away.
    #[test]
    fn a_luhn_invalid_imei_is_flagged() {
        let out = lines(&["imei0 (SIM 1, item 5e81) = 490154203237519"]);
        assert_eq!(imei_entries(&out)[0]["luhn"], false);
    }

    /// 37.003's mapping, and 99 kept as "not known".
    #[test]
    fn csq_becomes_dbm_and_99_stays_unknown() {
        assert_eq!(csq_rssi(&lines(&["+CSQ: 23,99"])), Some(-67));
        assert_eq!(csq_rssi(&lines(&["+CSQ: 0,99"])), Some(-113));
        assert_eq!(csq_rssi(&lines(&["+CSQ: 99,99"])), None);
        assert_eq!(csq_rssi(&lines(&["ERROR"])), None);
    }

    /// `decode_cesq` works to keep "not reported" apart from a reading; the
    /// panel must not undo that by parsing it back into a number.
    #[test]
    fn decoded_fields_keep_not_reported_apart() {
        let line = Some("decoded: RSRP -80 dBm, RSRQ -9.5 dB, SINR not reported".to_string());
        assert_eq!(decoded_field(line.as_ref(), "RSRP").as_deref(), Some("-80"));
        assert_eq!(decoded_field(line.as_ref(), "RSRQ").as_deref(), Some("-9.5"));
        assert_eq!(decoded_field(line.as_ref(), "SINR"), None);
        assert_eq!(decoded_field(None, "RSRP"), None);
    }

    #[test]
    fn a_neighbor_line_becomes_numbers_and_text() {
        let entry = neighbor_entry("LTE,band=3,earfcn=1650,pci=88,rsrp=-95.0,rsrq=-12.0");
        assert_eq!(entry["rat"], "LTE");
        assert_eq!(entry["band"], "3", "a band is a label, not arithmetic");
        assert_eq!(entry["earfcn"], 1650.0);
        assert_eq!(entry["pci"], 88.0);
        assert_eq!(entry["rsrp"], -95.0);
        let nr = neighbor_entry("NR,band=78,earfcn=627264,pci=5,rsrp=-95.0,rsrq=-12.0,sinr=1.0");
        assert_eq!(nr["sinr"], 1.0);
    }

    /// The neighbour key repeats, so the panel needs all of them -- and one
    /// "not reported" line must not be counted as a neighbour.
    #[test]
    fn a_repeated_summary_key_yields_every_line() {
        let out = lines(&[
            "neighbor: LTE,band=3,earfcn=1650,pci=88,rsrp=-95.0,rsrq=-12.0",
            "neighbor: NR,band=78,earfcn=627264,pci=5,rsrp=-95.0,rsrq=-12.0",
            "neighbors_lte: 1",
            "neighbors_nr: not reported",
        ]);
        assert_eq!(summary_all(&out, "neighbor").len(), 2);
        assert_eq!(summary(&out, "neighbors_lte").as_deref(), Some("1"));
        assert_eq!(summary(&out, "neighbors_nr"), None);
    }

    /// A band list is validated, not filtered: dropping a bad token and
    /// locking the rest would be a lock nobody asked for.
    #[test]
    fn band_tokens_validate_instead_of_dropping() {
        assert_eq!(band_tokens("78, 41").unwrap(), vec![41, 78]);
        assert_eq!(band_tokens("3,3,3").unwrap(), vec![3]);
        assert!(band_tokens("").is_err());
        assert!(band_tokens("n78").is_err());
        assert!(band_tokens("78,n41").is_err());
    }

    #[test]
    fn a_lock_request_needs_a_known_rat_and_numbers() {
        assert_eq!(
            band_request("rat=nr&bands=78").unwrap(),
            ("nr".to_string(), vec![78])
        );
        assert!(band_request("bands=78").is_err(), "no RAT");
        assert!(band_request("rat=gsm&bands=78").is_err(), "not this generation");
        let (rat, freq, pci) = cell_request("rat=lte&freq=1650&pci=88").unwrap();
        assert_eq!((rat.as_str(), freq, pci), ("lte", 1650, 88));
        assert!(cell_request("rat=lte&freq=abc&pci=88").is_err());
        assert!(cell_request("rat=lte&pci=88").is_err());
    }

    #[test]
    fn locked_bands_and_cells_come_out_of_the_summary_lines() {
        let out = lines(&[
            "lte_bands: 1,3,41",
            "nr_bands: -",
            "lte_cells: 1650/88 3000/7",
            "nr_cells: -",
            "sprat: LTE 32",
        ]);
        let state = band_state_of(&out);
        assert_eq!(state["lte_bands"], json!([1, 3, 41]));
        assert_eq!(state["nr_bands"], json!([]));
        assert_eq!(state["lte_cells"][0]["freq"], 1650);
        assert_eq!(state["lte_cells"][1]["pci"], 7);
        assert_eq!(state["nr_cells"], json!([]));
        assert_eq!(state["sprat"], "LTE 32");
    }

    /// The page's half of the identity guard: the confirmation is the value
    /// itself, and the acknowledgement is explicit.  A request that fails
    /// either never reaches the modem.
    #[test]
    fn an_identity_write_takes_two_entries_and_an_acknowledgement() {
        // A Luhn-valid placeholder, of the kind the rig and the docs use.
        let imei = "490154203237518";
        let args = imei_write_args(&format!(
            "imei={imei}&confirm={imei}&acknowledged=yes&index=1"
        ))
        .unwrap();
        assert_eq!(
            args,
            vec!["write", "490154203237518", "--index", "1", "--yes"]
        );
        // the confirmation must be the value, not a boolean
        assert!(imei_write_args(&format!("imei={imei}&confirm=yes&acknowledged=yes")).is_err());
        // a mistyped digit fails in front of the person who typed it
        assert!(imei_write_args(&format!(
            "imei={imei}&confirm=490154203237519&acknowledged=yes"
        ))
        .is_err());
        assert!(
            imei_write_args(&format!("imei={imei}&confirm={imei}")).is_err(),
            "no acknowledgement"
        );
        assert!(imei_write_args(&format!(
            "imei={imei}&confirm={imei}&acknowledged=yes&index=7"
        ))
        .is_err());
        assert!(imei_write_args("").is_err());
        let overridden = imei_write_args(&format!(
            "imei={imei}&confirm={imei}&acknowledged=yes&allow_bad_checksum=yes"
        ))
        .unwrap();
        assert!(overridden.contains(&"--allow-bad-checksum".to_string()));
    }

    /// Two places can name the port -- this constant and the systemd unit that
    /// actually ships -- and a deployment where they disagree serves a port
    /// nobody documented.  So the test reads the unit.
    #[test]
    fn the_unit_and_the_code_agree_on_the_default_port() {
        let port = DEFAULT_LISTEN
            .rsplit(':')
            .next()
            .expect("a listen address has a port");
        assert_eq!(port, "7887");
        let unit = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/units/unisoc-cpd-web.service"
        ))
        .expect("the web unit is part of the tree");
        assert!(
            unit.contains(&format!(":{port}")),
            "the web unit does not listen on :{port}:\n{unit}"
        );
    }

    #[test]
    fn an_apn_request_needs_a_value_and_a_context() {
        assert_eq!(apn_request("apn=cbnet").unwrap(), ("cbnet".to_string(), 1));
        assert_eq!(
            apn_request("apn=cbnet&cid=3").unwrap(),
            ("cbnet".to_string(), 3)
        );
        assert!(apn_request("").is_err(), "no APN");
        assert!(apn_request("apn=%20").is_err(), "a blank APN is no APN");
        assert!(apn_request("apn=cbnet&cid=0").is_err(), "cid 0 is not a context");
        assert!(apn_request("apn=cbnet&cid=x").is_err());
    }

    #[test]
    fn registration_fields_are_read_by_position() {
        let cereg = Some("2,1,\"DE0400\",\"005BE001\",7".to_string());
        assert_eq!(reg_state(cereg.as_ref()), Some(1));
        assert_eq!(reg_field(cereg.as_ref(), 4).as_deref(), Some("7"));
        assert_eq!(
            reg_field(cereg.as_ref(), 2).as_deref(),
            Some("DE0400"),
            "a quoted field loses its quotes"
        );
        assert_eq!(reg_state(Some(&"-".to_string())), None);
    }
}
