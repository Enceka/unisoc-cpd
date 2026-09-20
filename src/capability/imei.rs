//! `imei`: read the device identity, probe the AT surface around it, and --
//! only past every guard below -- write it.
//!
//! The guards are the feature.  Each one exists because of a way this can go
//! permanently wrong:
//!
//! 1. `[nv].readonly` must be false in the platform profile -- the profile is
//!    where the owner's decision lives, and the default stays read-only.
//! 2. `[imei].write_command` must be set -- the exact write dialect is a
//!    firmware property.  An empty template means "no verified contract on
//!    this platform" and nothing is sent; nothing is guessed here.
//! 3. The candidate value must be 15 digits and Luhn-valid.  An invalid IMEI
//!    is refused outright; `--allow-bad-checksum` exists for lab dummy
//!    values and is recorded in the run summary when used.
//! 4. A fresh NV backup must be taken first -- the write refuses without one.
//! 5. The write is not believed until the independent diag read-back returns
//!    the value.  If verification is impossible, the outcome is Fail.
//!
//! Identity writes are for your own development hardware: restoring a value
//! lost to a bad flash, or programming a lab IMEI on a device that has none.
//! Altering a device's identity to disguise it is a crime in a number of
//! jurisdictions, and networks blacklist by IMEI.  Every write is recorded in
//! the run summary.

use super::{emit, flag_value, has_flag, positionals, Capability, Outcome};
use crate::context::Context;
use crate::identity::{self, IMEI_RECORD_MARKER};
use anyhow::{bail, Result};
use std::time::Duration;

/// Read-form identity commands to probe when the profile lists none.
const DEFAULT_PROBES: &[&str] = &["AT+SPIMEI?", "AT+EGMR?", "AT+SPSN?"];

/// The slot label for an item index, in the CP's and Android's zero-based
/// naming: the first SIM slot is IMEI 0.  The third NV item exists in the
/// identity table but is a spare on this CP family.
pub fn sim_slot_label(index: u32) -> &'static str {
    match index {
        0 => "SIM 1",
        1 => "SIM 2",
        _ => "spare",
    }
}

/// Parse and validate the `write` arguments.  Pure, so the guards are
/// testable without a device: returns (imei, index, backup_dir).
pub fn parse_write_args(args: &[String]) -> Result<(String, u32, Option<String>)> {
    if !has_flag(args, "--yes") {
        bail!(
            "imei write refuses without --yes: this changes device identity. \
             Take a backup first (nv backup), then pass --yes explicitly."
        );
    }
    let pos = positionals(args);
    let Some(imei) = pos.get(1).map(|s| s.trim().to_string()) else {
        bail!("imei write needs a 15-digit IMEI: write <imei> --index 0|1|2 --yes");
    };
    if !imei.bytes().all(|b| b.is_ascii_digit()) || imei.len() != 15 {
        bail!("imei must be exactly 15 ASCII digits, got {imei:?}");
    }
    if !identity::luhn_valid(&imei) && !has_flag(args, "--allow-bad-checksum") {
        bail!(
            "check digit does not satisfy Luhn (expected {} as the 15th digit); \
             refusing a structurally invalid IMEI. \
             --allow-bad-checksum overrides this for lab dummy values.",
            identity::imei_check_digit(&imei[..14])
                .map(|d| d.to_string())
                .unwrap_or_default()
        );
    }
    // Indices follow the CP's and Android's zero-based naming: SIM slot 1 is
    // IMEI 0.  The substituted {index} in a write template is this value.
    let index = flag_value(args, "--index")
        .map(|v| v.parse::<u32>())
        .transpose()
        .map_err(|_| anyhow::anyhow!("--index expects 0, 1 or 2"))?
        .unwrap_or(0);
    if index > 2 {
        bail!("--index expects 0, 1 or 2 (SIM 1, SIM 2, spare)");
    }
    let backup_dir = flag_value(args, "--backup-dir").map(|s| s.to_string());
    Ok((imei, index, backup_dir))
}

/// Substitute the profile's write template.  Pure, testable.
pub fn render_write_command(template: &str, imei: &str, index: u32) -> String {
    template
        .replace("{imei}", imei)
        .replace("{index}", &index.to_string())
}

fn item_id_for(ctx: &Context, index: u32) -> Result<String> {
    ctx.profile
        .nv
        .imei_items
        .iter()
        .find(|it| it.index == index)
        .map(|it| it.id.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "profile {:?} lists no imei item for index {index} ([nv].imei_items)",
                ctx.profile.name
            )
        })
}

fn diag_node(ctx: &Context) -> Result<&std::path::Path> {
    ctx.profile
        .nv
        .diag_node
        .as_deref()
        .map(std::path::Path::new)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "profile {:?} sets no [nv].diag_node; the diag read contract is \
                 unverified here (contracts section 8)",
                ctx.profile.name
            )
        })
}

/// One diag read of one identity item, decoded.
pub fn read_item(ctx: &Context, index: u32) -> Result<String> {
    let id = item_id_for(ctx, index)?;
    let node = diag_node(ctx)?;
    let request = identity::nv_read_frame(&id)?;
    let reply = identity::diag_exchange(node, &request, Duration::from_millis(400))?;
    identity::extract_imei(&reply, IMEI_RECORD_MARKER)
        .ok_or_else(|| anyhow::anyhow!("no 15-digit identity record after the marker in the reply"))
}

pub struct Imei;

impl Capability for Imei {
    fn name(&self) -> &'static str {
        "imei"
    }

    fn summary(&self) -> &'static str {
        "device identity (guarded): read [--index N] | probe | write <imei> --index N --yes"
    }

    fn native_only(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let pos = positionals(args);
        match pos.first().map(|s| s.as_str()).unwrap_or("read") {
            "read" => {
                let mut out = Vec::new();
                if ctx.profile.nv.imei_items.is_empty() {
                    bail!("profile {:?} lists no [nv].imei_items", ctx.profile.name);
                }
                let wanted: Option<u32> = flag_value(args, "--index")
                    .map(|v| v.parse())
                    .transpose()
                    .map_err(|_| anyhow::anyhow!("--index expects 0, 1 or 2"))?;
                let mut any = false;
                for it in &ctx.profile.nv.imei_items {
                    if wanted.is_some_and(|w| w != it.index) {
                        continue;
                    }
                    match read_item(ctx, it.index) {
                        Ok(imei) => {
                            any = true;
                            out.push(format!(
                                "imei{} ({}, item {}) = {}{}",
                                it.index,
                                sim_slot_label(it.index),
                                it.id,
                                imei,
                                if identity::luhn_valid(&imei) {
                                    ""
                                } else {
                                    "  (LUHN INVALID -- treat with suspicion)"
                                }
                            ));
                        }
                        Err(e) => out.push(format!(
                            "imei{} ({}, item {}): {e:#}",
                            it.index,
                            sim_slot_label(it.index),
                            it.id
                        )),
                    }
                }
                ctx.event("imei-read", out.join(" | "));
                if any {
                    Ok(Outcome::pass(out))
                } else {
                    out.push("no item produced a record; try imei probe for the AT surface".into());
                    Ok(Outcome::fail(out))
                }
            }
            "probe" => {
                let session = ctx.at()?;
                let mut out = Vec::new();
                let commands: Vec<String> = if ctx.profile.imei.probes.is_empty() {
                    DEFAULT_PROBES.iter().map(|s| s.to_string()).collect()
                } else {
                    ctx.profile.imei.probes.clone()
                };
                let mut any_ok = false;
                for cmd in &commands {
                    let reply = session.command(cmd, Duration::from_secs(6), &[], 0);
                    any_ok |= reply.ok();
                    emit(&mut out, cmd, &reply);
                }
                out.push(
                    "probe only reports what the CP answered; a write template is not \
                     derived from these answers (contracts section 8)"
                        .into(),
                );
                if any_ok {
                    Ok(Outcome::pass(out))
                } else {
                    Ok(Outcome::fail(out))
                }
            }
            "write" => {
                // Guard 1: the profile records the owner's decision.
                if ctx.profile.nv.readonly {
                    bail!(
                        "profile {:?} declares [nv].readonly = true; identity writes are \
                         refused. Flip it only as an explicit owner decision.",
                        ctx.profile.name
                    );
                }
                // Guard 2: a verified write template.
                let Some(template) = ctx.profile.imei.write_template() else {
                    bail!(
                        "profile {:?} has no [imei].write_command: no verified write \
                         contract on this platform, so nothing is sent. Probe first \
                         (imei probe), pin the dialect in the profile, only then set \
                         the template.",
                        ctx.profile.name
                    );
                };
                // Guard 3: structural validation (see parse_write_args).
                let (imei, index, backup_dir) = parse_write_args(args)?;
                let item_id = item_id_for(ctx, index)?;
                if !identity::luhn_valid(&imei) {
                    ctx.event("imei-write-luhn-override", imei.clone());
                }

                let mut out = Vec::new();
                out.push(format!(
                    "identity write on {}: imei{index} ({}, item {item_id}) -> {imei}{}",
                    sim_slot_label(index),
                    ctx.profile.name,
                    if has_flag(args, "--allow-bad-checksum") {
                        " (checksum overridden)"
                    } else {
                        ""
                    }
                ));
                out.push(
                    "only on hardware you own; keep the factory value from the label or \
                     a backup; networks blacklist by IMEI and disguising a device is a \
                     crime in a number of jurisdictions"
                        .into(),
                );

                // Guard 4: a fresh backup, or nothing is sent.
                let nodes: Vec<String> = ctx
                    .profile
                    .boot
                    .partitions
                    .iter()
                    .filter(|n| super::side::is_nv_partition(n))
                    .cloned()
                    .collect();
                if nodes.is_empty() {
                    bail!(
                        "profile {:?} lists no NV partitions to back up",
                        ctx.profile.name
                    );
                }
                let dir =
                    super::side::backup_nv_nodes(ctx, &mut out, backup_dir.as_deref(), &nodes)?;
                out.push(format!("backup at {}", dir.display()));

                // The write itself.
                let session = ctx.at()?;
                let cmd = render_write_command(&template, &imei, index);
                let reply = session.command(&cmd, Duration::from_secs(8), &[], 0);
                emit(&mut out, &cmd, &reply);
                ctx.event("imei-write", format!("index={index}"));
                if !reply.ok() {
                    out.push("the CP refused the write; nothing is claimed to have changed".into());
                    return Ok(Outcome::fail(out));
                }

                // Guard 5: believe the read-back, not the ACK.
                match read_item(ctx, index) {
                    Ok(back) if back == imei => {
                        out.push(format!("read-back confirms imei{index} = {back}"));
                        Ok(Outcome::pass(out))
                    }
                    Ok(back) => {
                        out.push(format!(
                            "MISMATCH: read-back shows imei{index} = {back}; the CP accepted \
                             the command but the identity did not stick. Restore the backup \
                             at {} before rebooting the CP.",
                            dir.display()
                        ));
                        Ok(Outcome::fail(out))
                    }
                    Err(e) => {
                        out.push(format!(
                            "VERIFICATION IMPOSSIBLE ({e:#}); the CP accepted the write but \
                             the diag read-back could not confirm it. Check with imei read \
                             before trusting this state."
                        ));
                        Ok(Outcome::fail(out))
                    }
                }
            }
            other => bail!("imei: unknown action {other:?} (read|probe|write)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn write_args_demand_yes_and_a_valid_imei() {
        let good = "490154203237518";
        assert!(
            parse_write_args(&args(&["write"])).is_err(),
            "--yes required"
        );
        assert!(
            parse_write_args(&args(&["write", good])).is_err(),
            "a bare candidate value must still refuse without --yes"
        );
        assert!(parse_write_args(&args(&["write", good, "--yes"])).is_ok());
        // Zero-based: SIM slot 1 is index 0, slot 2 is index 1, 2 is the spare.
        assert!(parse_write_args(&args(&["write", good, "--yes", "--index", "0"])).is_ok());
        assert!(parse_write_args(&args(&["write", good, "--yes", "--index", "1"])).is_ok());
        assert!(parse_write_args(&args(&["write", good, "--yes", "--index", "2"])).is_ok());
        assert!(parse_write_args(&args(&["write", good, "--yes", "--index", "3"])).is_err());
        // Luhn-invalid: refused, unless overridden.
        let bad = "490154203237519";
        assert!(parse_write_args(&args(&["write", bad, "--yes"])).is_err());
        assert!(parse_write_args(&args(&["write", bad, "--yes", "--allow-bad-checksum"])).is_ok());
        // Structural garbage: refused even with the override.
        assert!(
            parse_write_args(&args(&["write", "12345", "--yes", "--allow-bad-checksum"])).is_err()
        );
        assert!(parse_write_args(&args(&["write", "49015420323751x", "--yes"])).is_err());
        assert!(parse_write_args(&args(&["write", good, "--yes", "--index", "7"])).is_err());
        let (parsed, index, dir) = parse_write_args(&args(&[
            "write",
            good,
            "--yes",
            "--index",
            "1",
            "--backup-dir",
            "/tmp/b",
        ]))
        .unwrap();
        assert_eq!(
            (parsed.as_str(), index, dir.as_deref()),
            (good, 1, Some("/tmp/b"))
        );
        // Default index: 0, the first SIM slot, because the CP counts from zero.
        assert_eq!(
            parse_write_args(&args(&["write", good, "--yes"]))
                .unwrap()
                .1,
            0
        );
    }

    #[test]
    fn template_substitution() {
        assert_eq!(
            render_write_command("AT+SPIMEI={index},\"{imei}\"", "490154203237518", 2),
            "AT+SPIMEI=2,\"490154203237518\""
        );
        assert_eq!(render_write_command("AT+X={imei}", "1", 1), "AT+X=1");
    }
}
