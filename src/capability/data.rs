//! `data` — PDP context bring-up and the bearer interface.
//!
//! The AT half is the sequence captured from the vendor side; the Linux half
//! (bring the interface up, add the address the modem handed us) is generic.
//! Anything that is genuinely platform-shaped — the NAT table, a LED, a QMI
//! bridge — is a `vendor`-mode command or a profile hook, never a `#ifdef`.

use super::{emit, positionals, Capability, Outcome};
use crate::context::Context;
use anyhow::{bail, Result};
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

impl Capability for Data {
    fn name(&self) -> &'static str {
        "data"
    }

    fn summary(&self) -> &'static str {
        "PDP context and bearer interface: up [apn] | down | status | apn"
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
                for cmd in [
                    "AT+CEREG?".to_string(),
                    "AT+CGATT?".to_string(),
                    "AT+CGACT?".to_string(),
                    format!("AT+CGCONTRDP={cid}"),
                ] {
                    let r = session.command(&cmd, Duration::from_secs(8), &[], 0);
                    emit(&mut out, &cmd, &r);
                }
                if let Some(iface) = &iface {
                    let (_, addr) = run("ip", &["-br", "addr", "show", iface]);
                    out.push(format!("interface {iface}: {addr}"));
                    let (_, route) = run("ip", &["route", "show", "dev", iface]);
                    out.push(format!("routes: {}", route.replace('\n', " | ")));
                }
                Ok(Outcome::pass(out))
            }
            "apn" => {
                let mut out = Vec::new();
                match &ctx.profile.data.apn_source {
                    Some(p) => match apn_from_source(p) {
                        Some(apn) => out.push(format!("APN from {p}: {apn}")),
                        None => out.push(format!("no APN= line in {p}")),
                    },
                    None => out.push("this profile has no apn_source".to_string()),
                }
                Ok(Outcome::pass(out))
            }
            "up" => {
                let apn = pos.get(1).cloned().or_else(|| {
                    ctx.profile
                        .data
                        .apn_source
                        .as_deref()
                        .and_then(apn_from_source)
                });
                let session = ctx.at()?;
                let mut out = Vec::new();

                if let Some(apn) = &apn {
                    let cmd = format!("AT+CGDCONT={cid},\"IPV4V6\",\"{apn}\"");
                    let r = session.command(&cmd, Duration::from_secs(15), &[], 0);
                    emit(&mut out, &cmd, &r);
                } else {
                    out.push("! no APN given and none in the profile's apn_source".into());
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
                        // the sipa driver hands up wrong hardware checksums
                        let _ = run("ethtool", &["-K", iface, "rx", "off"]);
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
            other => bail!("data: unknown action {other:?} (up [apn]|down|status|apn)"),
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
