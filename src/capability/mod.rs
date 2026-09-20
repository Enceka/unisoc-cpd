//! Capabilities: the things the daemon knows how to do with the CP.
//!
//! Each one keeps two modes.  `native` drives the modem through our own AT
//! layer; `vendor` still lets the vendor daemon do the work and only records
//! what it produced.  The same acceptance test has to pass in both before the
//! vendor side can be switched off, so the vendor path lives here too.
//!
//! `serve` is the odd one out and is here on purpose: it is not something the
//! daemon does *to* the modem, it is the daemon holding the channel so that
//! every other verb can be asked for by name (`core/capability/serve.rs`).

pub mod control;
pub mod data;
pub mod imei;
pub mod link;
pub mod radio;
pub mod serve;
pub mod side;

use crate::context::Context;
use anyhow::{bail, Result};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
}

#[derive(Debug)]
pub struct Outcome {
    pub status: Status,
    pub output: Vec<String>,
}

impl Outcome {
    pub fn pass(output: Vec<String>) -> Self {
        Self {
            status: Status::Pass,
            output,
        }
    }

    pub fn fail(output: Vec<String>) -> Self {
        Self {
            status: Status::Fail,
            output,
        }
    }

    pub fn passed(&self) -> bool {
        self.status == Status::Pass
    }

    pub fn push(&mut self, line: impl Into<String>) {
        self.output.push(line.into());
    }
}

pub trait Capability {
    fn name(&self) -> &'static str;
    fn summary(&self) -> &'static str;
    /// Native-only capabilities have no vendor counterpart to compare against.
    fn native_only(&self) -> bool {
        false
    }
    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome>;
}

pub fn find(name: &str) -> Option<Box<dyn Capability>> {
    let all: Vec<Box<dyn Capability>> = vec![
        Box::new(link::Link),
        Box::new(control::Sim),
        Box::new(control::Cfun),
        Box::new(control::Register),
        Box::new(control::Signal),
        Box::new(control::Operator),
        Box::new(radio::Band),
        Box::new(radio::Nr5g),
        Box::new(radio::Ims),
        Box::new(control::Sms),
        Box::new(control::Ussd),
        Box::new(control::Call),
        Box::new(data::Data),
        Box::new(imei::Imei),
        Box::new(side::Nv),
        Box::new(side::Diag),
        Box::new(serve::Serve),
    ];
    all.into_iter().find(|c| c.name() == name)
}

pub fn catalogue() -> Vec<(&'static str, &'static str, bool)> {
    let names = [
        "link", "sim", "cfun", "register", "signal", "operator", "band", "nr", "ims", "sms",
        "ussd", "call", "data", "imei", "nv", "diag", "serve",
    ];
    names
        .iter()
        .filter_map(|n| find(n))
        .map(|c| (c.name(), c.summary(), c.native_only()))
        .collect()
}

/// `--flag value` from the raw argument list.
pub fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().map(|s| s.as_str());
        }
        if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
            return Some(rest);
        }
    }
    None
}

pub fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Arguments that are neither `--flags` nor the value of one.
pub fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a.starts_with("--") {
            skip_next = !a.contains('=');
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// The vendor path: run the profile's command for this capability and record it.
pub fn run_vendor(ctx: &Context, capability: &str, args: &[String]) -> Result<Outcome> {
    let Some(cmd) = ctx.profile.vendor_command(capability) else {
        bail!(
            "profile {} has no vendor path for '{capability}'; \
             this capability can only be measured in --mode native",
            ctx.profile.name
        );
    };
    let full = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, args.join(" "))
    };
    ctx.verbose(format!("vendor: {full}"));
    let out = Command::new("sh").arg("-c").arg(&full).output()?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        lines.push(format!("!stderr: {}", stderr.trim()));
    }
    let status = if out.status.success() {
        Status::Pass
    } else {
        Status::Fail
    };
    Ok(Outcome {
        status,
        output: lines,
    })
}

/// Send one AT command and hand the reply back, recording it for the summary.
pub fn at(ctx: &Context, cmd: &str, timeout_s: f64) -> Result<crate::at::Reply> {
    let session = ctx
        .session()
        .ok_or_else(|| anyhow::anyhow!("AT channel is not open"))?;
    Ok(session.command(cmd, std::time::Duration::from_secs_f64(timeout_s), &[], 0))
}

pub fn at_with(
    ctx: &Context,
    cmd: &str,
    timeout_s: f64,
    expect: &[String],
    retries: u32,
) -> Result<crate::at::Reply> {
    let session = ctx
        .session()
        .ok_or_else(|| anyhow::anyhow!("AT channel is not open"))?;
    Ok(session.command(
        cmd,
        std::time::Duration::from_secs_f64(timeout_s),
        expect,
        retries,
    ))
}

/// Append a reply to the output, prefixed with the command it answers.
pub fn emit(out: &mut Vec<String>, cmd: &str, reply: &crate::at::Reply) {
    out.push(format!("> {cmd}"));
    for line in &reply.lines {
        out.push(format!("  {line}"));
    }
    for line in &reply.urcs {
        out.push(format!("  (urc) {line}"));
    }
    out.push(format!("  {}", reply.final_code.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_value_reads_both_spellings() {
        let args: Vec<String> = ["--seconds", "5"].iter().map(|s| s.to_string()).collect();
        assert_eq!(flag_value(&args, "--seconds"), Some("5"));
        let args: Vec<String> = ["--seconds=7"].iter().map(|s| s.to_string()).collect();
        assert_eq!(flag_value(&args, "--seconds"), Some("7"));
        let args: Vec<String> = ["--seconds"].iter().map(|s| s.to_string()).collect();
        assert_eq!(flag_value(&args, "--seconds"), None);
    }

    #[test]
    fn positionals_skips_flags_and_their_values() {
        let args: Vec<String> = ["+8613800138000", "--timeout", "20"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(positionals(&args), vec!["+8613800138000".to_string()]);
    }

    #[test]
    fn the_catalogue_covers_the_plan() {
        let names: Vec<&str> = catalogue().into_iter().map(|(n, _, _)| n).collect();
        for want in [
            "sim", "register", "data", "sms", "call", "ussd", "band", "cfun", "nv", "diag", "link",
        ] {
            assert!(names.contains(&want), "missing capability {want}");
        }
    }
}
