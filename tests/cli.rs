//! End-to-end runs of the real binary against a fake CP on a pty.

mod common;

use common::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_unisoc-cpd")
}

fn run_cli(profile: &Path, runs: &Path, state: &Path, mode: &str, capability: &str, extra: &[&str]) -> Output {
    Command::new(bin())
        .arg("--profile")
        .arg(profile)
        .arg("--mode")
        .arg(mode)
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
        } else if path.file_name().map(|n| n.to_string_lossy() == name).unwrap_or(false) {
            return Some(path);
        }
    }
    None
}

fn write_profile(dir: &Path, cmd: &Path, urc: Option<&Path>, extra: &str) -> PathBuf {
    let path = dir.join("pty.toml");
    std::fs::write(&path, profile_toml(cmd, urc, extra)).expect("write profile");
    path
}

#[test]
fn link_passes_and_writes_a_run_summary() {
    let (master, slave) = pty_pair();
    let modem = fake_modem(master);
    let dir = scratch("cli-link");
    let profile = write_profile(&dir, &slave, None, "");
    let runs = dir.join("runs");
    let state = dir.join("state");

    let out = run_cli(
        &profile,
        &runs,
        &state,
        "native",
        "link",
        &["--seconds", "1", "--interval", "0.2", "--timeout", "2"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("status: pass"), "stdout:\n{stdout}");
    assert!(
        modem.commands().iter().filter(|c| c.as_str() == "AT").count() >= 3,
        "the modem did not see the probe cadence: {:?}",
        modem.commands()
    );

    let summary_path = find_file(&runs, "summary.json").expect("a run summary was written");
    let records: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let last = records.as_array().unwrap().last().unwrap();
    assert_eq!(last["tool"], "unisoc-cpd");
    assert_eq!(last["capability"], "link");
    assert_eq!(last["mode"], "native");
    assert_eq!(last["status"], "pass");
    assert_eq!(last["exit_code"], 0);
    assert!(last["channels"]["cmd"]["rx_lines"].as_u64().unwrap() > 0);
    // Every reply carries an interleaved URC, so the URC counter must be up.
    assert!(
        last["urc"]["lines"].as_u64().unwrap() > 0,
        "URCs were not counted: {last:#}"
    );
    assert!(last["at"]["probes"].as_u64().unwrap() >= 3);
    assert_eq!(last["at"]["probe_failures"], 0);
}

#[test]
fn a_second_reader_on_the_at_channel_is_refused() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    let dir = scratch("cli-busy");
    let profile = write_profile(&dir, &slave, None, "");
    let state = dir.join("state");

    // We are the first reader; the daemon must refuse to become the second.
    let mine = unisoc_cpd::channel::SerialChannel::new(
        &slave,
        "cmd",
        &state,
        Duration::from_millis(10),
        true,
    );
    mine.open().expect("first reader takes the channel");

    let out = run_cli(&profile, &dir.join("runs"), &state, "native", "sim", &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "stderr:\n{stderr}");
    assert!(
        stderr.contains("another process"),
        "no explanation for the refusal: {stderr}"
    );
    mine.close();
}

#[test]
fn sim_reports_the_pin_state() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    let dir = scratch("cli-sim");
    let profile = write_profile(&dir, &slave, None, "");

    let out = run_cli(&profile, &dir.join("runs"), &dir.join("state"), "native", "sim", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("+CPIN: READY"), "stdout:\n{stdout}");
    assert!(stdout.contains("status: pass"));
}

#[test]
fn signal_decodes_cesq() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    let dir = scratch("cli-signal");
    let profile = write_profile(&dir, &slave, None, "");

    let out = run_cli(&profile, &dir.join("runs"), &dir.join("state"), "native", "signal", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    // index 60 -> -80 dBm, index 20 -> -9.5 dB
    assert!(stdout.contains("RSRP -80 dBm"), "stdout:\n{stdout}");
}

#[test]
fn the_urc_channel_is_used_when_the_profile_names_one() {
    let (cmd_master, cmd_slave) = pty_pair();
    let (urc_master, urc_slave) = pty_pair();
    let _modem = fake_modem(cmd_master);
    let (stop, handle) = urc_emitter(urc_master, Duration::from_millis(50));
    let dir = scratch("cli-urc");
    let profile = write_profile(&dir, &cmd_slave, Some(&urc_slave), "");
    let runs = dir.join("runs");

    let out = run_cli(
        &profile,
        &runs,
        &dir.join("state"),
        "native",
        "link",
        &["--seconds", "1", "--interval", "0.3"],
    );
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = handle.join();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let summary_path = find_file(&runs, "summary.json").expect("summary");
    let records: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let last = records.as_array().unwrap().last().unwrap();
    assert!(
        last["urc"]["lines"].as_u64().unwrap() >= 2,
        "the URC channel was not drained: {last:#}"
    );
    assert!(last["channels"]["urc"]["rx_lines"].as_u64().unwrap() >= 2);
}

#[test]
fn vendor_mode_runs_the_profiles_command_and_records_it() {
    let dir = scratch("cli-vendor");
    let profile = write_profile(
        &dir,
        Path::new("/dev/null"),
        None,
        "\n[vendor]\nregister = \"echo vendor-says-hello\"\n",
    );
    let out = run_cli(
        &profile,
        &dir.join("runs"),
        &dir.join("state"),
        "vendor",
        "register",
        &[],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("vendor-says-hello"), "stdout:\n{stdout}");
    assert!(stdout.contains("status: pass"));
}

#[test]
fn a_native_only_capability_refuses_vendor_mode() {
    let dir = scratch("cli-native-only");
    let profile = write_profile(&dir, Path::new("/dev/null"), None, "");
    let out = run_cli(&profile, &dir.join("runs"), &dir.join("state"), "vendor", "link", &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("native-only"), "stderr:\n{stderr}");
}

#[test]
fn the_rig_spelling_of_mode_is_accepted() {
    let out = Command::new(bin())
        .args(["mode", "native", "capabilities"])
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("capability"));
}

#[test]
fn capabilities_and_profiles_list_from_the_tree() {
    let out = Command::new(bin()).arg("capabilities").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    for want in ["link", "sim", "register", "data", "sms", "call", "ussd", "band", "cfu", "imei", "nv", "diag"] {
        assert!(stdout.contains(want), "capabilities output lacks {want}:\n{stdout}");
    }

    // This also proves both shipped profiles parse.
    let out = Command::new(bin()).arg("profiles").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("e5"), "stdout:\n{stdout}");
    assert!(stdout.contains("mu300"), "stdout:\n{stdout}");
    assert!(!stdout.contains("INVALID"), "stdout:\n{stdout}");
}
