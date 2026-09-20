//! Side contracts: NV, and diagnostics.
//!
//! Writes here are *guarded*, not forbidden -- the plan's only red line is
//! the AT channel -- because the accident a raw block device invites is one
//! command away:
//!
//! * the profile decides.  `[nv].readonly = true` (the default) keeps every
//!   write path refused;
//! * the only write this capability performs is `restore`: an exact image
//!   whose sha256 somebody vouched for (the backup manifest or `--sha256`),
//!   whose size equals the partition's byte for byte, written with `--yes`,
//!   re-read and hashed after the write;
//! * `backup` is how the images it accepts are produced: every NV partition
//!   of the profile, both slots, into one directory with a manifest.json.
//!
//! There is deliberately still no in-place NV editing here: the fixnv images
//! carry internal checksums the CP's NV service maintains, and hand-editing
//! bytes is how a modem loses its calibration.

use super::{flag_value, has_flag, positionals, Capability, Outcome};
use crate::context::Context;
use crate::probes;
use anyhow::{bail, Context as _, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn nv_candidates(base: &str) -> Vec<PathBuf> {
    let by_name = Path::new("/dev/block/by-name");
    let mut v = vec![by_name.join(base)];
    for slot in ["_a", "_b"] {
        v.push(by_name.join(format!("{base}{slot}")));
    }
    v
}

fn first_existing(base: &str) -> Option<PathBuf> {
    nv_candidates(base).into_iter().find(|p| p.exists())
}

/// The partition names that carry NV, as opposed to firmware images: the
/// heuristic is the name, because that is the only portable signal there is
/// (fixnv, runtimenv, deltanv, ...).
pub(crate) fn is_nv_partition(base: &str) -> bool {
    base.to_ascii_lowercase().contains("nv")
}

/// Raw byte copy that works when either end is a block device.  Both
/// `std::fs::copy` and the `io::copy` fast path demand regular files and
/// refuse `/dev/block/*` outright, which is exactly what NV backup needs.
fn copy_raw(src: &mut dyn std::io::Read, dst: &mut dyn std::io::Write) -> Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        dst.write_all(&buf[..n])?;
        total += n as u64;
    }
}

/// Copy every (or only the named) NV partition of the profile, both slots,
/// into one directory, and write a manifest.json that vouches for each image.
/// This is also the mandatory first step of every identity write.
pub(crate) fn backup_nv_nodes(
    ctx: &mut Context,
    out: &mut Vec<String>,
    dir: Option<&str>,
    only: &[String],
) -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir: PathBuf = match dir {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(format!("nv-backups/{stamp}")),
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating backup dir {}", dir.display()))?;

    let mut entries = Vec::new();
    for base in &ctx.profile.boot.partitions {
        // Default: only partitions whose name carries NV.  Firmware images
        // (nr_modem, nr_phy) are not NV; naming them explicitly overrides.
        let wanted = if only.is_empty() {
            is_nv_partition(base)
        } else {
            only.iter().any(|n| n == base)
        };
        if !wanted {
            continue;
        }
        let Some(path) = first_existing(base) else {
            out.push(format!("{base:<16} (absent, skipped)"));
            continue;
        };
        let file_name = format!(
            "{}.img",
            path.file_name().and_then(|n| n.to_str()).unwrap_or(base)
        );
        let image = dir.join(&file_name);
        let mut src = std::fs::File::open(&path)
            .with_context(|| format!("opening {} for backup", path.display()))?;
        let mut dst = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&image)
            .with_context(|| format!("creating {}", image.display()))?;
        let n = copy_raw(&mut src, &mut dst)
            .with_context(|| format!("copying {} to {}", path.display(), image.display()))?;
        let _ = dst.sync_all();
        drop(dst);
        let digest = sha256_of(&image, None)?.0;
        out.push(format!(
            "{base:<16} -> {} ({n} bytes, sha256 {digest})",
            image.display()
        ));
        entries.push(serde_json::json!({
            "partition": base,
            "file": file_name,
            "sha256": digest,
            "bytes": n,
        }));
    }
    if entries.is_empty() {
        bail!("nothing backed up: no partition matched the request");
    }
    let manifest = dir.join("manifest.json");
    std::fs::write(&manifest, serde_json::to_string_pretty(&entries)?)?;
    out.push(format!("manifest: {}", manifest.display()));
    ctx.event("nv-backup", format!("{} partitions", entries.len()));
    Ok(dir)
}

/// The sha256 a manifest.json next to `image` vouches for, if one does.
fn manifest_sha_for(image: &Path) -> Result<Option<String>> {
    let manifest = image.parent().unwrap_or(Path::new(".")).join("manifest.json");
    if !manifest.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let entries: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", manifest.display()))?;
    let wanted = image.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    for e in entries.as_array().into_iter().flatten() {
        if e.get("file").and_then(|v| v.as_str()) == Some(wanted) {
            return Ok(e.get("sha256").and_then(|v| v.as_str()).map(|s| s.to_string()));
        }
    }
    Ok(None)
}

/// Every check a candidate image must pass before one byte reaches NV:
/// exact size, and a sha256 somebody vouched for (manifest or --sha256).
fn vet_image(image: &Path, required_sha: Option<&str>, target_size: u64) -> Result<String> {
    let meta = std::fs::metadata(image).with_context(|| format!("stating {}", image.display()))?;
    if meta.len() != target_size {
        bail!(
            "{} is {} bytes but the partition is {target_size}: refusing a partial write",
            image.display(),
            meta.len()
        );
    }
    let digest = sha256_of(image, None)?.0;
    if let Some(want) = required_sha {
        if !digest.eq_ignore_ascii_case(want) {
            bail!(
                "{} hashes to {digest}, not the vouched {want}: refusing",
                image.display()
            );
        }
    }
    Ok(digest)
}

fn sha256_of(path: &Path, limit: Option<u64>) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total: u64 = 0;
    loop {
        if let Some(lim) = limit {
            if total >= lim {
                break;
            }
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut take = n;
        if let Some(lim) = limit {
            take = take.min((lim - total) as usize);
        }
        hasher.update(&buf[..take]);
        total += take as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

pub struct Nv;

impl Capability for Nv {
    fn name(&self) -> &'static str {
        "nv"
    }

    fn summary(&self) -> &'static str {
        "NV view and guarded restore: list | hash <node> [--head N] | backup [--dir D] [nodes...] | restore <node> <img> --yes"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("list");
        let mut out = Vec::new();

        match action {
            "list" => {
                if ctx.profile.boot.partitions.is_empty() {
                    out.push("this profile lists no boot.partitions".to_string());
                }
                for base in &ctx.profile.boot.partitions {
                    match first_existing(base) {
                        Some(p) => {
                            let info = probes::node_info(p.to_str().unwrap_or_default());
                            out.push(format!("{base:<16} {} {}", p.display(), info.describe()));
                        }
                        None => out.push(format!("{base:<16} (absent)")),
                    }
                }
                out.push(
                    "writes go through guarded backup/restore only; no in-place editing (contracts section 8)"
                        .to_string(),
                );
                Ok(Outcome::pass(out))
            }
            "hash" => {
                let Some(node) = pos.get(1) else {
                    bail!("nv hash needs a partition name (see `nv list`)");
                };
                let limit = match flag_value(args, "--head") {
                    Some(v) => Some(v.parse::<u64>().map_err(|_| anyhow::anyhow!("--head expects bytes"))?),
                    None => None,
                };
                let Some(path) = first_existing(node) else {
                    bail!("no NV node for {node:?} under /dev/block/by-name");
                };
                let (digest, n) = sha256_of(&path, limit)?;
                out.push(format!("{node} {}", path.display()));
                out.push(format!("sha256 {digest}"));
                out.push(format!("bytes hashed {n}"));
                ctx.event("nv-hash", format!("{node} {digest}"));
                Ok(Outcome::pass(out))
            }
            "backup" => {
                let only: Vec<String> = pos.get(1..).unwrap_or_default().to_vec();
                let dir = backup_nv_nodes(ctx, &mut out, flag_value(args, "--dir"), &only)?;
                out.push(format!("backup complete: {}", dir.display()));
                Ok(Outcome::pass(out))
            }
            "restore" => {
                if ctx.profile.nv.readonly {
                    bail!(
                        "profile {:?} declares [nv].readonly = true; NV writes are refused. \
                         Flip it only as an explicit owner decision.",
                        ctx.profile.name
                    );
                }
                let (Some(node), Some(image_arg)) = (pos.get(1), pos.get(2)) else {
                    bail!("nv restore needs a node and an image: restore <node> <image> --yes");
                };
                if !has_flag(args, "--yes") {
                    bail!("nv restore refuses without --yes: it overwrites modem NV partitions");
                }
                let image = PathBuf::from(image_arg);
                let Some(representative) = first_existing(node) else {
                    bail!("no NV node for {node:?} under /dev/block/by-name");
                };
                let target_size = std::fs::metadata(&representative)
                    .with_context(|| format!("stating {}", representative.display()))?
                    .len();
                let vouched = match flag_value(args, "--sha256") {
                    Some(s) => Some(s.to_string()),
                    None => manifest_sha_for(&image)?,
                };
                let Some(vouched) = vouched else {
                    bail!(
                        "nobody vouches for {}: pass --sha256 <digest> or keep the \
                         manifest.json the backup wrote next to the image; \
                         refusing an unvouched raw write",
                        image.display()
                    );
                };
                let digest = vet_image(&image, Some(&vouched), target_size)?;

                let targets: Vec<PathBuf> = match flag_value(args, "--slot") {
                    Some(s) => {
                        let suffix = format!("_{}", s.trim_start_matches('_'));
                        nv_candidates(node)
                            .into_iter()
                            .filter(|p| p.exists())
                            .filter(|p| p.to_string_lossy().ends_with(&suffix))
                            .collect()
                    }
                    None => nv_candidates(node).into_iter().filter(|p| p.exists()).collect(),
                };
                if targets.is_empty() {
                    bail!("no existing slot of {node} matched the request");
                }
                out.push(format!(
                    "restore {} -> {} slot(s), {} bytes each, sha256 {digest}",
                    image.display(),
                    targets.len(),
                    target_size
                ));
                out.push(
                    "the CP's NV service must not be running while raw NV is written; \
                     stop it first or it may rewrite the block under you"
                        .into(),
                );
                for t in &targets {
                    let before = sha256_of(t, None)?.0;
                    let mut src = std::fs::File::open(&image)?;
                    let mut dst = std::fs::OpenOptions::new()
                        .write(true)
                        .open(t)
                        .with_context(|| format!("opening {} for write", t.display()))?;
                    copy_raw(&mut src, &mut dst)?;
                    dst.sync_all()
                        .with_context(|| format!("syncing {}", t.display()))?;
                    let after = sha256_of(t, None)?.0;
                    if after != digest {
                        bail!(
                            "{} reads back {after}, not {digest} (it was {before}); the \
                             target may be corrupted -- restore from nv-backups NOW",
                            t.display()
                        );
                    }
                    out.push(format!("{}: {before} -> {after}", t.display()));
                }
                ctx.event("nv-restore", format!("{node} {}", image.display()));
                Ok(Outcome::pass(out))
            }
            other => bail!(
                "nv: unknown action {other:?} (list | hash <node> [--head N] | backup [--dir D] [nodes...] | restore <node> <img> --yes)"
            ),
        }
    }
}

pub struct Diag;

impl Capability for Diag {
    fn name(&self) -> &'static str {
        "diag"
    }

    fn summary(&self) -> &'static str {
        "diagnostics: channels | spools | mailbox | asserts | urc | all"
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let pos = positionals(args);
        let what = pos.first().map(|s| s.as_str()).unwrap_or("all");
        let mut out = Vec::new();

        if matches!(what, "spools" | "all") {
            let mut nodes: Vec<(&str, String)> = Vec::new();
            if let Some(v) = &ctx.profile.channels.log {
                nodes.push(("log", v.clone()));
            }
            if let Some(v) = &ctx.profile.channels.dump {
                nodes.push(("dump", v.clone()));
            }
            if let Some(v) = &ctx.profile.channels.stime {
                nodes.push(("stime", v.clone()));
            }
            for (k, v) in &ctx.profile.channels.spool {
                nodes.push((k.as_str(), v.clone()));
            }
            for (name, path) in nodes {
                let info = probes::node_info(&path);
                out.push(format!("{name:<8} {path:<24} {}", info.describe()));
            }
        }

        if matches!(what, "mailbox" | "all") {
            match probes::mailbox_irq_count(&ctx.profile.mailbox.irq_match) {
                Some(v) => out.push(format!("mailbox irq total {v}")),
                None => out.push(format!(
                    "mailbox irq: no '{}' lines in /proc/interrupts",
                    ctx.profile.mailbox.irq_match
                )),
            }
        }

        if matches!(what, "asserts" | "all") {
            let pattern = &ctx.profile.telemetry.assert_pattern;
            match probes::kernel_log_matches(pattern) {
                Some(v) => out.push(format!("kernel log matches \"{pattern}\": {v}")),
                None => out.push("kernel log: not readable here".to_string()),
            }
        }

        if matches!(what, "channels" | "all") {
            out.push(format!(
                "cmd {} urc {}",
                ctx.profile.channels.cmd,
                ctx.profile.channels.urc.clone().unwrap_or_else(|| "(none)".into())
            ));
            for name in ["cmd", "urc"] {
                if let Some(ch) = ctx.channel(name) {
                    out.push(format!(
                        "{name}: path {} open {} healthy {}",
                        ch.path.display(),
                        ch.is_open(),
                        ch.healthy()
                    ));
                }
            }
            if let Some(s) = ctx.session() {
                let m = s.metrics();
                out.push(format!(
                    "AT commands {} ok {} errors {} timeouts {} urc {}",
                    m.commands, m.ok, m.errors, m.timeouts, m.urc_lines
                ));
            } else {
                out.push("AT channels not opened by this run".to_string());
            }
        }

        if matches!(what, "urc" | "all") {
            match ctx.session() {
                Some(s) => {
                    let tail = s.urc_tail(20);
                    if tail.is_empty() {
                        out.push("URC tail: (empty)".to_string());
                    } else {
                        out.push(format!("URC tail ({} lines):", tail.len()));
                        out.extend(tail.into_iter().map(|l| format!("  {l}")));
                    }
                }
                None => out.push("URC tail: session not open".to_string()),
            }
        }

        if out.is_empty() {
            bail!("diag: unknown topic {what:?} (channels|spools|mailbox|asserts|urc|all)");
        }
        Ok(Outcome::pass(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_a_known_digest() {
        let dir = std::env::temp_dir().join("unisoc-cpd-nv-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("blob");
        std::fs::write(&f, b"abc").unwrap();
        let (d, n) = sha256_of(&f, None).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            d,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_head_limit_is_honoured() {
        let dir = std::env::temp_dir().join("unisoc-cpd-nv-test2");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("blob");
        std::fs::write(&f, b"abcdef").unwrap();
        let (d, n) = sha256_of(&f, Some(3)).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            d,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn vet_image_enforces_size_and_vouched_hash() {
        let dir = std::env::temp_dir().join("unisoc-cpd-nv-vet");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("blob");
        std::fs::write(&f, b"0123456789").unwrap();
        let (d, _) = sha256_of(&f, None).unwrap();

        assert_eq!(vet_image(&f, Some(&d), 10).unwrap(), d);
        assert_eq!(vet_image(&f, None, 10).unwrap(), d);
        // Wrong size: refused before any hash question is asked.
        assert!(vet_image(&f, Some(&d), 11).is_err());
        // Wrong vouch: refused.
        assert!(vet_image(&f, Some("deadbeef"), 10).is_err());
    }

    #[test]
    fn manifest_sha_is_found_by_file_name() {
        let dir = std::env::temp_dir().join("unisoc-cpd-nv-manifest");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("part_a.img");
        std::fs::write(&f, b"xyz").unwrap();
        let (d, _) = sha256_of(&f, None).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::json!([{ "partition": "part", "file": "part_a.img", "sha256": d, "bytes": 3 }])
                .to_string(),
        )
        .unwrap();
        assert_eq!(manifest_sha_for(&f).unwrap().as_deref(), Some(d.as_str()));
        // An image with no manifest next to it: nobody vouches.
        let lonely = dir.join("lonely.img");
        std::fs::write(&lonely, b"xyz").unwrap();
        assert_eq!(manifest_sha_for(&lonely).unwrap(), None);
    }

    #[test]
    fn nv_partition_heuristic_names_nv_only() {
        assert!(is_nv_partition("nr_fixnv1"));
        assert!(is_nv_partition("nr_runtimenv2"));
        assert!(is_nv_partition("deltanv"));
        assert!(!is_nv_partition("nr_modem"));
        assert!(!is_nv_partition("nr_phy"));
    }
}
