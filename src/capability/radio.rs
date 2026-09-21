//! Radio configuration capabilities that only exist on this CP generation:
//! band locking, 5G SA/NSA and the IMS/VoLTE probes.
//!
//! Everything here is expressed through `crate::unisoc_at`, so the tables and
//! bit masks live in one place and are tested there.  Two of these commands
//! (`AT+SPFORCEFRQ` cell locking and `AT+CAVIMS`) sit under the plan's W5
//! "probe whether it works or say it does not" rule: they report exactly what
//! the modem answered rather than pretending a configuration took.

use super::{emit, positionals, Capability, Outcome};
use crate::context::Context;
use crate::unisoc_at::{self, Rat};
use anyhow::{bail, Result};
use std::time::Duration;

fn outcome(lines: Vec<String>, ok: bool) -> Outcome {
    if ok {
        Outcome::pass(lines)
    } else {
        Outcome::fail(lines)
    }
}

fn parse_rat(token: Option<&String>) -> Result<Rat> {
    let Some(token) = token else {
        bail!("this action needs a RAT: lte or nr");
    };
    Rat::parse(token).ok_or_else(|| anyhow::anyhow!("unknown RAT {token:?}: expected lte or nr"))
}

// ---------------------------------------------------------------------- band

pub struct Band;

impl Capability for Band {
    fn name(&self) -> &'static str {
        "band"
    }

    fn summary(&self) -> &'static str {
        "band lock (AT+SPLBAND) and cell lock (AT+SPFORCEFRQ): status|lock|unlock|cell-lock|cell-unlock"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("status");
        let mut out = Vec::new();

        match action {
            "status" => {
                let mut ok = true;
                let sprat = session.command("AT+SPRAT?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+SPRAT?", &sprat);
                ok &= sprat.ok();
                if let Some(line) = sprat.first_with_prefix("+SPRAT:") {
                    out.push(format!(
                        "sprat: {}",
                        line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("")
                    ));
                }

                for rat in [Rat::Lte, Rat::Nr] {
                    let cmd = unisoc_at::band_query_command(rat);
                    let r = session.command(cmd, Duration::from_secs(8), &[], 0);
                    emit(&mut out, cmd, &r);
                    let bands = r
                        .first_with_prefix("+SPLBAND:")
                        .map(|l| unisoc_at::parse_locked_bands(l, rat))
                        .unwrap_or_default();
                    let list = bands
                        .iter()
                        .map(|b| b.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    out.push(format!(
                        "  -> {:<3} locked bands: {}",
                        rat.as_str(),
                        if list.is_empty() {
                            "(none)".to_string()
                        } else {
                            list.clone()
                        }
                    ));
                    // The summary line the web UI reads: `-` is "no lock", so a
                    // panel never has to tell an empty list from a missing one.
                    out.push(format!(
                        "{}_bands: {}",
                        rat.as_str().to_ascii_lowercase(),
                        if list.is_empty() { "-".to_string() } else { list }
                    ));
                    ok &= r.ok();
                }

                for rat in [Rat::Lte, Rat::Nr] {
                    let cmd = unisoc_at::cell_query_command(rat);
                    let r = session.command(&cmd, Duration::from_secs(8), &[], 0);
                    emit(&mut out, &cmd, &r);
                    let cells = r
                        .first_with_prefix("+SPFORCEFRQ:")
                        .map(|l| unisoc_at::parse_locked_cells(l, rat))
                        .unwrap_or_default();
                    if !cells.is_empty() {
                        let shown: Vec<String> =
                            cells.iter().map(|(f, p)| format!("{f}/{p}")).collect();
                        out.push(format!(
                            "  -> {:<3} locked cells: {}",
                            rat.as_str(),
                            shown.join(" ")
                        ));
                    }
                    out.push(format!(
                        "{}_cells: {}",
                        rat.as_str().to_ascii_lowercase(),
                        if cells.is_empty() {
                            "-".to_string()
                        } else {
                            cells
                                .iter()
                                .map(|(f, p)| format!("{f}/{p}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        }
                    ));
                }
                Ok(outcome(out, ok))
            }
            "lock" => {
                let rat = parse_rat(pos.get(1))?;
                let bands: Vec<u32> = pos[2.min(pos.len())..]
                    .iter()
                    .filter_map(|b| b.parse::<u32>().ok())
                    .collect();
                if bands.is_empty() {
                    bail!("band lock needs at least one band number, e.g. `band lock nr 78`");
                }
                let cmd = match rat {
                    Rat::Lte => unisoc_at::lte_band_lock_command(&bands),
                    Rat::Nr => unisoc_at::nr_band_lock_command(&bands),
                };
                let r = session.command(&cmd, Duration::from_secs(30), &[], 0);
                emit(&mut out, &cmd, &r);
                // Read it back: a lock that did not take must not look like one.
                let readback = session.command(
                    unisoc_at::band_query_command(rat),
                    Duration::from_secs(8),
                    &[],
                    0,
                );
                emit(&mut out, unisoc_at::band_query_command(rat), &readback);
                let applied = readback
                    .first_with_prefix("+SPLBAND:")
                    .map(|l| unisoc_at::parse_locked_bands(l, rat))
                    .unwrap_or_default();
                out.push(format!(
                    "  -> {:<3} locked bands: {}",
                    rat.as_str(),
                    if applied.is_empty() {
                        "(none)".to_string()
                    } else {
                        applied
                            .iter()
                            .map(|b| b.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    }
                ));
                let wanted: Vec<u32> = bands.clone();
                let ok = r.ok() && applied == wanted;
                if !ok {
                    ctx.note(format!(
                        "band lock {rat:?} requested {wanted:?} but the modem reports {applied:?}"
                    ));
                } else {
                    ctx.event("band-lock", format!("{} {:?}", rat.as_str(), wanted));
                }
                Ok(outcome(out, ok))
            }
            "unlock" => {
                let which = pos.get(1).map(|s| s.to_ascii_lowercase());
                let rats: Vec<Rat> = match which.as_deref() {
                    Some("all") | None => vec![Rat::Lte, Rat::Nr],
                    Some(_) => vec![parse_rat(pos.get(1))?],
                };
                let mut ok = true;
                for rat in rats {
                    let cmd = unisoc_at::band_unlock_command(rat);
                    let r = session.command(cmd, Duration::from_secs(30), &[], 0);
                    emit(&mut out, cmd, &r);
                    ok &= r.ok();
                    ctx.event("band-unlock", rat.as_str());
                }
                Ok(outcome(out, ok))
            }
            "cell-lock" => {
                let rat = parse_rat(pos.get(1))?;
                let (Some(freq), Some(pci)) = (pos.get(2), pos.get(3)) else {
                    bail!("cell-lock needs <lte|nr> <freq> <pci>");
                };
                let cmd = unisoc_at::cell_lock_command(
                    rat,
                    freq.parse().map_err(|_| anyhow::anyhow!("freq must be a number"))?,
                    pci.parse().map_err(|_| anyhow::anyhow!("pci must be a number"))?,
                );
                let r = session.command(&cmd, Duration::from_secs(30), &[], 0);
                emit(&mut out, &cmd, &r);
                Ok(outcome(out, r.ok()))
            }
            "cell-unlock" => {
                let which = pos.get(1).map(|s| s.to_ascii_lowercase());
                let rats: Vec<Rat> = match which.as_deref() {
                    Some("all") | None => vec![Rat::Lte, Rat::Nr],
                    Some(_) => vec![parse_rat(pos.get(1))?],
                };
                let mut ok = true;
                for rat in rats {
                    let cmd = unisoc_at::cell_unlock_command(rat);
                    let r = session.command(&cmd, Duration::from_secs(30), &[], 0);
                    emit(&mut out, &cmd, &r);
                    ok &= r.ok();
                }
                Ok(outcome(out, ok))
            }
            other => bail!(
                "band: unknown action {other:?} \
                 (status|lock <lte|nr> <band...>|unlock <lte|nr|all>|cell-lock <lte|nr> <freq> <pci>|cell-unlock <lte|nr|all>)"
            ),
        }
    }
}

// ------------------------------------------------------------------ 5G SA/NSA

pub struct Nr5g;

impl Capability for Nr5g {
    fn name(&self) -> &'static str {
        "nr"
    }

    fn summary(&self) -> &'static str {
        "5G SA/NSA preference and 5G registration: status|sa on|sa off"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("status");
        let mut out = Vec::new();
        let mut ok = true;

        match action {
            "status" | "sa" => {
                let q = session.command("AT+SP5GRAN?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+SP5GRAN?", &q);
                if let Some(line) = q.first_with_prefix("+SP5GRAN:") {
                    match unisoc_at::parse_5g_sa(line) {
                        Some(1) => out.push("  -> NR SA allowed".to_string()),
                        Some(0) => out.push("  -> NSA only (SA disabled)".to_string()),
                        _ => out.push("  -> unrecognised +SP5GRAN".to_string()),
                    }
                }
                let c5 = session.command("AT+C5GREG?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+C5GREG?", &c5);
                ok &= q.ok() && c5.ok();
            }
            _ => {}
        }

        if action == "sa" {
            let Some(setting) = pos.get(1) else {
                bail!("nr sa needs on or off");
            };
            let value = match setting.to_ascii_lowercase().as_str() {
                "on" | "1" | "enable" => 1,
                "off" | "0" | "disable" => 0,
                other => bail!("nr sa: expected on or off, got {other:?}"),
            };
            let cmd = format!("AT+SP5GRAN={value}");
            let r = session.command(&cmd, Duration::from_secs(20), &[], 0);
            emit(&mut out, &cmd, &r);
            let readback = session.command("AT+SP5GRAN?", Duration::from_secs(8), &[], 0);
            emit(&mut out, "AT+SP5GRAN?", &readback);
            let applied = readback
                .first_with_prefix("+SP5GRAN:")
                .and_then(unisoc_at::parse_5g_sa);
            out.push(match applied {
                Some(1) => "  -> NR SA allowed".to_string(),
                Some(0) => "  -> NSA only (SA disabled)".to_string(),
                _ => "  -> unrecognised +SP5GRAN".to_string(),
            });
            ok &= r.ok() && applied == Some(value);
            ctx.event("nr-sa", format!("{value}"));
        } else if action != "status" {
            bail!("nr: unknown action {action:?} (status|sa on|sa off)");
        }

        Ok(outcome(out, ok))
    }
}

// ------------------------------------------------------------------- IMS/VoLTE

pub struct Ims;

impl Capability for Ims {
    fn name(&self) -> &'static str {
        "ims"
    }

    fn summary(&self) -> &'static str {
        "VoLTE/VoNR probe and switch, IMS registration: status|volte on|off|vonr on|off (W5: it works, or it does not)"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let session = ctx.at()?;
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("status");
        let mut out = Vec::new();
        let ok;

        match action {
            "status" => {
                let volte = session.command("AT+CAVIMS?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+CAVIMS?", &volte);
                if let Some(line) = volte.first_with_prefix("+CAVIMS:") {
                    out.push(format!(
                        "  -> VoLTE {}",
                        match unisoc_at::parse_volte(line) {
                            Some(1) => "enabled",
                            Some(0) => "disabled",
                            _ => "unknown",
                        }
                    ));
                }
                // +CIREG is the gate a VoLTE call waits on: without an IMS
                // registration there is nothing for a dial to ride.
                let imsreg = session.command("AT+CIREG?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+CIREG?", &imsreg);
                let verdict = match imsreg.first_with_prefix("+CIREG:").and_then(unisoc_at::parse_ims_reg) {
                    Some(1) => "registered: a VoLTE dial is the next thing to try".to_string(),
                    Some(0) => "not registered: nothing is up for a dial to ride".to_string(),
                    Some(state) => format!("in state {state} (shape not yet measured)"),
                    None => "unknown shape (researched, not yet measured on this generation)".to_string(),
                };
                out.push(format!("  -> IMS {verdict}"));
                // VoNR is a vendor command in a quoted form; the answer is
                // only meaningful when the modem already answered +SP5GCMDS.
                let vonr = session.command(
                    "AT+SP5GCMDS=\"get nr synch_param\",42",
                    Duration::from_secs(15),
                    &[],
                    0,
                );
                emit(&mut out, "AT+SP5GCMDS=\"get nr synch_param\",42", &vonr);
                ok = volte.ok() && imsreg.ok();
            }
            "volte" => {
                let Some(setting) = pos.get(1) else {
                    bail!("ims volte needs on or off");
                };
                let value = match setting.to_ascii_lowercase().as_str() {
                    "on" | "1" => 1,
                    "off" | "0" => 0,
                    other => bail!("ims volte: expected on or off, got {other:?}"),
                };
                let cmd = format!("AT+CAVIMS={value}");
                let r = session.command(&cmd, Duration::from_secs(20), &[], 0);
                emit(&mut out, &cmd, &r);
                let readback = session.command("AT+CAVIMS?", Duration::from_secs(8), &[], 0);
                emit(&mut out, "AT+CAVIMS?", &readback);
                let applied = readback
                    .first_with_prefix("+CAVIMS:")
                    .and_then(unisoc_at::parse_volte);
                out.push(format!(
                    "  -> VoLTE {}",
                    match applied {
                        Some(1) => "enabled",
                        Some(0) => "disabled",
                        _ => "unknown",
                    }
                ));
                ok = r.ok() && applied == Some(value);
                ctx.event("volte", format!("{value}"));
            }
            "vonr" => {
                let Some(setting) = pos.get(1) else {
                    bail!("ims vonr needs on or off");
                };
                let value = match setting.to_ascii_lowercase().as_str() {
                    "on" | "1" => 1,
                    "off" | "0" => 0,
                    other => bail!("ims vonr: expected on or off, got {other:?}"),
                };
                let cmd = format!("AT+SP5GCMDS=\"set nr param\",45,{value}");
                let r = session.command(&cmd, Duration::from_secs(20), &[], 0);
                emit(&mut out, &cmd, &r);
                ok = r.ok();
                ctx.event("vonr", format!("{value}"));
            }
            other => bail!("ims: unknown action {other:?} (status|volte on|off|vonr on|off)"),
        }
        Ok(outcome(out, ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rat_tokens_are_checked() {
        let t = "nr".to_string();
        assert_eq!(parse_rat(Some(&t)).unwrap(), Rat::Nr);
        assert!(parse_rat(None).is_err());
        let bad = "3g".to_string();
        assert!(parse_rat(Some(&bad)).is_err());
    }
}
