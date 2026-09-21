//! Control-plane capabilities: SIM/PIN, CFUN, registration, signal, operator,
//! band, SMS, USSD, voice call.
//!
//! The AT sequences are the ones written down in `docs/BASEBAND-CONTRACTS.md`
//! (captured from the vendor side), so a `native` run and an Android oracle run
//! can be diffed command by command.

use super::{emit, flag_value, positionals, Capability, Outcome};
use crate::context::Context;
use anyhow::Result;
use serde::Serialize;
use std::time::Duration;

fn outcome(lines: Vec<String>, ok: bool) -> Outcome {
    if ok {
        Outcome::pass(lines)
    } else {
        Outcome::fail(lines)
    }
}

fn each(out: &mut Vec<String>, pairs: &[(&str, crate::at::Reply)]) -> bool {
    let mut ok = true;
    for (cmd, reply) in pairs {
        emit(out, cmd, reply);
        ok &= reply.ok();
    }
    ok
}

// ---------------------------------------------------------------- SIM / PIN

pub struct Sim;

impl Capability for Sim {
    fn name(&self) -> &'static str {
        "sim"
    }

    fn summary(&self) -> &'static str {
        "SIM presence and PIN state (AT+CPIN?), optional PIN entry"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let t = Duration::from_secs(5);
        let mut out = Vec::new();

        if pos.first().map(|s| s.as_str()) == Some("identity") {
            // Read-only SIM identity, deliberately behind its own action.  The
            // rig needs the ICCID to tell two SIMs apart; device identity
            // (IMEI) lives in its own guarded `imei` capability, so nothing
            // identity-related hides in here.  `+CNUM` is the SIM's own
            // MSISDN, which many cards leave unprovisioned: it is read and
            // reported, but its absence is not a failure of the read.
            let imsi = session.command("AT+CIMI", t, &[], 0);
            let iccid = session.command("AT+CCID", t, &[], 0);
            let cnum = session.command("AT+CNUM", t, &[], 0);
            emit(&mut out, "AT+CIMI", &imsi);
            emit(&mut out, "AT+CCID", &iccid);
            emit(&mut out, "AT+CNUM", &cnum);
            out.push(format!("imsi: {}", imsi_value(&imsi)));
            out.push(format!("iccid: {}", iccid_value(&iccid)));
            let phone = cnum_value(&cnum);
            out.push(format!("phone: {}", phone.as_deref().unwrap_or("-")));
            if phone.is_none() {
                ctx.note(
                    "AT+CNUM carried no MSISDN: many SIMs are not provisioned with one"
                        .to_string(),
                );
            }
            return Ok(outcome(out, imsi.ok() && iccid.ok()));
        }
        if let Some(pin) = pos.first() {
            if pin != "info" {
                let cmd = format!("AT+CPIN=\"{pin}\"");
                let r = session.command(&cmd, Duration::from_secs(20), &[], 0);
                emit(&mut out, &cmd, &r);
                let after = session.command("AT+CPIN?", t, &[], 0);
                emit(&mut out, "AT+CPIN?", &after);
                let ok = r.ok()
                    && after
                        .first_with_prefix("+CPIN:")
                        .map(|l| l.contains("READY"))
                        .unwrap_or(false);
                return Ok(outcome(out, ok));
            }
        }

        let r = session.command("AT+CPIN?", t, &[], 0);
        emit(&mut out, "AT+CPIN?", &r);
        let state = r.first_with_prefix("+CPIN:").unwrap_or("");
        let ready = state.contains("READY");
        if !ready {
            ctx.note(format!(
                "SIM not ready: {}",
                if state.is_empty() {
                    "(no +CPIN)"
                } else {
                    state
                }
            ));
        }
        Ok(outcome(out, r.ok() && ready))
    }
}

/// `AT+CIMI` answers the IMSI bare: the first line that is all digits.
fn imsi_value(reply: &crate::at::Reply) -> String {
    reply
        .lines
        .iter()
        .map(|l| l.trim())
        .find(|l| l.len() >= 6 && l.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or("-")
        .to_string()
}

/// `+CCID: 8986012345678901234`
fn iccid_value(reply: &crate::at::Reply) -> String {
    reply
        .first_with_prefix("+CCID:")
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "-".to_string())
}

/// `+CNUM: "","+8613800138000",145` — the address is the field after the
/// alpha tag.  A card with no MSISDN answers with nothing to report.
fn cnum_value(reply: &crate::at::Reply) -> Option<String> {
    let line = reply.first_with_prefix("+CNUM:")?;
    let fields = split_fields(line.split_once(':')?.1);
    fields
        .iter()
        .map(|f| f.trim().trim_matches('"'))
        .find(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit() || c == '+' || c == '*'))
        .map(|f| f.to_string())
}

/// The payload of a `+CEREG:`-style answer, for the summary lines a client
/// reads instead of re-parsing AT framing.  `-` means the modem did not answer
/// the query at all, which is a different fact from an empty answer.
fn reg_value(reply: &crate::at::Reply, prefix: &str) -> String {
    reply
        .first_with_prefix(prefix)
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "-".to_string())
}

/// `+COPS:`'s comma-separated fields, quote-aware (an operator name may be
/// spelled in any of three formats, and the numeric one is quoted).
fn cops_fields(reply: &crate::at::Reply) -> Option<Vec<String>> {
    let line = reply.first_with_prefix("+COPS:")?;
    Some(split_fields(line.split_once(':')?.1))
}

/// Field `field` of the `which`-th `+COPS:` line.  The triple-format query
/// answers three lines in a row — long name, short name, numeric plus AcT —
/// so a single reply carries all of it and the index picks the line.
fn cops_field(lines: &[String], which: usize, field: usize) -> Option<String> {
    lines
        .iter()
        .filter(|l| l.trim_start().starts_with("+COPS:"))
        .nth(which)
        .and_then(|l| l.split_once(':'))
        .map(|(_, body)| split_fields(body))
        .and_then(|fields| fields.get(field).cloned())
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// The operator's name for an MCC-MNC, where this build knows one.  The
/// mapping is public numbering-plan data, not a device identifier; an unknown
/// code is reported as unknown rather than guessed at.
pub fn operator_name(numeric: &str) -> Option<&'static str> {
    Some(match numeric {
        "46000" | "46002" | "46004" | "46007" | "46008" => "中国移动",
        "46001" | "46006" | "46009" => "中国联通",
        "46003" | "46005" | "46011" => "中国电信",
        "46015" => "中国广电",
        _ => return None,
    })
}

// ---------------------------------------------------------------------- CFUN

pub struct Cfun;

impl Capability for Cfun {
    fn name(&self) -> &'static str {
        "cfun"
    }

    fn summary(&self) -> &'static str {
        "radio power (AT+CFUN?; this family's AT+SFUN=2/4; 'cold' is the CFUN=0 -> SFUN cold cycle)"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("status");
        let mut out = Vec::new();
        let ok;

        match action {
            "status" => {
                let r = session.command("AT+CFUN?", Duration::from_secs(5), &[], 0);
                emit(&mut out, "AT+CFUN?", &r);
                ok = r.ok();
            }
            "off" => {
                let r = session.command("AT+CFUN=0", Duration::from_secs(15), &[], 0);
                emit(&mut out, "AT+CFUN=0", &r);
                ok = r.ok();
            }
            // The Unisoc stack needs SFUN, not CFUN, to come back from off:
            // a cold CP can read +CFUN: 1 with the stack still down.
            "on" => {
                for cmd in ["AT+SFUN=2", "AT+SFUN=4"] {
                    let r = session.command(cmd, Duration::from_secs(25), &[], 0);
                    emit(&mut out, cmd, &r);
                }
                let r = session.command("AT+CFUN?", Duration::from_secs(5), &[], 0);
                emit(&mut out, "AT+CFUN?", &r);
                ok = r
                    .first_with_prefix("+CFUN:")
                    .map(|l| l.contains('1'))
                    .unwrap_or(false);
            }
            // FINDINGS 2: a RIL shutdown parks the CP at +CFUN: 0, and there the
            // SFUN pair alone is not stack bring-up -- +CFUN: 1 comes back with no
            // registration.  The cold cycle is the documented recovery for that
            // state, and the one to run when `on` leaves +CFUN: 1 but no RF.
            "cold" => {
                let r = session.command("AT+CFUN=0", Duration::from_secs(15), &[], 0);
                emit(&mut out, "AT+CFUN=0", &r);
                for cmd in ["AT+SFUN=2", "AT+SFUN=4"] {
                    let r = session.command(cmd, Duration::from_secs(25), &[], 0);
                    emit(&mut out, cmd, &r);
                }
                let r = session.command("AT+CFUN?", Duration::from_secs(5), &[], 0);
                emit(&mut out, "AT+CFUN?", &r);
                ok = r
                    .first_with_prefix("+CFUN:")
                    .map(|l| l.contains('1'))
                    .unwrap_or(false);
            }
            "reset" => {
                let r = session.command("AT+SFUN=4", Duration::from_secs(25), &[], 0);
                emit(&mut out, "AT+SFUN=4", &r);
                ok = r.ok();
            }
            "cycling" => {
                let a = session.command("AT+SFUN=5", Duration::from_secs(20), &[], 0);
                emit(&mut out, "AT+SFUN=5", &a);
                let b = session.command("AT+SFUN=3", Duration::from_secs(10), &[], 0);
                emit(&mut out, "AT+SFUN=3", &b);
                // documented as leaving this modem's SIM undetected until reboot
                ctx.note("SFUN=5/3 leaves this modem's SIM undetected until a reboot".to_string());
                ok = false;
            }
            other => anyhow::bail!("cfun: unknown action {other:?} (status|on|off|cold|reset|cycling)"),
        }
        Ok(outcome(out, ok))
    }
}

// ------------------------------------------------------------------ register

pub struct Register;

impl Capability for Register {
    fn name(&self) -> &'static str {
        "register"
    }

    fn summary(&self) -> &'static str {
        "circuit/packet registration: AT+CEREG?, AT+CREG?, AT+CGATT?"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("status");
        let t = Duration::from_secs(8);
        let mut out = Vec::new();

        match action {
            "status" => {
                let cereg = session.command("AT+CEREG?", t, &[], 0);
                let creg = session.command("AT+CREG?", t, &[], 0);
                let gatt = session.command("AT+CGATT?", t, &[], 0);
                // +C5GREG is this generation's 5G SA registration query.
                let c5greg = session.command("AT+C5GREG?", t, &[], 0);
                for (cmd, reply) in [
                    ("AT+CEREG?", &cereg),
                    ("AT+CREG?", &creg),
                    ("AT+CGATT?", &gatt),
                    ("AT+C5GREG?", &c5greg),
                ] {
                    emit(&mut out, cmd, reply);
                }
                let ok = cereg.ok() && creg.ok() && gatt.ok() && c5greg.ok();

                // The summary lines a client reads without re-parsing AT: the
                // `+CEREG` shape is `n,stat[,tac,ci,act]`, and `act` (11 = NR
                // SA, 13 = EN-DC) is what tells SA from NSA.
                out.push(format!("cereg: {}", reg_value(&cereg, "+CEREG:")));
                out.push(format!("creg: {}", reg_value(&creg, "+CREG:")));
                out.push(format!("c5greg: {}", reg_value(&c5greg, "+C5GREG:")));
                out.push(format!("gatt: {}", reg_value(&gatt, "+CGATT:")));

                let stat = reg_value(&cereg, "+CEREG:");
                if stat.starts_with("0,") {
                    ctx.note(format!("not registered: +CEREG: {stat}"));
                }
                Ok(outcome(out, ok))
            }
            // UE usage setting: whether the modem prefers voice or data.  The
            // CS voice workstream (A9) needs voice-centric; the bearer wants
            // data-centric.  Both are ordinary 27.007-ish settings, no NV.
            "uemode" => {
                let ceus = session.command("AT+CEUS?", t, &[], 0);
                let cemode = session.command("AT+CEMODE?", t, &[], 0);
                let ok = each(&mut out, &[("AT+CEUS?", ceus), ("AT+CEMODE?", cemode)]);
                Ok(outcome(out, ok))
            }
            "data-centric" => {
                let a = session.command("AT+CEUS=0", Duration::from_secs(20), &[], 0);
                let b = session.command("AT+CEMODE=1", Duration::from_secs(20), &[], 0);
                let ok = each(&mut out, &[("AT+CEUS=0", a), ("AT+CEMODE=1", b)]);
                ctx.event("uemode", "data-centric");
                Ok(outcome(out, ok))
            }
            "voice-centric" => {
                let a = session.command("AT+CEUS=1", Duration::from_secs(20), &[], 0);
                let b = session.command("AT+CEMODE=2", Duration::from_secs(20), &[], 0);
                let ok = each(&mut out, &[("AT+CEUS=1", a), ("AT+CEMODE=2", b)]);
                ctx.event("uemode", "voice-centric");
                Ok(outcome(out, ok))
            }
            other => anyhow::bail!(
                "register: unknown action {other:?} (status|uemode|data-centric|voice-centric)"
            ),
        }
    }
}

// -------------------------------------------------------------------- signal

pub struct Signal;

impl Capability for Signal {
    fn name(&self) -> &'static str {
        "signal"
    }

    fn summary(&self) -> &'static str {
        "signal quality (AT+CSQ, AT+CESQ decoded), plus `serving` and `neighbors` measurements"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        match positionals(args).first().map(|s| s.as_str()) {
            // The measurement tree, probed under W5: what the CP answers is
            // reported, and a sub-command it refuses is reported as unsupported
            // rather than filled in from the serving cell.
            Some("serving") => return signal_serving(ctx),
            Some("neighbors") => return signal_neighbors(ctx),
            None | Some("status") => {}
            Some(other) => anyhow::bail!(
                "signal: unknown action {other:?} (status|serving|neighbors)"
            ),
        }
        let session = ctx.at()?;
        let t = Duration::from_secs(8);
        let mut out = Vec::new();

        let csq = session.command("AT+CSQ", t, &[], 0);
        let cesq = session.command("AT+CESQ", t, &[], 0);

        if let Some(line) = cesq.first_with_prefix("+CESQ:") {
            if let Some((rsrp, rsrq, sinr)) = decode_cesq(line) {
                // A field the modem did not report must not look like a very
                // bad reading: "not reported" is a fact, "-115 dBm" is a lie
                // the reader will believe.  Measured on the device: the idle
                // CP answers 255 for every field.
                let dbm = |v: Option<i32>| {
                    v.map(|v| format!("{v} dBm"))
                        .unwrap_or_else(|| "not reported".to_string())
                };
                let db = |v: Option<f64>| {
                    v.map(|v| format!("{v:.1} dB"))
                        .unwrap_or_else(|| "not reported".to_string())
                };
                out.push(format!(
                    "decoded: RSRP {}, RSRQ {}, SINR {}",
                    dbm(rsrp),
                    db(rsrq),
                    db(sinr)
                ));
            }
        }
        let ok = each(&mut out, &[("AT+CSQ", csq), ("AT+CESQ", cesq)]);
        Ok(outcome(out, ok))
    }
}

/// `+CESQ: rxlev,ber,rscp,ecno,rsrq,rsrp[,ssrsrq,ssrsrp,sssinr]`
///
/// `255` is 3GPP's "not reported" marker, not an index: decoding it as
/// `idx-140` yields "115 dBm", which a reader will believe.  An unreported
/// field comes back as `None` so the display can say so.  (Measured on the
/// device, 2026-09-20: an unregistered CP answers 255 in every field.)
pub fn decode_cesq(line: &str) -> Option<(Option<i32>, Option<f64>, Option<f64>)> {
    let body = line.split_once(':')?.1;
    let parts: Vec<&str> = body.split(',').map(|s| s.trim()).collect();
    let idx = |i: usize| parts.get(i).and_then(|v| v.parse::<i32>().ok());
    let rsrp_idx = idx(5)?;
    let rsrq_idx = idx(4)?;
    // 3GPP mapping over the reported *index*, 0..97 -> -140..-44 dBm.
    let reported = |v: i32| (v != 255).then_some(v);
    let rsrp = reported(rsrp_idx).map(|idx| idx - 140);
    let rsrq = reported(rsrq_idx).map(|idx| -19.5 + idx as f64 * 0.5);
    let sinr = idx(8).and_then(reported).map(|v| (v as f64 - 20.0) / 2.0);
    Some((rsrp, rsrq, sinr))
}

// ---------------------------------------------- the measurement tree (W5)

/// One field of a serving-cell summary line: a number, or "not reported".
fn shown(value: Option<f64>) -> String {
    value.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".to_string())
}

fn shown_text(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "-".to_string())
}

fn shown_int(value: Option<u32>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string())
}

fn serving_summary(rat: &str, s: &crate::unisoc_at::Serving) -> Vec<String> {
    vec![
        format!("serving_{rat}_band: {}", shown_text(&s.band)),
        format!("serving_{rat}_earfcn: {}", shown_int(s.earfcn)),
        format!("serving_{rat}_pci: {}", shown_int(s.pci)),
        format!("serving_{rat}_rsrp: {}", shown(s.rsrp)),
        format!("serving_{rat}_rsrq: {}", shown(s.rsrq)),
        format!("serving_{rat}_sinr: {}", shown(s.sinr)),
        format!("serving_{rat}_bandwidth: {}", shown_text(&s.bandwidth)),
        format!("serving_{rat}_cell: {}", shown_text(&s.cell)),
    ]
}

/// The serving cell, out of the generation's own measurement sub-commands.
///
/// These commands are the ones the Android-side helper for this CP generation
/// asks; their *answers* have not been captured on this handset yet, so the
/// parser is allowed to come back empty and this action then reports `not
/// reported`.  That is the W5 rule: a sub-command the CP does not answer
/// leaves a gap in the table, never a plausible-looking number.
fn signal_serving(ctx: &mut Context) -> Result<Outcome> {
    let session = ctx.at()?;
    let t = Duration::from_secs(6);
    let mut out = Vec::new();
    let mut measured = false;

    let queries: [(&str, (u32, u32), fn(&[String]) -> Option<crate::unisoc_at::Serving>); 2] = [
        (
            "lte",
            crate::unisoc_at::ENGMD_LTE_SERVING,
            crate::unisoc_at::parse_lte_serving,
        ),
        (
            "nr",
            crate::unisoc_at::ENGMD_NR_SERVING,
            crate::unisoc_at::parse_nr_serving,
        ),
    ];

    for (rat, (group, index), parse) in queries {
        let cmd = crate::unisoc_at::engmd(group, index);
        let reply = session.command(&cmd, t, &[], 0);
        emit(&mut out, &cmd, &reply);
        match parse(&reply.lines) {
            Some(serving) => {
                measured = true;
                out.push(format!(
                    "  -> {:<3} band {}, PCI {}, EARFCN {}, RSRP {} dBm, RSRQ {} dB",
                    rat.to_uppercase(),
                    shown_text(&serving.band),
                    shown_int(serving.pci),
                    shown_int(serving.earfcn),
                    shown(serving.rsrp),
                    shown(serving.rsrq),
                ));
                out.extend(serving_summary(rat, &serving));
            }
            None => out.push(format!("serving_{rat}: not reported")),
        }
    }

    if !measured {
        ctx.note(
            "the CP does not answer the SPENGMD serving queries: no measurement to report"
                .to_string(),
        );
    }
    Ok(if measured {
        Outcome::pass(out)
    } else {
        Outcome::fail(out)
    })
}

/// The neighbour list, probed the same way as the serving cell.
///
/// `not reported` and `0` are kept apart on purpose: the first says the CP
/// refuses the query (or does not have it), the second says it answered and
/// there is nothing in range.  A UI that showed both as "0 neighbours" would
/// be claiming a measurement that was never made.
fn signal_neighbors(ctx: &mut Context) -> Result<Outcome> {
    let session = ctx.at()?;
    let t = Duration::from_secs(8);
    let mut out = Vec::new();

    let ask = |out: &mut Vec<String>, group: u32, index: u32| -> crate::at::Reply {
        let cmd = crate::unisoc_at::engmd(group, index);
        let reply = session.command(&cmd, t, &[], 0);
        emit(out, &cmd, &reply);
        reply
    };
    let lte = ask(&mut out, crate::unisoc_at::ENGMD_LTE_NEIGHBORS.0, crate::unisoc_at::ENGMD_LTE_NEIGHBORS.1);
    let nr = ask(&mut out, crate::unisoc_at::ENGMD_NR_NEIGHBORS.0, crate::unisoc_at::ENGMD_NR_NEIGHBORS.1);

    let lte_cells = crate::unisoc_at::parse_lte_neighbors(&lte.lines);
    let nr_cells = crate::unisoc_at::parse_nr_neighbors(&nr.lines);

    let mut read_any = false;
    for (rat, cells, reply) in [("LTE", &lte_cells, &lte), ("NR", &nr_cells, &nr)] {
        match cells {
            // A reading means the answer was understood -- the cells it
            // carried, or none at all if there is nothing in range.  An
            // answer nobody could read stays `not reported` instead of being
            // printed as a zero, which is the difference the parsers' Option
            // exists to preserve.
            Some(cells) if reply.ok() => {
                read_any = true;
                for cell in cells {
                    out.push(format!(
                        "neighbor: {rat},band={},earfcn={},pci={},rsrp={:.1},rsrq={:.1}{}",
                        shown_text(&cell.band),
                        cell.earfcn,
                        cell.pci,
                        cell.rsrp,
                        cell.rsrq,
                        cell.sinr
                            .map(|s| format!(",sinr={s:.1}"))
                            .unwrap_or_default()
                    ));
                }
                out.push(format!(
                    "neighbors_{}: {}",
                    rat.to_ascii_lowercase(),
                    cells.len()
                ));
            }
            _ => out.push(format!(
                "neighbors_{}: not reported",
                rat.to_ascii_lowercase()
            )),
        }
    }

    if !read_any {
        ctx.note(
            "the CP does not answer the SPENGMD neighbour queries: it does not report \
             neighbours (or not in a shape this build knows)"
                .to_string(),
        );
    }
    Ok(if read_any {
        Outcome::pass(out)
    } else {
        Outcome::fail(out)
    })
}

// ------------------------------------------------------------------ operator

pub struct Operator;

impl Capability for Operator {
    fn name(&self) -> &'static str {
        "operator"
    }

    fn summary(&self) -> &'static str {
        "network selection: AT+COPS? / scan (AT+COPS=?) / automatic / manual"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("status");
        let mut out = Vec::new();
        let ok;

        match action {
            "status" => {
                // `AT+COPS?` on its own answers only the mode on this CP --
                // measured on the device: `+COPS: 0`, with no operator and no
                // AcT, which is why the operator panel had nothing to show.
                // The vendor RIL asks for all three name formats in one
                // command; the same command is asked here.
                let cmd = "AT+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?";
                let r = session.command(cmd, Duration::from_secs(10), &[], 0);
                emit(&mut out, cmd, &r);
                let long = cops_field(&r.lines, 0, 2);
                let short = cops_field(&r.lines, 1, 2);
                let numeric = cops_field(&r.lines, 2, 2);
                let act = cops_field(&r.lines, 2, 3);

                // A firmware that answers the plain query with everything still
                // works: fall back to the single line for the code and the AcT
                // (the name form is not in it, so the name stays unknown rather
                // than being filled in with the number).
                let (name, numeric, act) = if numeric.is_some() {
                    (long.or(short), numeric, act)
                } else {
                    let plain = session.command("AT+COPS?", Duration::from_secs(8), &[], 0);
                    emit(&mut out, "AT+COPS?", &plain);
                    let fields = cops_fields(&plain);
                    let field = |i: usize| {
                        fields
                            .as_ref()
                            .and_then(|f| f.get(i))
                            .map(|v| v.trim().trim_matches('"').to_string())
                            .filter(|v| !v.is_empty())
                    };
                    (None, field(2), field(3))
                };

                let or_dash = |v: Option<String>| v.unwrap_or_else(|| "-".to_string());
                out.push(format!("operator_numeric: {}", or_dash(numeric)));
                out.push(format!("operator_name: {}", or_dash(name)));
                out.push(format!("operator_act: {}", or_dash(act)));
                ok = r.ok();
            }
            "scan" => {
                // A full scan takes tens of seconds and answers with a long
                // list; the vendor oracle is Android's own scan result.
                let r = session.command("AT+COPS=?", Duration::from_secs(180), &[], 0);
                emit(&mut out, "AT+COPS=?", &r);
                ok = r.ok() && r.lines.iter().any(|l| l.contains('('));
            }
            "auto" => {
                let r = session.command("AT+COPS=0", Duration::from_secs(120), &[], 0);
                emit(&mut out, "AT+COPS=0", &r);
                ok = r.ok();
            }
            "manual" => {
                let Some(mccmnc) = pos.get(1) else {
                    anyhow::bail!("operator manual needs an MCC+MNC, e.g. 46001");
                };
                let cmd = format!("AT+COPS=1,2,\"{mccmnc}\"");
                let r = session.command(&cmd, Duration::from_secs(120), &[], 0);
                emit(&mut out, &cmd, &r);
                ok = r.ok();
            }
            other => anyhow::bail!(
                "operator: unknown action {other:?} (status|scan|auto|manual <mccmnc>)"
            ),
        }
        Ok(outcome(out, ok))
    }
}

// ----------------------------------------------------------------------- SMS

pub struct Sms;

impl Capability for Sms {
    fn name(&self) -> &'static str {
        "sms"
    }

    fn summary(&self) -> &'static str {
        "SMS: text mode, list/read/delete, and MO send (AT+CMGS with its '>' prompt)"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("list");
        let mut out = Vec::new();
        let mut ok;

        // Text mode first: the listing format is what the oracle compares.
        let mode = session.command("AT+CMGF=1", Duration::from_secs(10), &[], 0);
        emit(&mut out, "AT+CMGF=1", &mode);
        ok = mode.ok();

        match action {
            // The SMS service surface, read-only: which service is selected,
            // which storages exist, and the service-centre address the MO path
            // will use.  A `+CMS ERROR: 302` on send is usually decided by one
            // of these, not by the send itself.
            "status" => {
                let mut ok = true;
                let mut smsc: Option<String> = None;
                for cmd in ["AT+CMGF?", "AT+CSMS?", "AT+CPMS?", "AT+CSCA?", "AT+CSCS?", "AT+CNMI?"] {
                    let r = session.command(cmd, Duration::from_secs(8), &[], 0);
                    emit(&mut out, cmd, &r);
                    if cmd == "AT+CSCA?" {
                        // The stored value may be hex-of-ASCII (the RIL writes it
                        // under CSCS="HEX"); the summary names the number itself.
                        smsc = r.first_with_prefix("+CSCA:").and_then(smsc_from_answer);
                    }
                    ok &= r.ok();
                }
                out.push(format!("smsc: {}", smsc.as_deref().unwrap_or("-")));
                return Ok(outcome(out, ok));
            }
            // Re-set the service-centre address.  Measured on the device: the
            // RIL leaves the SMSC written under `CSCS="HEX"`, so after the
            // daemon moves the character set to GSM the stored value reads
            // back as hex-of-ASCII and a send answers `+CMS ERROR: 313`.  The
            // owner re-arms it in the charset the daemon now speaks.
            "csca" => {
                let Some(number) = pos.get(1) else {
                    anyhow::bail!(
                        "sms csca needs the service-centre address, e.g. +8613800755500"
                    );
                };
                let cmd = format!("AT+CSCA=\"{number}\"");
                let r = session.command(&cmd, Duration::from_secs(15), &[], 0);
                emit(&mut out, &cmd, &r);
                let after = session.command("AT+CSCA?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+CSCA?", &after);
                let read_back = after.first_with_prefix("+CSCA:").unwrap_or("");
                // The readback must now name the number itself, not a hex
                // rendering of it: a write that did not take must not look
                // like one.
                let ok = r.ok() && read_back.contains(number.as_str());
                if !ok {
                    ctx.note(format!("CSCA readback does not name {number}: {read_back}"));
                }
                return Ok(outcome(out, ok));
            }
            "list" => {
                let r = session.command("AT+CMGL=\"ALL\"", Duration::from_secs(30), &[], 0);
                emit(&mut out, "AT+CMGL=\"ALL\"", &r);
                ok &= r.ok();
            }
            "read" => {
                let Some(idx) = pos.get(1) else {
                    anyhow::bail!("sms read needs an index");
                };
                let cmd = format!("AT+CMGR={idx}");
                let r = session.command(&cmd, Duration::from_secs(20), &[], 0);
                emit(&mut out, &cmd, &r);
                ok &= r.ok();
            }
            "delete" => {
                let Some(idx) = pos.get(1) else {
                    anyhow::bail!("sms delete needs an index, or ALL");
                };
                let cmd = if idx.eq_ignore_ascii_case("all") {
                    "AT+CMGD=1,4".to_string()
                } else {
                    format!("AT+CMGD={idx}")
                };
                let r = session.command(&cmd, Duration::from_secs(20), &[], 0);
                emit(&mut out, &cmd, &r);
                ok &= r.ok();
            }
            "send" => {
                let (Some(number), Some(text)) = (pos.get(1), pos.get(2)) else {
                    anyhow::bail!("sms send needs <number> <text>");
                };
                // The submit goes out in PDU mode, on purpose: this CP's
                // text-mode submit answers `+CMS ERROR: 313` against a SIM
                // that receives fine (measured, 2026-09-20), text mode cannot
                // carry 中文 under a GSM charset, and PDU is what the vendor
                // RIL does.  Text mode is restored afterwards, because the
                // resident owner's MT reader depends on it.
                let pdu_mode = session.command("AT+CMGF=0", Duration::from_secs(10), &[], 0);
                emit(&mut out, "AT+CMGF=0", &pdu_mode);
                ok &= pdu_mode.ok();

                // The CP refuses a submit whose service-centre field is empty
                // (measured: `+CMS ERROR: 302`), so the SMSC is read and named
                // explicitly.
                let smsc = session
                    .command("AT+CSCA?", Duration::from_secs(8), &[], 0)
                    .first_with_prefix("+CSCA:")
                    .and_then(smsc_from_answer);
                match crate::pdu::encode_submit(smsc.as_deref(), number, text) {
                    Ok((hex, octets)) => {
                        // The full PDU goes into the output: it is the oracle
                        // difference against Android, and it is how "the
                        // number was never submitted" gets disproved.
                        out.push(format!("pdu({octets}): {hex}"));
                        let cmd = format!("AT+CMGS={octets}");
                        let r = session.command_prompted(
                            &cmd,
                            Duration::from_secs(15),
                            &hex,
                            Duration::from_secs(120),
                        );
                        emit(&mut out, &format!("{cmd} <pdu>"), &r);
                        ok &= r.ok() && r.line_with("+CMGS:").is_some();
                    }
                    Err(e) => {
                        ctx.note(format!("the message was never submitted: {e}"));
                        out.push(format!("not submitted: {e}"));
                        ok = false;
                    }
                }

                let restore = session.command("AT+CMGF=1", Duration::from_secs(10), &[], 0);
                emit(&mut out, "AT+CMGF=1", &restore);
                ok &= restore.ok();
            }
            other => anyhow::bail!(
                "sms: unknown action {other:?} (status|list|read <i>|delete <i|all>|send <num> <text>)"
            ),
        }
        Ok(outcome(out, ok))
    }
}

// ------------------------------------------------------- a text-mode message

/// One message as the modem reported it, parsed out of an `AT+CMGR` reply in
/// text mode.  This is the shape the resident owner hands a client when the
/// modem announces a message with `+CMTI:` — the MT half of A5.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextMessage {
    /// The storage the `+CMTI` named (`"SM"`, `"ME"`); filled in by the caller,
    /// because the `CMGR` reply does not carry it.
    pub storage: String,
    pub index: u32,
    /// The modem's own status word: `REC UNREAD`, `REC READ`, `STO SENT`, …
    pub status: String,
    /// Originator address, as reported (MO storage entries report the
    /// destination here instead — the field is `<oa>/<da>`).
    pub from: String,
    /// The service-centre timestamp, verbatim (`"26/09/20,10:00:00+32"`).
    pub timestamp: String,
    pub text: String,
}

/// Comma-split with quote awareness.  Two things make a naive `split(',')`
/// wrong here: a quoted field may contain commas (the service-centre timestamp
/// does), and an *empty* field is still a field (the alphabet name between the
/// address and the timestamp usually is — losing it shifts every later field).
fn split_fields(body: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in body.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    fields.push(current.trim().to_string());
    fields
}

/// The status words a text-mode `+CMGR` header may carry (27.005).  Anything
/// else means the reply is not a text-mode message — PDU mode answers with a
/// bare length, and this daemon does not decode PDU, so it must say so rather
/// than hand back hex dressed up as a text.
const CMGR_TEXT_STATUSES: &[&str] = &["REC UNREAD", "REC READ", "STO UNSENT", "STO SENT", "ALL"];

/// A body the modem could not convert into the TE character set arrives as
/// UCS2 hex: `"6D4B8BD5"` *is* "测试".  Measured on the device with
/// `CSCS="GSM"` — the address fields come through as text, a Chinese body
/// comes through as hex.
///
/// The trade is explicit: a GSM-7 body that literally *is* hex-looking text
/// decodes wrongly.  On this network a Chinese message is far more likely than
/// a message that spells hex, so the modem's UCS2 rendering wins.
fn decode_ucs2_hex(body: &str) -> String {
    let t = body.trim();
    if t.len() < 4 || t.len() % 4 != 0 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return body.to_string();
    }
    let units: Vec<u16> = (0..t.len())
        .step_by(4)
        .filter_map(|i| u16::from_str_radix(&t[i..i + 4], 16).ok())
        .collect();
    if units.len() * 4 != t.len() {
        return body.to_string();
    }
    String::from_utf16(&units).unwrap_or_else(|_| body.to_string())
}

/// `+CSCA: "<number>",<toa>` — the number, decoded when the modem stored it
/// as hex-of-ASCII.  Measured on the device: the RIL writes the SMSC under
/// `CSCS="HEX"`, which makes the readback read `2B3836…` for `+8613800755000`.
fn smsc_from_answer(line: &str) -> Option<String> {
    let field = line.split_once(':')?.1.trim();
    let value = field.split(',').next()?.trim().trim_matches('"');
    if value.is_empty() {
        return None;
    }
    if value.starts_with('+') || value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(value.to_string());
    }
    if value.len() % 2 == 0 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        let decoded: String = (0..value.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(&value[i..i + 2], 16).ok())
            .map(|b| b as char)
            .collect();
        if decoded.starts_with('+') || decoded.bytes().all(|b| b.is_ascii_digit()) {
            return Some(decoded);
        }
    }
    None
}

/// Parse the reply lines of `AT+CMGR=<index>` in text mode: a `+CMGR:` header
/// of comma-separated quoted fields, then the body up to the final result
/// code, which the caller's `lines` already excludes (`Reply.lines`).
///
/// `None` means "not a text-mode message": no header, or a header that does
/// not carry one of the 27.005 status words.
pub fn parse_cmgr(index: u32, lines: &[String]) -> Option<TextMessage> {
    let header = lines.iter().find(|l| l.starts_with("+CMGR:"))?;
    let fields = split_fields(header.split_once(':')?.1);

    let status = fields.first()?.to_ascii_uppercase();
    if !CMGR_TEXT_STATUSES.contains(&status.as_str()) {
        return None;
    }

    let text = decode_ucs2_hex(
        &lines
            .iter()
            .filter(|l| !l.starts_with("+CMGR:"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
    );

    Some(TextMessage {
        storage: String::new(),
        index,
        status,
        from: fields.get(1).cloned().unwrap_or_default(),
        timestamp: fields.get(3).cloned().unwrap_or_default(),
        text,
    })
}

// ---------------------------------------------------------------------- USSD

pub struct Ussd;

impl Capability for Ussd {
    fn name(&self) -> &'static str {
        "ussd"
    }

    fn summary(&self) -> &'static str {
        "USSD session: AT+CUSD=1,\"<code>\",15"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let code = pos
            .first()
            .cloned()
            .or_else(|| flag_value(args, "--code").map(|s| s.to_string()));
        let mut out = Vec::new();

        match code {
            Some(code) => {
                let cmd = format!("AT+CUSD=1,\"{code}\",15");
                let expect = vec!["+CUSD:".to_string()];
                let r = session.command(&cmd, Duration::from_secs(30), &expect, 0);
                emit(&mut out, &cmd, &r);
                let answered = r
                    .line_with("+CUSD:")
                    .map(|l| !l.contains("+CUSD: 2") && !l.trim_end().ends_with("\"\""))
                    .unwrap_or(false);
                Ok(outcome(out, r.ok() && answered))
            }
            None => {
                let r = session.command("AT+CUSD=2", Duration::from_secs(15), &[], 0);
                emit(&mut out, "AT+CUSD=2", &r);
                out.push("(no code given; cancelled any open USSD session)".into());
                Ok(outcome(out, r.ok()))
            }
        }
    }
}

// ---------------------------------------------------------------------- call

pub struct Call;

impl Capability for Call {
    fn name(&self) -> &'static str {
        "call"
    }

    fn summary(&self) -> &'static str {
        "voice CS: dial/answer/hangup/DTMF/list (ATD, ATA, ATH, AT+VTS, AT+CLCC)"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let audio = ctx.profile.voice.supported;
        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("list");
        let expect_connect = vec!["CONNECT".to_string()];
        let mut out = Vec::new();
        if !audio {
            // Call signaling is measurable without an audio path; the audio
            // itself is the platform hook that is the other half of W4.
            out.push(
                "no audio route in this profile (voice.supported = false): \
                 signaling only -- the far end may hear silence"
                    .to_string(),
            );
            ctx.note("voice.supported = false: no UCM/voice route on this platform");
        }
        let ok;

        match action {
            "dial" => {
                let Some(number) = pos.get(1) else {
                    anyhow::bail!("call dial needs a number");
                };
                let cmd = format!("ATD{number};");
                let r = session.command(&cmd, Duration::from_secs(60), &expect_connect, 0);
                emit(&mut out, &cmd, &r);
                ok = r.ok();
            }
            "answer" => {
                let r = session.command("ATA", Duration::from_secs(30), &expect_connect, 0);
                emit(&mut out, "ATA", &r);
                ok = r.ok();
            }
            "hangup" => {
                let r = session.command("ATH", Duration::from_secs(20), &[], 0);
                emit(&mut out, "ATH", &r);
                ok = r.ok();
            }
            "dtmf" => {
                let Some(digit) = pos.get(1) else {
                    anyhow::bail!("call dtmf needs a digit");
                };
                let cmd = format!("AT+VTS=\"{digit}\"");
                let r = session.command(&cmd, Duration::from_secs(15), &[], 0);
                emit(&mut out, &cmd, &r);
                ok = r.ok();
            }
            "list" => {
                let r = session.command("AT+CLCC", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+CLCC", &r);
                ok = r.ok();
            }
            other => anyhow::bail!(
                "call: unknown action {other:?} (dial <n>|answer|hangup|dtmf <d>|list)"
            ),
        }
        Ok(outcome(out, ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cesq_treats_255_as_not_reported() {
        // The measured idle answer: every field 255.  Decoding it as an index
        // yields "115 dBm", which is exactly the lie this test pins out.
        let (rsrp, rsrq, sinr) = decode_cesq("+CESQ: 99,99,255,255,255,255,75,67,73").unwrap();
        assert_eq!(rsrp, None);
        assert_eq!(rsrq, None);
        // The SS-SINR field (73) *was* reported, and only it decodes.
        assert_eq!(sinr, Some(26.5));
    }

    /// A short `+CESQ` (no SS- fields at all) still decodes the ones it has.
    #[test]
    fn cesq_without_the_ss_fields_still_decodes() {
        let (rsrp, rsrq, sinr) = decode_cesq("+CESQ: 99,99,255,255,20,60").unwrap();
        assert_eq!(rsrp, Some(-80));
        assert_eq!(rsrq, Some(-9.5));
        assert_eq!(sinr, None);
    }

    #[test]
    fn a_cmgr_reply_parses_into_a_message() {
        // The shape that matters: an empty alphabet field between the address
        // and the timestamp, and a comma *inside* the timestamp's quotes.
        let lines = vec![
            "+CMGR: \"REC UNREAD\",\"+8613800138000\",,\"26/09/20,10:00:00+32\"".to_string(),
            "hello, world".to_string(),
        ];
        let message = parse_cmgr(7, &lines).unwrap();
        assert_eq!(message.index, 7);
        assert_eq!(message.status, "REC UNREAD");
        assert_eq!(message.from, "+8613800138000");
        assert_eq!(message.timestamp, "26/09/20,10:00:00+32");
        assert_eq!(message.text, "hello, world");
        assert_eq!(message.storage, "");
    }

    #[test]
    fn a_multi_line_body_stays_whole() {
        let lines = vec![
            "+CMGR: \"REC READ\",\"10086\",\"CMCC\",\"26/09/20,10:00:00+32\"".to_string(),
            "line one".to_string(),
            "line two".to_string(),
        ];
        let message = parse_cmgr(1, &lines).unwrap();
        assert_eq!(message.from, "10086");
        assert_eq!(message.timestamp, "26/09/20,10:00:00+32");
        assert_eq!(message.text, "line one\nline two");
    }

    /// The measured CSCA readback of this unit: the RIL stored the SMSC under
    /// `CSCS="HEX"`, so the GSM readback is hex-of-ASCII.  The send path has
    /// to name the number, not the hex rendering of it.
    #[test]
    fn the_smsc_is_decoded_out_of_the_hex_rendering() {
        let line = "+CSCA: \"2B38363133383030373535353030\",145";
        assert_eq!(smsc_from_answer(line).as_deref(), Some("+8613800755500"));
        assert_eq!(
            smsc_from_answer("+CSCA: \"+8613800755500\",145").as_deref(),
            Some("+8613800755500")
        );
        assert_eq!(smsc_from_answer("+CSCA: \"\",129"), None);
    }

    #[test]
    fn a_chinese_body_arriving_as_ucs2_hex_is_decoded() {
        // Measured on the device: "测试" arrives as `6D4B8BD5` under
        // `CSCS="GSM"`, because the modem cannot convert it.
        let lines = vec![
            "+CMGR: \"REC READ\",\"+8613000000000\",,\"26/09/20,12:50:08+32\"".to_string(),
            "6D4B8BD5".to_string(),
        ];
        let message = parse_cmgr(1, &lines).unwrap();
        assert_eq!(message.text, "测试");
    }

    #[test]
    fn a_text_body_stays_text() {
        let lines = vec![
            "+CMGR: \"REC READ\",\"+8613000000000\",,\"26/09/20,12:50:08+32\"".to_string(),
            "hello from index 7".to_string(),
        ];
        assert_eq!(parse_cmgr(7, &lines).unwrap().text, "hello from index 7");
    }

    #[test]
    fn a_pdu_mode_reply_is_refused_not_misread() {
        // PDU mode answers with a bare length; handing hex back as text would
        // be exactly the kind of quiet lie this module exists to avoid.
        assert!(parse_cmgr(3, &["+CMGR: 25".to_string(), "07914477".to_string()]).is_none());
        assert!(parse_cmgr(3, &[]).is_none());
    }

    #[test]
    fn cesq_decodes_a_real_reading() {
        // rsrp index 60 -> -80 dBm, rsrq index 20 -> -9.5 dB
        let (rsrp, rsrq, _) = decode_cesq("+CESQ: 99,99,255,255,20,60,75,67,73").unwrap();
        assert_eq!(rsrp, Some(-80));
        assert_eq!(rsrq, Some(-9.5));
    }
}
