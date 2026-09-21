//! W7 end to end: the web front-end against a running daemon on the fake CP.
//!
//! The page and the JSON API must answer while `serve` owns the channels;
//! what they return is the daemon's own data, passed through.

mod common;

use common::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_unisoc-cpd")
}

fn http_try(port: u16, path: &str) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: bench\r\nConnection: close\r\n\r\n").ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

fn wait_for_daemon(socket: &Path) -> bool {
    for _ in 0..50 {
        if UnixStream::connect(socket).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn http_post_try(port: u16, path: &str, body: &str) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: bench\r\nContent-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

#[test]
fn web_serves_the_page_and_the_daemons_state() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    // Leak the fake modem for the process lifetime.
    std::mem::forget(_modem);
    let dir = scratch("web");
    let profile = dir.join("pty.toml");
    std::fs::write(&profile, profile_toml(&slave, None, "")).expect("write profile");
    let socket = dir.join("state").join("cmd.sock");
    let runs = dir.join("runs");
    let state = dir.join("state");

    let mut serve = Command::new(bin())
        .args([
            "--profile",
            profile.to_str().unwrap(),
            "--mode",
            "native",
            "--runs-dir",
            runs.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
            "serve",
            "--seconds",
            "180",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");
    assert!(wait_for_daemon(&socket), "serve did not come up");

    // A distinct port per test process, so parallel runs cannot collide.
    let port = 21000 + (std::process::id() % 20000) as u16;
    let web_log = std::fs::File::create(dir.join("web.log")).expect("create web.log");
    let mut web = Command::new(bin())
        .args([
            "--profile",
            profile.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "web",
            &format!("127.0.0.1:{port}"),
        ])
        .stdout(web_log.try_clone().expect("clone log"))
        .stderr(web_log.try_clone().expect("clone log"))
        .spawn()
        .expect("spawn web");

    let mut page_ok = false;
    let mut state_ok = false;
    let mut dial_ok = false;
    let mut at_ok = false;
    let mut info_ok = false;
    let mut identity_ok = false;
    let mut network_ok = false;
    let mut metrics_ok = false;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(body) = http_try(port, "/") {
            page_ok |= body.contains("unisoc-cpd");
        }
        if let Some(body) = http_try(port, "/api/state") {
            state_ok |= body.contains("\"at\"");
        }
        if let Some(body) = http_try(port, "/api/urc") {
            dial_ok |= body.contains("urc");
        }
        if !at_ok {
            // The AT console is a front-end over the daemon's own `at`
            // capability, so the fake CP's answer is what must come back.
            if let Some(body) = http_post_try(port, "/api/at", "cmd=AT%2BCSQ") {
                at_ok |= body.contains("+CSQ: 23,99");
            }
        }
        if !info_ok {
            // `link info` runs over the same socket; its summary lines are
            // what the baseband bar reads.
            if let Some(body) = http_try(port, "/api/info") {
                info_ok |= body.contains("FAKE-CP-MODEL") && body.contains("FAKE-FW-0.0.1");
            }
        }
        if !identity_ok {
            if let Some(body) = http_try(port, "/api/identity") {
                identity_ok |= body.contains("8986012345678901234")
                    && body.contains("+8613800138000")
                    && body.contains("460011234567890");
            }
        }
        if !network_ok {
            if let Some(body) = http_try(port, "/api/network") {
                network_ok |= body.contains("46001") && body.contains("5G SA");
            }
        }
        if !metrics_ok {
            if let Some(body) = http_try(port, "/api/metrics") {
                // The serving cell and one neighbour, out of the (placeholder)
                // measurement the fake CP answers.
                metrics_ok |= body.contains("\"earfcn\":1650")
                    && body.contains("-85.0")
                    && body.contains("627264");
            }
        }
        if page_ok && state_ok && dial_ok && at_ok && info_ok && identity_ok && network_ok && metrics_ok
        {
            break;
        }
    }
    let _ = web.kill();
    let _ = serve.kill();
    let web_out = std::fs::read_to_string(dir.join("web.log")).unwrap_or_default();
    assert!(page_ok, "the page did not load; web.log: {web_out}");
    assert!(state_ok, "/api/state did not answer");
    assert!(dial_ok, "/api/urc did not answer");
    assert!(at_ok, "/api/at did not run AT+CSQ through the daemon");
    assert!(info_ok, "/api/info did not report the CP identity");
    assert!(identity_ok, "/api/identity did not report ICCID/phone/IMSI");
    assert!(network_ok, "/api/network did not report the operator and the RAT");
    assert!(metrics_ok, "/api/metrics did not report the serving cell and neighbours");
}
