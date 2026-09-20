//! Control-plane capabilities: SIM/PIN, CFUN, registration, signal, operator,
//! band, SMS, USSD, voice call.
//!
//! The AT sequences are the ones written down in `docs/BASEBAND-CONTRACTS.md`
//! (captured from the vendor side), so a `native` run and an Android oracle run
//! can be diffed command by command.

use super::{emit, flag_value, positionals, Capability, Outcome};
use crate::context::Context;
use anyhow::Result;
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
            // identity-related hides in here.
            let imsi = session.command("AT+CIMI", t, &[], 0);
            let iccid = session.command("AT+CCID", t, &[], 0);
            let ok = each(&mut out, &[("AT+CIMI", imsi), ("AT+CCID", iccid)]);
            return Ok(outcome(out, ok));
        }

        if let Some(pin) = pos.first() {
            if pin != "info" {
                let cmd = format!("AT+CPIN=\"{pin}\"");
                let r = session.command(&cmd, Duration::from_secs(20), &[], 0);
                emit(&mut out, &cmd, &r);
                let after = session.command("AT+CPIN?", t, &[], 0);
                emit(&mut out, "AT+CPIN?", &after);
                let ok = r.ok() && after.first_with_prefix("+CPIN:").map(|l| l.contains("READY")).unwrap_or(false);
                return Ok(outcome(out, ok));
            }
        }

        let r = session.command("AT+CPIN?", t, &[], 0);
        emit(&mut out, "AT+CPIN?", &r);
        let state = r.first_with_prefix("+CPIN:").unwrap_or("");
        let ready = state.contains("READY");
        if !ready {
            ctx.note(format!("SIM not ready: {}", if state.is_empty() { "(no +CPIN)" } else { state }));
        }
        Ok(outcome(out, r.ok() && ready))
    }
}

// ---------------------------------------------------------------------- CFUN

pub struct Cfun;

impl Capability for Cfun {
    fn name(&self) -> &'static str {
        "cfun"
    }

    fn summary(&self) -> &'static str {
        "radio power (AT+CFUN?, and this family's AT+SFUN=2/4)"
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
                ok = r.first_with_prefix("+CFUN:").map(|l| l.contains('1')).unwrap_or(false);
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
            other => anyhow::bail!("cfun: unknown action {other:?} (status|on|off|reset|cycling)"),
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
                let ok = each(
                    &mut out,
                    &[
                        ("AT+CEREG?", cereg),
                        ("AT+CREG?", creg),
                        ("AT+CGATT?", gatt),
                        ("AT+C5GREG?", c5greg),
                    ],
                );

                // AcT 1/5 registered, 11 = NR SA, 13 = EN-DC.
                let stat = out
                    .iter()
                    .find(|l| l.trim_start().starts_with("+CEREG:"))
                    .map(|l| l.trim().to_string())
                    .unwrap_or_default();
                if stat.contains(": 0,") {
                    ctx.note(format!("not registered: {stat}"));
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
        "signal quality (AT+CSQ, AT+CESQ with RSRP/RSRQ decoded)"
    }

    fn run(&self, ctx: &mut Context, _args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let t = Duration::from_secs(8);
        let mut out = Vec::new();

        let csq = session.command("AT+CSQ", t, &[], 0);
        let cesq = session.command("AT+CESQ", t, &[], 0);

        if let Some(line) = cesq.first_with_prefix("+CESQ:") {
            if let Some((rsrp, rsrq, sinr)) = decode_cesq(line) {
                let sinr = sinr
                    .map(|v| format!("{v:.1} dB"))
                    .unwrap_or_else(|| "n/a".to_string());
                out.push(format!("decoded: RSRP {rsrp} dBm, RSRQ {rsrq:.1} dB, SINR {sinr}"));
            }
        }
        let ok = each(&mut out, &[("AT+CSQ", csq), ("AT+CESQ", cesq)]);
        Ok(outcome(out, ok))
    }
}

/// `+CESQ: rxlev,ber,rscp,ecno,rsrq,rsrp[,ssrsrq,ssrsrp,sssinr]`
pub fn decode_cesq(line: &str) -> Option<(i32, f64, Option<f64>)> {
    let body = line.split_once(':')?.1;
    let parts: Vec<&str> = body.split(',').map(|s| s.trim()).collect();
    let idx = |i: usize| parts.get(i).and_then(|v| v.parse::<i32>().ok());
    let rsrp_idx = idx(5)?;
    let rsrq_idx = idx(4)?;
    // 3GPP mapping over the reported *index*, 0..97 -> -140..-44 dBm.
    let rsrp = rsrp_idx - 140;
    let rsrq = -19.5 + rsrq_idx as f64 * 0.5;
    let sinr = idx(8).map(|v| (v as f64 - 20.0) / 2.0);
    Some((rsrp, rsrq, sinr))
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
                let r = session.command("AT+COPS?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+COPS?", &r);
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
            other => anyhow::bail!("operator: unknown action {other:?} (status|scan|auto|manual <mccmnc>)"),
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
                let cmd = format!("AT+CMGS=\"{number}\"");
                let r = session.command_prompted(
                    &cmd,
                    Duration::from_secs(15),
                    text,
                    Duration::from_secs(120),
                );
                emit(&mut out, &format!("{cmd} <text>"), &r);
                ok &= r.ok() && r.line_with("+CMGS:").is_some();
            }
            other => anyhow::bail!("sms: unknown action {other:?} (list|read <i>|delete <i|all>|send <num> <text>)"),
        }
        Ok(outcome(out, ok))
    }
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
        if !ctx.profile.voice.supported {
            let mut out = vec![
                "voice is not marked supported in this profile".to_string(),
                "AT call control is still reachable, but there is no in-call audio path".to_string(),
            ];
            let session = ctx.at()?;
            let r = session.command("AT+CLCC", Duration::from_secs(8), &[], 0);
            emit(&mut out, "AT+CLCC", &r);
            ctx.note("voice.supported = false in the profile: no UCM/voice route on this platform");
            return Ok(outcome(out, r.ok()));
        }

        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("list");
        let expect_connect = vec!["CONNECT".to_string()];
        let mut out = Vec::new();
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
            other => anyhow::bail!("call: unknown action {other:?} (dial <n>|answer|hangup|dtmf <d>|list)"),
        }
        Ok(outcome(out, ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cesq_decodes_the_index_mapping() {
        let (rsrp, rsrq, _) = decode_cesq("+CESQ: 99,99,255,255,255,255,75,67,73").unwrap();
        assert_eq!(rsrp, 255 - 140);
        assert_eq!(rsrq, -19.5 + 255.0 * 0.5);
    }

    #[test]
    fn cesq_decodes_a_real_reading() {
        // rsrp index 60 -> -80 dBm, rsrq index 20 -> -9.5 dB
        let (rsrp, rsrq, _) = decode_cesq("+CESQ: 99,99,255,255,20,60,75,67,73").unwrap();
        assert_eq!(rsrp, -80);
        assert!((rsrq - (-9.5)).abs() < 1e-9);
    }
}
