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
            .unwrap_or_else(|| "127.0.0.1:8080".into());
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

    json!({
        "rssi_dbm": csq_rssi(&status_out),
        "rsrp_dbm": number("RSRP"),
        "rsrq_db": number("RSRQ"),
        "sinr_db": number("SINR"),
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
        "revision": summary(&out, "revision"),
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
    let name = numeric
        .as_deref()
        .and_then(crate::capability::control::operator_name)
        .map(|s| s.to_string());

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
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>unisoc-cpd</title>
<style>
 body{font-family:system-ui,sans-serif;background:#111;color:#ddd;margin:0;padding:12px}
 h1{font-size:18px} h2{font-size:15px;margin:14px 0 6px}
 .chips span{display:inline-block;background:#1d2b1d;border:1px solid #2e4d2e;border-radius:10px;padding:2px 10px;margin:2px;font-size:12px}
 .chips span.ok{background:#163416;border-color:#3f7f3f;color:#b8e6b8}
 .chips span.bad{background:#3d1515;border-color:#8f3f3f;color:#f0b8b8}
 .chips span.dim{opacity:.55}
 pre{background:#181818;border:1px solid #333;border-radius:6px;padding:8px;min-height:14px;max-height:220px;overflow:auto;font-size:12px;white-space:pre-wrap}
 input,button{font-size:14px;border-radius:6px;border:1px solid #444;background:#222;color:#eee;padding:6px 10px;margin:2px}
 button{cursor:pointer;background:#28422a} button.red{background:#5a2323}
 #banner{display:none;position:fixed;inset:0;background:rgba(120,20,20,.94);z-index:9;text-align:center;padding-top:30vh}
 #banner button{font-size:22px;margin:12px}
 .msg{border-bottom:1px solid #2a2a2a;padding:6px 2px;font-size:13px}
 .from{color:#8bc78b}
 .warn{background:#3a2a12;border:1px solid #8a6420;color:#f0d9a8;border-radius:6px;padding:8px;font-size:12px;margin:6px 0}
 details{border:1px solid #333;border-radius:6px;margin:8px 0;padding:6px 8px;background:#161616}
 summary{cursor:pointer;font-weight:600;font-size:14px}
 .kv{font-size:13px;line-height:1.7} .kv b{color:#9cc79c;font-weight:600;display:inline-block;min-width:74px}
 .kv .dim{opacity:.55}
 .err{color:#f0b8b8;font-size:12px}
 .hist{font-size:12px;color:#888;cursor:pointer}
 .hist:hover{color:#ddd}
</style></head><body>
<h1>unisoc-cpd</h1>
<div class="kv" id="baseband">…</div>
<div class="chips" id="radio">…</div>
<div class="chips" id="locks">…</div>
<div class="chips" id="chips"></div>
<div id="banner"><div id="bnr-txt" style="font-size:24px">+CRING: VOICE</div>
 <button onclick="act('answer')">接听</button><button class="red" onclick="act('hangup')">挂断</button></div>

<details id="d-identity"><summary>高级信息（ICCID / IMEI / IMSI / IP / 手机号 / SMSC）</summary>
<button onclick="loadIdentity()">刷新</button>
<div class="kv" id="identity">展开后读取…</div></details>

<details id="d-metrics"><summary>信号详情（RSSI / RSRP / RSRQ / 频率 / 频宽 / PCI / 小区ID · 邻区）</summary>
<button onclick="loadMetrics()">刷新</button>
<div class="kv" id="metrics">展开后读取…（要探测测量类 AT，可能较慢）</div></details>

<details id="d-network"><summary>网络（运营商 · 5G SA/NSA · 注册状态）</summary>
<button onclick="loadNetwork()">刷新</button>
<div class="kv" id="network">展开后读取…</div></details>

<details id="d-bands"><summary>锁频段（LTE / NR 带号 · AT+SPLBAND）</summary>
<button onclick="loadLocks()">刷新</button>
<div class="warn">⚠️ 锁到当前网络用不到的频段会直接失去服务（一直无信号直到解锁）。下面显示的是 CP 读回的
当前锁定，不是你刚按下的按钮——写入后守护进程会立刻读回比对。</div>
<div class="kv" id="bands-state">展开后读取…</div>
<div>LTE <input id="lte-bands" placeholder="1,3,41" size="20" autocomplete="off">
<button onclick="bandLock('lte')">锁定</button><button class="red" onclick="bandUnlock('lte')">解锁</button></div>
<div id="band-quick-lte" class="hist"></div>
<div>NR <input id="nr-bands" placeholder="41,78" size="20" autocomplete="off">
<button onclick="bandLock('nr')">锁定</button><button class="red" onclick="bandUnlock('nr')">解锁</button></div>
<div id="band-quick-nr" class="hist"></div>
<div><button class="red" onclick="bandUnlock('')">LTE+NR 全部解锁</button></div>
<pre id="bands-out">…</pre></details>

<details id="d-cells"><summary>锁基站（EARFCN + PCI · AT+SPFORCEFRQ）</summary>
<button onclick="loadLocks()">刷新</button>
<div class="warn">⚠️ 锁基站比锁频段更紧：锁到一个不可用的小区会一直无服务。邻区列表里的“锁”会把参数填进来，
但仍要你按一下才会写。</div>
<div class="kv" id="cells-state">展开后读取…</div>
<div>RAT <select id="cell-rat"><option value="lte">LTE</option><option value="nr">NR</option></select>
EARFCN <input id="cell-freq" size="9" autocomplete="off"> PCI <input id="cell-pci" size="5" autocomplete="off">
<button onclick="cellLock()">锁定</button><button class="red" onclick="cellUnlock()">解锁</button></div>
<pre id="cells-out">…</pre></details>

<h2>短信 · inbox</h2><div id="msgs">…</div>
<h2>发短信</h2>
<div><input id="to" placeholder="+86…" size="14"> <input id="text" placeholder="内容" size="24">
<button onclick="sendSms()">发送</button></div><pre id="sms-out"></pre>

<h2>电话</h2>
<div><input id="num" placeholder="号码" size="14">
<button onclick="dial()">呼叫</button>
<button onclick="act('answer')">接听</button>
<button class="red" onclick="act('hangup')">挂断</button></div><pre id="call-out"></pre>

<h2>自定义 AT 控制台</h2>
<div class="warn">⚠️ 这是直通 CP 的原始 AT 通道。查询类（<code>AT+CSQ</code>、<code>AT+CEREG?</code>）是安全的；
但写类命令（<code>AT+SPLBAND=1,…</code>、<code>AT+SPFORCEFRQ=…</code>、<code>AT+SPIMEI=…</code>、任何厂商
NV 命令）会立刻并可能永久改变基带配置 / NV，写错可能失联直到恢复出厂。
命令由持有唯一 AT 通道的守护进程执行，本页只做转发。</div>
<div><input id="at-cmd" placeholder="AT+CSQ" size="34" autocomplete="off">
<button onclick="sendAt()">发送</button>
<button class="red" onclick="clearAtHist()">清空历史</button></div>
<pre id="at-out">…</pre>
<div id="at-hist"></div>

<h2>事件流（urc）</h2><pre id="urc-out">…</pre>
<h2>状态详情</h2><pre id="stat-out">…</pre>

<script>
function out(id, resp){ document.getElementById(id).textContent =
  (resp.output||[]).join('\n') + (resp.status ? ('\n['+resp.status+']') : '')
  + (resp.error ? ('\n[error] '+resp.error) : ''); }
async function get(u){ const r = await fetch(u); return r.json(); }
async function post(u, data){ const r = await fetch(u, {method:'POST',
  headers:{'Content-Type':'application/x-www-form-urlencoded'},
  body: new URLSearchParams(data).toString()}); return r.json(); }
function chip(list, label, cls){
  return '<span class="'+(cls||'')+'">'+label+'</span>'; }
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
async function refreshUrc(){ try{ const d = await get('/api/urc'); const us = d.urcs||[];
  document.getElementById('urc-out').textContent = us.map(fmtUrc).join('\n');
  const ring = us.filter(isRing);
  const b = document.getElementById('banner');
  if(ring.length){ if(b.style.display!=='block'){ b.style.display='block';
    const o = ring[ring.length-1].urc;
    document.getElementById('bnr-txt').textContent = (o&&o.kind) ? '来电 '+(o.number||'') : String(o); } }
  else { b.style.display='none'; }
 }catch(e){} }
async function refreshMsgs(){ try{ const d = await get('/api/messages'); const ms = d.messages||[];
  document.getElementById('msgs').innerHTML = ms.map(function(m){
   return '<div class="msg"><span class="from">'+m.from+'</span> '
    +(m.timestamp||'')+'<br>'+ (m.text||'').replace(/&/g,'&amp;').replace(/</g,'&lt;') +'</div>';}).join('')
   || '（空）';
 }catch(e){} }
async function sendSms(){ const r = await post('/api/send',
  {to:document.getElementById('to').value, text:document.getElementById('text').value});
 out('sms-out', r); refreshMsgs(); }
async function dial(){ const r = await post('/api/dial', {number:document.getElementById('num').value});
 out('call-out', r); }
async function act(w){ const r = await post('/api/'+w, {}); out('call-out', r);
 if(w!=='hangup') setTimeout(function(){document.getElementById('banner').style.display='none';}, 800); }

function esc(s){ return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;'); }
function kv(label, value){
  return '<div><b>'+label+'</b>'+(value==null||value===''
    ? '<span class="dim">未上报</span>' : esc(value))+'</div>'; }
async function refreshInfo(){ try{ const d = await get('/api/info');
  document.getElementById('baseband').innerHTML =
    kv('基带', (d.model||'—') + ' · ' + (d.firmware||'—') + ' · ' + (d.profile||'—')); }catch(e){} }
async function loadIdentity(){
  const el = document.getElementById('identity'); el.textContent = '读取中…';
  try{ const d = await get('/api/identity'); let h = '';
    h += kv('ICCID', d.iccid); h += kv('IMSI', d.imsi); h += kv('手机号', d.phone);
    (d.imei||[]).forEach(function(e){
      h += kv('IMEI' + e.index + ' (' + (e.slot||'') + ')',
              e.value ? (e.value + (e.luhn===false ? '  ⚠ Luhn 校验不过' : '')) : (e.detail||'读取失败')); });
    h += kv('IP', d.ip); h += kv('APN', d.apn); h += kv('DNS', d.dns); h += kv('SMSC', d.smsc);
    (d.errors||[]).forEach(function(x){ h += '<div class="err">'+esc(x)+'</div>'; });
    el.innerHTML = h;
  }catch(e){ el.textContent = '读取失败: '+e; } }
async function loadMetrics(){
  const el = document.getElementById('metrics');
  el.textContent = '读取中…（要探测测量类 AT，可能几十秒）';
  try{ const d = await get('/api/metrics'); let h = '';
    h += kv('RSSI', d.rssi_dbm!=null ? d.rssi_dbm+' dBm' : null);
    h += kv('RSRP', d.rsrp_dbm!=null ? d.rsrp_dbm+' dBm' : null);
    h += kv('RSRQ', d.rsrq_db!=null ? d.rsrq_db+' dB' : null);
    h += kv('SINR', d.sinr_db!=null ? d.sinr_db+' dB' : null);
    function cell(tag, s){
      if(!s || s.earfcn==null) return kv(tag + ' 服务小区', null);
      return kv(tag + ' 服务小区', 'band ' + (s.band||'—') + ' · EARFCN ' + s.earfcn
        + ' · PCI ' + (s.pci!=null?s.pci:'—') + ' · 频宽 ' + (s.bandwidth||'—')
        + ' · 小区ID ' + (s.cell||'—')); }
    h += cell('LTE', d.lte); h += cell('NR', d.nr);
    h += kv('邻区 LTE', d.neighbors_lte!=null ? d.neighbors_lte+' 个' : null);
    h += kv('邻区 NR', d.neighbors_nr!=null ? d.neighbors_nr+' 个' : null);
    (d.neighbors||[]).forEach(function(n){
      h += '<div>' + esc(n.rat + '  band ' + (n.band||'—') + '  EARFCN ' + n.earfcn
        + '  PCI ' + n.pci + '  RSRP ' + n.rsrp + ' dBm  RSRQ ' + n.rsrq + ' dB'
        + (n.sinr!=null ? '  SINR ' + n.sinr + ' dB' : ''))
        + ' <button onclick="lockNeighbor(this.getAttribute(\'data-rat\'), this.getAttribute(\'data-freq\'), this.getAttribute(\'data-pci\'))"'
        + ' data-rat="' + (n.rat==='NR'?'nr':'lte') + '" data-freq="' + n.earfcn + '" data-pci="' + n.pci + '"'
        + '>锁</button></div>'; });
    if(!d.serving_supported) h += '<div class="err">CP 未上报服务小区测量（本代可能不支持 SPENGMD 测量树）</div>';
    el.innerHTML = h;
  }catch(e){ el.textContent = '读取失败: '+e; } }
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
    document.getElementById(id).innerHTML = '常用：' + bands.map(function(b){
      return '<span class="hist" onclick="addBand(this)" data-rat="'+rat+'" data-band="'+b+'">n'+b+'</span>';
    }).join(' ');
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
  document.getElementById('locks').innerHTML =
    chip(0, lb.length ? 'LTE 锁 band '+lb.join(',') : 'LTE 未锁频段', lb.length?'bad':'dim') +
    chip(0, nb.length ? 'NR 锁 band '+nb.join(',') : 'NR 未锁频段', nb.length?'bad':'dim') +
    chip(0, (lc.length+nc.length) ? '已锁基站 '+(lc.length+nc.length)+' 个' : '未锁基站',
         (lc.length+nc.length)?'bad':'dim');
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
function lazyLoad(){
  quickBands();
  document.getElementById('d-identity').addEventListener('toggle', function(){ if(this.open) loadIdentity(); });
  document.getElementById('d-metrics').addEventListener('toggle', function(){ if(this.open) loadMetrics(); });
  document.getElementById('d-network').addEventListener('toggle', function(){ if(this.open) loadNetwork(); });
  document.getElementById('d-bands').addEventListener('toggle', function(){ if(this.open) loadLocks(); });
  document.getElementById('d-cells').addEventListener('toggle', function(){ if(this.open) loadLocks(); });
}

var atHist = [];
try { atHist = JSON.parse(localStorage.getItem('atHist') || '[]') || []; } catch(e) { atHist = []; }
function renderAtHist(){
  document.getElementById('at-hist').innerHTML = atHist.length
    ? atHist.map(function(c){
        var esc = c.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/"/g,'&quot;');
        return '<span class="hist" onclick="useAt(this)" data-cmd="'+esc+'">'+esc+'</span>';
      }).join(' · ')
    : '<span class="hist">（无历史）</span>';
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
