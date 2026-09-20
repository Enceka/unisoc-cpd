//! AT-layer behaviour over a real tty: demultiplexing, pacing, timeouts and
//! the `>` continuation prompt.

mod common;

use common::*;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use unisoc_cpd::at::{AtSession, FinalCode};
use unisoc_cpd::channel::SerialChannel;
use unisoc_cpd::profile::AtOptions;

struct Rig {
    _cmd: Arc<SerialChannel>,
    _urc: Option<Arc<SerialChannel>>,
    session: Arc<AtSession>,
}

fn rig(cmd_path: &Path, urc_path: Option<&Path>, lock: &Path, pace: f64) -> Rig {
    let cmd = Arc::new(SerialChannel::new(
        cmd_path,
        "cmd",
        lock,
        Duration::from_millis(10),
        true,
    ));
    cmd.open().expect("open cmd");
    let urc = urc_path.map(|p| {
        let c = Arc::new(SerialChannel::new(
            p,
            "urc",
            lock,
            Duration::from_millis(10),
            true,
        ));
        c.open().expect("open urc");
        c
    });
    let mut opts = AtOptions::default();
    opts.pace_seconds = pace;
    opts.default_timeout = 2.0;
    opts.urc_gap_seconds = 1.0;
    let session = Arc::new(AtSession::new(Arc::clone(&cmd), urc.clone(), opts));
    Rig {
        _cmd: cmd,
        _urc: urc,
        session,
    }
}

#[test]
fn a_urc_interleaved_with_a_reply_is_routed_not_swallowed() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    let dir = scratch("urc-demux");
    let r = rig(&slave, None, &dir, 0.01);

    let reply = r.session.command("AT+CSQ", Duration::from_secs(2), &[], 0);

    assert_eq!(reply.final_code, FinalCode::Ok, "reply: {reply:?}");
    assert!(
        reply.lines.iter().any(|l| l == "+CSQ: 23,99"),
        "the response line was not kept: {:?}",
        reply.lines
    );
    assert!(
        reply.urcs.iter().any(|l| l.contains("+CGEV:")),
        "the interleaved URC was swallowed: {:?}",
        reply.urcs
    );
    assert!(
        !reply.lines.iter().any(|l| l.contains("+CGEV:")),
        "a URC leaked into the response: {:?}",
        reply.lines
    );
}

#[test]
fn an_unanswered_command_times_out_and_is_not_retried_by_default() {
    // A pty with nobody on the other end is a modem that has stopped talking.
    let (_master, slave) = pty_pair();
    let dir = scratch("timeout");
    let r = rig(&slave, None, &dir, 0.01);

    let started = Instant::now();
    let reply = r.session.command("AT", Duration::from_millis(300), &[], 0);

    assert_eq!(reply.final_code, FinalCode::Timeout);
    assert_eq!(reply.attempts, 1, "no retry was asked for");
    assert!(started.elapsed() >= Duration::from_millis(290));
    assert_eq!(r.session.metrics().timeouts, 1);
}

#[test]
fn timeout_retries_are_honoured_when_asked_for() {
    let (_master, slave) = pty_pair();
    let dir = scratch("retry");
    let r = rig(&slave, None, &dir, 0.01);

    let reply = r.session.command("AT", Duration::from_millis(120), &[], 2);

    assert_eq!(reply.final_code, FinalCode::Timeout);
    assert_eq!(reply.attempts, 3, "two retries on top of the first attempt");
    assert_eq!(r.session.metrics().retries, 2);
}

#[test]
fn pacing_puts_a_floor_between_writes() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    let dir = scratch("pacing");
    let r = rig(&slave, None, &dir, 0.3);

    let started = Instant::now();
    for _ in 0..3 {
        let reply = r.session.command("AT", Duration::from_secs(2), &[], 0);
        assert!(reply.ok());
    }
    // Two gaps of 0.3 s between three commands.
    assert!(
        started.elapsed() >= Duration::from_millis(600),
        "commands were not paced: {:?}",
        started.elapsed()
    );
}

#[test]
fn cmgs_waits_for_the_prompt_and_sends_the_body() {
    let (master, slave) = pty_pair();
    let modem = fake_modem(master);
    let dir = scratch("cmgs");
    let r = rig(&slave, None, &dir, 0.01);

    let reply = r.session.command_prompted(
        "AT+CMGS=\"+8613800138000\"",
        Duration::from_secs(2),
        "hello from the daemon",
        Duration::from_secs(3),
    );

    assert_eq!(reply.final_code, FinalCode::Ok, "reply: {reply:?}");
    assert!(
        reply.lines.iter().any(|l| l.contains("+CMGS:")),
        "no +CMGS reference: {:?}",
        reply.lines
    );
    assert!(modem.wait_for_command("AT+CMGS", Duration::from_secs(1)));
}

#[test]
fn an_error_final_code_is_reported_as_an_error() {
    let (master, slave) = pty_pair();
    let _modem = fake_modem(master);
    let dir = scratch("error");
    let r = rig(&slave, None, &dir, 0.01);

    let reply = r
        .session
        .command("AT+NOSUCHTHING", Duration::from_secs(2), &[], 0);

    assert_eq!(reply.final_code, FinalCode::Error);
    assert!(!reply.ok());
    assert_eq!(r.session.metrics().errors, 1);
}

#[test]
fn the_urc_channel_is_drained_continuously() {
    let (cmd_master, cmd_slave) = pty_pair();
    let (urc_master, urc_slave) = pty_pair();
    let _modem = fake_modem(cmd_master);
    let (stop, handle) = urc_emitter(urc_master, Duration::from_millis(50));
    let dir = scratch("urc-pump");

    let r = rig(&cmd_slave, Some(&urc_slave), &dir, 0.01);
    r.session.start_urc_pump();
    std::thread::sleep(Duration::from_millis(400));
    r.session.stop_urc_pump();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = handle.join();

    let m = r.session.metrics();
    assert!(m.urc_lines >= 2, "URC channel was not drained: {m:?}");
    assert!(r.session.urc_tail(5).len() >= 2);
}
