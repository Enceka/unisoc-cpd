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
    rig_without(name, &[])
}

/// The same rig, on a CP that does not have the commands containing `denied`.
fn rig_without(name: &str, denied: &[&str]) -> Rig {
    let (master, slave) = pty_pair();
    let _modem = fake_modem_without(master, denied);
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
    assert!(stdout.contains("+CIREG: 0,1"), "stdout:\n{stdout}");
    assert!(stdout.contains("IMS registered"), "stdout:\n{stdout}");
}

#[test]
fn call_dials_and_hangs_up_signaling_only_without_an_audio_route() {
    let r = rig("radio-call");
    let out = run(&r.dir, "call", &["dial", "18600000000"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("ATD18600000000;"), "stdout:\n{stdout}");
    assert!(stdout.contains("signaling only"), "stdout:\n{stdout}");

    let out = run(&r.dir, "call", &["hangup"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("ATH"), "stdout:\n{stdout}");
}

/// The measurement tree is probed, and what it answers is read.  The rig
/// answers in the shape the device was measured using: **no `+SPENGMD:`
/// header**, an all-zero LTE serving line when the UE is on NR SA, twelve-field
/// LTE neighbour records and eight column-wise NR neighbour columns.
#[test]
fn serving_and_neighbours_come_out_of_the_measurement_tree() {
    let r = rig("radio-serving");

    let out = run(&r.dir, "signal", &["serving"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("AT+SPENGMD=0,6,0"), "stdout:\n{stdout}");
    assert!(stdout.contains("AT+SPENGMD=0,14,1"), "stdout:\n{stdout}");
    // The headerless NR serving cell is read...
    assert!(stdout.contains("serving_nr_band: 78"), "stdout:\n{stdout}");
    assert!(stdout.contains("serving_nr_earfcn: 627264"), "stdout:\n{stdout}");
    assert!(stdout.contains("serving_nr_pci: 5"), "stdout:\n{stdout}");
    assert!(stdout.contains("serving_nr_rsrp: -95.0"), "stdout:\n{stdout}");
    assert!(stdout.contains("serving_nr_rsrq: -12.0"), "stdout:\n{stdout}");
    // SINR is not claimed from the serving record: the field is unresolved,
    // and CESQ is what measures it.
    assert!(stdout.contains("serving_nr_sinr: -"), "stdout:\n{stdout}");
    assert!(stdout.contains("serving_nr_bandwidth: 100"), "stdout:\n{stdout}");
    assert!(stdout.contains("serving_nr_cell: 4321"), "stdout:\n{stdout}");
    // ...and an all-zero LTE serving line is a gap, not a cell at zero.
    assert!(stdout.contains("serving_lte: not reported"), "stdout:\n{stdout}");
    assert!(!stdout.contains("serving_lte_pci"), "stdout:\n{stdout}");

    let out = run(&r.dir, "signal", &["neighbors"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("AT+SPENGMD=0,6,6"), "stdout:\n{stdout}");
    assert!(stdout.contains("AT+SPENGMD=0,14,2"), "stdout:\n{stdout}");
    // Read and empty is a zero; read and parsed is the list.
    assert!(stdout.contains("neighbors_lte: 0"), "stdout:\n{stdout}");
    assert!(stdout.contains("neighbors_nr: 2"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("neighbor: NR,band=78,earfcn=627264,pci=5,rsrp=-95.0,rsrq=-12.0,sinr=1.0"),
        "stdout:\n{stdout}"
    );
}

/// W5, end to end: a CP that does not have the measurement tree must produce
/// "not reported" -- never a table with numbers in it.
#[test]
fn a_cp_that_refuses_the_measurement_tree_says_not_reported() {
    let r = rig_without("radio-w5", &["SPENGMD"]);

    let out = run(&r.dir, "signal", &["serving"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("serving_lte: not reported"), "stdout:\n{stdout}");
    assert!(stdout.contains("serving_nr: not reported"), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("serving_lte_pci"),
        "a refused query must not produce a field:\n{stdout}"
    );

    let out = run(&r.dir, "signal", &["neighbors"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("neighbors_lte: not reported"), "stdout:\n{stdout}");
    assert!(stdout.contains("neighbors_nr: not reported"), "stdout:\n{stdout}");
    assert!(!stdout.contains("neighbor: LTE"), "stdout:\n{stdout}");
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
