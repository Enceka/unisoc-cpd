//! Command line: `unisoc-cpd [--mode vendor|native] <capability> [args]`.
//!
//! `mode vendor|native <capability>` is accepted as a spelling of the same
//! thing, because that is how the test rig in the plan invokes it.

use crate::capability::{self, Status};
use crate::context::{Context, Mode};
use crate::profile;
use anyhow::{bail, Result};
use clap::{ArgAction, Parser};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "unisoc-cpd",
    version,
    about = "Device-independent control daemon for the Unisoc CP (baseband)",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Profile name (see `unisoc-cpd profiles`) or a path to a .toml
    #[arg(long, global = true)]
    pub profile: Option<String>,

    /// Directory holding the profiles
    #[arg(long, global = true)]
    pub profiles_dir: Option<PathBuf>,

    /// vendor: the vendor daemon still does the work, we only record it
    /// native: we own the channel and drive the modem
    #[arg(long, short = 'm', global = true, default_value = "native")]
    pub mode: String,

    /// Where run summaries are written (default: ./runs)
    #[arg(long, global = true)]
    pub runs_dir: Option<PathBuf>,

    /// Where the channel ownership locks live (default: /run/unisoc-cpd)
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,

    /// Ask the resident owner (`serve`) at this socket instead of opening the
    /// channel here; `serve` itself listens on it (default: <state-dir>/cmd.sock)
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Do not write a run summary
    #[arg(long, global = true)]
    pub no_telemetry: bool,

    #[arg(long, short = 'v', global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// The capability to run (see `unisoc-cpd capabilities`)
    pub capability: String,

    /// Arguments for the capability
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

fn print_profiles(dir: Option<&std::path::Path>) -> Result<()> {
    let dir = profile::profiles_dir(dir);
    if !dir.is_dir() {
        bail!("no profile directory at {}", dir.display());
    }
    let mut rows: Vec<(String, String, bool, String)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match profile::load_file(&path) {
            Ok(p) => rows.push((
                p.name.clone(),
                p.generation.clone(),
                p.verified,
                p.channels.cmd.clone(),
            )),
            Err(e) => rows.push((
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                format!("INVALID: {e}"),
                false,
                String::new(),
            )),
        }
    }
    rows.sort();
    println!(
        "{:<10} {:<18} {:<9} {}",
        "name", "generation", "verified", "cmd channel"
    );
    for (name, gen, verified, cmd) in rows {
        println!(
            "{name:<10} {gen:<18} {:<9} {cmd}",
            if verified { "yes" } else { "no" }
        );
    }
    Ok(())
}

fn print_capabilities() {
    println!("{:<12} {:<8} {}", "capability", "modes", "what it does");
    for (name, summary, native_only) in capability::catalogue() {
        println!(
            "{name:<12} {:<8} {summary}",
            if native_only { "native" } else { "both" }
        );
    }
}

pub fn run(cli: Cli) -> Result<i32> {
    match cli.capability.as_str() {
        "profiles" => return print_profiles(cli.profiles_dir.as_deref()).map(|_| 0),
        "capabilities" => {
            print_capabilities();
            return Ok(0);
        }
        "profile-check" => {
            let ok = crate::profile_check::run(cli.profiles_dir.as_deref(), cli.verbose > 0)?;
            return Ok(if ok { 0 } else { 1 });
        }
        _ => {}
    }

    let mode = Mode::parse(&cli.mode)?;

    // `--socket` means "the daemon already owns the channel, ask it" — which is
    // the whole point of G2, and the only way a capability can run while
    // `serve` holds the port.  `serve` is the exception: for it, `--socket` is
    // where to listen, so it goes down the direct path.
    if let Some(socket) = &cli.socket {
        // `web` is the other exception: like `serve`, it is a front-end that
        // must run here, not a request to forward -- it drives the daemon,
        // never the channels.
        if !matches!(cli.capability.as_str(), "serve" | "web") {
            if mode == Mode::Vendor {
                bail!("--socket asks our own daemon; a vendor run has no socket to ask");
            }
            return crate::capability::serve::client(socket, &cli.capability, &cli.args);
        }
    }

    let profile = profile::resolve(cli.profile.as_deref(), cli.profiles_dir.as_deref())?;
    let runs_dir = cli
        .runs_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("runs"));
    let state_dir = cli
        .state_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/run/unisoc-cpd"));
    let argv: Vec<String> = std::env::args().collect();

    let capability_name = cli.capability.clone();
    let mut ctx = Context::new(
        profile,
        mode,
        runs_dir.clone(),
        state_dir,
        cli.verbose > 0,
        argv,
    );
    ctx.telemetry = !cli.no_telemetry;
    ctx.verbose(format!(
        "profile {} ({}) from {}",
        ctx.profile.name,
        ctx.profile.generation,
        ctx.profile
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(built-in)".into())
    ));

    let native_only = capability::find(&capability_name)
        .map(|c| c.native_only())
        .unwrap_or(false);

    let outcome = if mode == Mode::Vendor {
        if native_only {
            bail!("'{capability_name}' has no vendor path: it is native-only");
        }
        capability::run_vendor(&ctx, &capability_name, &cli.args)?
    } else {
        let Some(cap) = capability::find(&capability_name) else {
            bail!("unknown capability '{capability_name}'; try `unisoc-cpd capabilities`");
        };
        let mut cap_args = cli.args.clone();
        if capability_name == "web" {
            if let Some(socket) = &cli.socket {
                if !cap_args.iter().any(|a| a == "--socket" || a.starts_with("--socket=")) {
                    cap_args.push("--socket".into());
                    cap_args.push(socket.display().to_string());
                }
            }
        }
        cap.run(&mut ctx, &cap_args)?
    };

    let exit_code = match outcome.status {
        Status::Pass => 0,
        Status::Fail => 1,
    };
    let status = match outcome.status {
        Status::Pass => "pass",
        Status::Fail => "fail",
    };

    for line in &outcome.output {
        println!("{line}");
    }
    println!("status: {status}");

    if !cli.no_telemetry {
        let summary = ctx.finish(&capability_name, status, exit_code);
        match summary.write(&runs_dir) {
            Ok(path) => ctx.verbose(format!("run summary {}", path.display())),
            Err(e) => eprintln!("unisoc-cpd: could not write the run summary: {e:#}"),
        }
    }

    Ok(exit_code)
}
