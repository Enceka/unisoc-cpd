//! The generation-specific radio capabilities, end to end against the fake CP.

mod common;

use common::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_unisoc-cpd")
}

fn run(dir: &Path, capability: &str, args: &[&str]) -> Output {
    Command::new(bin())
        .arg("--profile")
        .arg(dir.join("pty.toml"))
        .arg("--runs-dir")
        .arg(dir.join("runs"))
        .arg("--state-dir")
        .arg(dir.join("state"))
        .arg(capability)
        .args(args)
        .output()
        .expect("run the binary")
}

struct Rig {
    dir: PathBuf,
}

fn rig(name: &str) -> Rig {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    // Leak the fake modem for the process lifetime: these tests are short and
    // the child needs the pty to stay answered.
    std::mem::forget(_modem);
    let dir = scratch(name);
    std::fs::write(dir.join("pty.toml"), profile_toml(&slave, None, "")).unwrap();
    Rig { dir }
}

#[test]
fn band_status_decodes_the_locked_bands() {
    let r = rig("radio-band-status");
    let out = run(&r.dir, "band", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("locked bands: 1,3,41"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("locked bands: 1,78,80"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn band_lock_is_read_back_before_it_is_believed() {
    let r = rig("radio-band-lock");
    let out = run(&r.dir, "band", &["lock", "nr", "78"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(
        stdout.contains("AT+SPLBAND=2,0,0,256,0"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("locked bands: 78"), "stdout:\n{stdout}");

    let out = run(&r.dir, "band", &["lock", "lte", "3", "41"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("locked bands: 3,41"), "stdout:\n{stdout}");
}

#[test]
fn band_lock_refuses_a_request_without_bands() {
    let r = rig("radio-band-lock-empty");
    let out = run(&r.dir, "band", &["lock", "nr"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("at least one band"), "stderr:\n{stderr}");
}

#[test]
fn band_unlock_all_clears_both_rats() {
    let r = rig("radio-band-unlock");
    let out = run(&r.dir, "band", &["unlock", "all"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(
        stdout.contains("AT+SPLBAND=1,0,0,0,0,0"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("AT+SPLBAND=2,0,0,0,0"), "stdout:\n{stdout}");

    // and the read-back now reports nothing locked
    let out = run(&r.dir, "band", &["unlock", "nr"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
}

#[test]
fn nr_sa_is_switched_and_read_back() {
    let r = rig("radio-nr-sa");
    let out = run(&r.dir, "nr", &["sa", "on"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("AT+SP5GRAN=1"), "stdout:\n{stdout}");
    assert!(stdout.contains("NR SA allowed"), "stdout:\n{stdout}");
}

#[test]
fn ims_status_probes_volte_and_vonr() {
    let r = rig("radio-ims");
    let out = run(&r.dir, "ims", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("+CAVIMS: 0"), "stdout:\n{stdout}");
    assert!(stdout.contains("VoLTE disabled"), "stdout:\n{stdout}");
    assert!(stdout.contains("+SP5GCMDS"), "stdout:\n{stdout}");
}

#[test]
fn register_reports_the_ue_usage_setting() {
    let r = rig("radio-uemode");
    let out = run(&r.dir, "register", &["uemode"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("+CEUS: 0"), "stdout:\n{stdout}");
    assert!(stdout.contains("+CEMODE: 1"), "stdout:\n{stdout}");

    let out = run(&r.dir, "register", &["voice-centric"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("AT+CEUS=1"), "stdout:\n{stdout}");
    assert!(stdout.contains("AT+CEMODE=2"), "stdout:\n{stdout}");
}
