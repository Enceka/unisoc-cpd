//! G2 end-to-end: the resident owner, on the same fake CP as the other runs.
//!
//! What these tests are about is not the AT that goes out (the capability tests
//! cover that) but **who owns the channel**: that `serve` takes it once and
//! keeps it, that a direct run is then refused loudly instead of starving, that
//! the capabilities can still be asked for over the socket, and that the
//! unsolicited stream is decoded while it is held.

mod common;

use common::*;
use serde_json::json;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_unisoc-cpd")
}

fn write_profile(dir: &Path, cmd: &Path, urc: Option<&Path>, extra: &str) -> PathBuf {
    let path = dir.join("pty.toml");
    std::fs::write(&path, profile_toml(cmd, urc, extra)).expect("write profile");
    path
}

/// One request, one connection, one line of JSON back.
fn try_ask(socket: &Path, request: &serde_json::Value) -> Option<serde_json::Value> {
    let stream = UnixStream::connect(socket).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut writer = stream.try_clone().ok()?;
    writeln!(writer, "{request}").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

fn ask(socket: &Path, request: &serde_json::Value) -> serde_json::Value {
    try_ask(socket, request)
        .unwrap_or_else(|| panic!("no answer from the daemon at {}", socket.display()))
}

struct Daemon {
    child: Child,
}

impl Daemon {
    /// Start `serve` and wait until it actually answers, not merely until the
    /// socket file exists.
    fn start(socket: &Path, profile: &Path, runs: &Path, state: &Path, extra: &[&str]) -> Self {
        let mut command = Command::new(bin());
        command
            .arg("--profile")
            .arg(profile)
            .arg("--mode")
            .arg("native")
            .arg("--runs-dir")
            .arg(runs)
            .arg("--state-dir")
            .arg(state)
            .arg("serve")
            .arg("--socket")
            .arg(socket)
            .arg("--seconds")
            .arg("120")
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn the daemon");

        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if try_ask(socket, &json!({"action": "state"})).is_some() {
                return Daemon { child };
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!(
                    "the daemon exited ({status}) instead of serving: {}",
                    collect(&mut child)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        panic!(
            "no daemon answered on {} within 15 s: {}",
            socket.display(),
            collect(&mut child)
        );
    }

    /// Wait until the daemon reports `predicate` about its own state.
    fn wait_for_state(
        &self,
        socket: &Path,
        what: &str,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = ask(socket, &json!({"action": "state"}))["state"].clone();
            if predicate(&state) {
                return state;
            }
            if Instant::now() >= deadline {
                panic!("{what} did not happen within 10 s: {state:#}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn collect(child: &mut Child) -> String {
    let mut out = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut out);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut out);
    }
    out
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_cli(
    profile: &Path,
    runs: &Path,
    state: &Path,
    capability: &str,
    extra: &[&str],
) -> std::process::Output {
    Command::new(bin())
        .arg("--profile")
        .arg(profile)
        .arg("--mode")
        .arg("native")
        .arg("--runs-dir")
        .arg(runs)
        .arg("--state-dir")
        .arg(state)
        .arg(capability)
        .args(extra)
        .output()
        .expect("run the binary")
}

fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path
            .file_name()
            .map(|n| n.to_string_lossy() == name)
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

fn summary_records(runs: &Path) -> Vec<serde_json::Value> {
    let path = find_file(runs, "summary.json").expect("a run summary was written");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The heart of G2: one owner for the whole boot, the capabilities still
/// available, and a second reader refused instead of silently starving.
#[test]
fn serve_owns_the_channel_and_answers_for_the_capabilities() {
    let (master, slave) = pty_pair();
    let modem = fake_modem(master);
    let dir = scratch("serve-owner");
    let profile = write_profile(&dir, &slave, None, "idle_probe_seconds = 0.5\n");
    let runs = dir.join("runs");
    let state_dir = dir.join("state");
    let socket = state_dir.join("cmd.sock");

    let daemon = Daemon::start(&socket, &profile, &runs, &state_dir, &[]);

    // 1. A capability asked for over the socket runs against the owned channel.
    let answer = ask(&socket, &json!({"capability": "sim"}));
    assert_eq!(answer["ok"], json!(true), "{answer:#}");
    assert_eq!(answer["status"], "pass", "{answer:#}");
    assert_eq!(answer["exit_code"], 0);
    let output = answer["output"].to_string();
    assert!(output.contains("+CPIN: READY"), "{answer:#}");
    assert!(modem.wait_for_command("AT+CPIN?", Duration::from_secs(2)));

    // The run happened in the daemon, so that is where its summary is.
    let records = summary_records(&runs);
    let last = records.last().unwrap();
    assert_eq!(last["capability"], "sim");
    assert_eq!(last["mode"], "native");
    assert_eq!(last["status"], "pass");

    // 2. The channel was opened once and never reopened -- the whole promise of
    //    a resident owner, and the `channels.cmd.opens` number A1 reads.
    let state = ask(&socket, &json!({"action": "state"}))["state"].clone();
    assert_eq!(state["channels"]["cmd"]["opens"], 1, "{state:#}");
    assert_eq!(state["channels"]["cmd"]["reopens"], 0, "{state:#}");
    // The readiness poke before it was one request, `sim` another.
    assert!(state["requests"].as_u64().unwrap() >= 2, "{state:#}");
    assert_eq!(state["pid"].as_u64().unwrap() as u32, daemon.child.id());

    // 3. A second reader is refused loudly: exit 3, and it says why.  This is
    //    the plan's only red line, tested from the outside.
    let direct = run_cli(&profile, &runs, &state_dir, "sim", &[]);
    let stderr = String::from_utf8_lossy(&direct.stderr);
    assert_eq!(direct.status.code(), Some(3), "stderr:\n{stderr}");
    assert!(stderr.contains("another process"), "stderr:\n{stderr}");

    // 4. The daemon is still the owner after that refusal, and its idle probe
    //    is what keeps saying so: a probe is a real command, counted apart from
    //    the commands a request sent.
    let state = daemon.wait_for_state(&socket, "an idle probe", |s| {
        s["probes"].as_u64().unwrap_or(0) >= 1
    });
    assert_eq!(state["probe_failures"], 0, "{state:#}");
    assert_eq!(state["channels"]["cmd"]["opens"], 1, "{state:#}");
    assert!(state["last_ok_age_s"].as_f64().is_some(), "{state:#}");
    let at = &state["at"];
    assert!(at["commands"].as_u64().unwrap() >= 2, "{state:#}");
    assert!(at["probes"].as_u64().unwrap() >= 1, "{state:#}");
    assert_eq!(at["probe_failures"], 0, "{state:#}");
}

/// `--socket` is how a capability runs while the daemon owns the channel, so
/// the CLI stays the interface: the same command, asked instead of taken.
#[test]
fn the_cli_asks_the_daemon_when_it_is_given_a_socket() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    let dir = scratch("serve-client");
    let profile = write_profile(&dir, &slave, None, "");
    let runs = dir.join("runs");
    let state_dir = dir.join("state");
    let socket = state_dir.join("cmd.sock");

    // Nothing is serving yet: that is an environment problem, not a failure.
    let idle = run_cli(
        &profile,
        &runs,
        &state_dir,
        "sim",
        &["--socket", socket.to_str().unwrap()],
    );
    let stderr = String::from_utf8_lossy(&idle.stderr);
    assert_eq!(idle.status.code(), Some(3), "stderr:\n{stderr}");
    assert!(
        stderr.contains("cannot reach the daemon"),
        "stderr:\n{stderr}"
    );

    let _daemon = Daemon::start(&socket, &profile, &runs, &state_dir, &[]);

    let out = run_cli(
        &profile,
        &runs,
        &state_dir,
        "sim",
        &["--socket", socket.to_str().unwrap()],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("+CPIN: READY"), "stdout:\n{stdout}");
    assert!(stdout.contains("status: pass"), "stdout:\n{stdout}");

    // `state` and `urc` are questions for the daemon itself, not capabilities.
    let out = run_cli(
        &profile,
        &runs,
        &state_dir,
        "state",
        &["--socket", socket.to_str().unwrap()],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let state: serde_json::Value = serde_json::from_str(stdout.trim()).expect("state is JSON");
    assert!(state["pid"].is_number(), "{stdout}");
    assert_eq!(state["urc_undecoded"], 0, "{stdout}");

    // A capability the daemon does not know is a rejected request, not a crash.
    let out = run_cli(
        &profile,
        &runs,
        &state_dir,
        "teleport",
        &["--socket", socket.to_str().unwrap()],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(stderr.contains("unknown capability"), "stderr:\n{stderr}");
}

/// The half of the control plane that is not an answer: while the daemon holds
/// the channels, the unsolicited stream is decoded into events a client can ask
/// for -- which is what MT SMS and an incoming call are built on.
#[test]
fn the_daemon_decodes_the_unsolicited_stream_it_owns() {
    let (cmd_master, cmd_slave) = pty_pair();
    let (urc_master, urc_slave) = pty_pair();
    let _modem = fake_modem(cmd_master);
    let (stop, emitter) = urc_emitter(urc_master, Duration::from_millis(30));
    let dir = scratch("serve-urc");
    let profile = write_profile(&dir, &cmd_slave, Some(&urc_slave), "");
    let runs = dir.join("runs");
    let state_dir = dir.join("state");
    let socket = state_dir.join("cmd.sock");

    let daemon = Daemon::start(&socket, &profile, &runs, &state_dir, &[]);

    // Wait for the stream to have been read, decoded and kept.
    let state = daemon.wait_for_state(&socket, "the URC stream to be decoded", |s| {
        s["urc_decoded"].as_u64().unwrap_or(0) >= 3
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = emitter.join();

    assert_eq!(state["channels"]["urc"]["opens"], 1, "{state:#}");
    assert_eq!(
        state["urc_undecoded"], 0,
        "every line was decoded: {state:#}"
    );
    assert!(
        state["urc_lines"].as_u64().unwrap() >= state["urc_decoded"].as_u64().unwrap(),
        "{state:#}"
    );

    let answer = ask(&socket, &json!({"action": "urc", "limit": 10}));
    let urcs = answer["urcs"].as_array().expect("urcs").clone();
    assert!(!urcs.is_empty(), "{answer:#}");
    // The emitter sends +CSQ, +CGEV and +SIND in turn, and all three are decoded.
    let kinds: Vec<String> = urcs
        .iter()
        .map(|e| e["urc"]["kind"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(kinds.iter().any(|k| k == "urc-signal"), "{kinds:?}");
    assert!(kinds.iter().any(|k| k == "urc-bearer"), "{kinds:?}");
    assert!(kinds.iter().any(|k| k == "urc-sim-indication"), "{kinds:?}");
    assert!(urcs.iter().all(|e| e["at"].is_string()), "{urcs:?}");

    // The same events are what a run summary records, so a URC that arrived
    // during a request is not lost to the summary's reader.
    let answer = ask(&socket, &json!({"capability": "signal"}));
    assert_eq!(answer["status"], "pass", "{answer:#}");
    let records = summary_records(&runs);
    let last = records.last().unwrap();
    assert_eq!(last["capability"], "signal");
    assert!(
        last["events"]
            .as_array()
            .map(|e| !e.is_empty())
            .unwrap_or(false),
        "the URCs seen during the run are in its summary: {last:#}"
    );
}

/// A file left behind by a killed daemon must not stop the next one, and a live
/// daemon must not be joined by a second one on the same socket.
#[test]
fn a_stale_socket_is_replaced_but_a_live_one_is_respected() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    let dir = scratch("serve-stale");
    let profile = write_profile(&dir, &slave, None, "");
    let runs = dir.join("runs");
    let state_dir = dir.join("state");
    let socket = state_dir.join("cmd.sock");

    std::fs::create_dir_all(&state_dir).unwrap();
    File::create(&socket).expect("a stale socket file from a killed daemon");

    let first = Daemon::start(&socket, &profile, &runs, &state_dir, &[]);
    assert!(ask(&socket, &json!({"action": "state"}))["state"]["pid"].is_number());

    // The same command again: same socket path, same channel.  It must refuse
    // on the socket, and it must not have become a second reader.
    let out = Command::new(bin())
        .args([
            "--profile",
            profile.to_str().unwrap(),
            "--mode",
            "native",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "serve",
            "--socket",
            socket.to_str().unwrap(),
        ])
        .output()
        .expect("run a second daemon");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "stderr:\n{stderr}");
    assert!(
        stderr.contains("another daemon is already serving"),
        "stderr:\n{stderr}"
    );
    assert!(ask(&socket, &json!({"action": "state"}))["state"]["pid"].is_number());

    drop(first);
    // Killing the daemon leaves the socket file (SIGTERM does not unwind), and
    // the next start has to get past it.
    let _second = Daemon::start(&socket, &profile, &runs, &state_dir, &[]);
}
