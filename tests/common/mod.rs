//! Test rig: a fake CP on a pty.
//!
//! A pty is the only honest stand-in for an SIPC tty: a real tty with a driver
//! that hands lines over in bursts.  The fake modem answers the commands this
//! daemon actually sends, interleaves a URC into every reply, and supports the
//! `>` continuation prompt -- which is what makes these tests about the AT
//! layer rather than about the test's own convenience.

#![allow(dead_code)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub fn pty_pair() -> (File, PathBuf) {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "posix_openpt");
    unsafe {
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
    }
    let mut name = [0i8; 256];
    let rc = unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) };
    assert_eq!(rc, 0, "ptsname_r");
    let cstr = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    (
        unsafe { File::from_raw_fd(master) },
        PathBuf::from(cstr.to_str().unwrap()),
    )
}

pub struct FakeModem {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    commands: Arc<Mutex<Vec<String>>>,
    urc_lines: Arc<AtomicU64>,
}

impl FakeModem {
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    pub fn urc_lines(&self) -> u64 {
        self.urc_lines.load(Ordering::SeqCst)
    }

    pub fn wait_for_command(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.commands.lock().unwrap().iter().any(|c| c.contains(needle)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Stop answering and stop emitting URCs; the port stays open.
    pub fn silence(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for FakeModem {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Answer AT commands on `cmd_master`; interleave a URC into every reply.
pub fn fake_modem(cmd_master: File) -> FakeModem {
    let stop = Arc::new(AtomicBool::new(false));
    let commands = Arc::new(Mutex::new(Vec::new()));
    let urc_lines = Arc::new(AtomicU64::new(0));

    let stop_thread = Arc::clone(&stop);
    let cmds = Arc::clone(&commands);
    let urcs = Arc::clone(&urc_lines);

    let mut reader = cmd_master.try_clone().expect("clone master");
    let mut writer = cmd_master;
    let handle = std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut awaiting_body = false;
        let mut state = ModemState::default();
        while !stop_thread.load(Ordering::SeqCst) {
            let n = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => continue,
            };
            buf.extend_from_slice(&chunk[..n]);
            loop {
                if awaiting_body {
                    // an SMS body is terminated by Ctrl-Z, not by a newline
                    match buf.iter().position(|b| *b == 0x1a) {
                        Some(i) => {
                            buf.drain(..=i);
                            awaiting_body = false;
                            let _ = writer.write_all(b"+CMGS: 7\r\nOK\r\n");
                            continue;
                        }
                        None => break,
                    }
                }
                let Some(idx) = buf.iter().position(|b| *b == b'\r' || *b == b'\n') else {
                    break;
                };
                let line: Vec<u8> = buf.drain(..=idx).collect();
                let cmd = String::from_utf8_lossy(&line[..idx]).trim().to_string();
                if cmd.is_empty() {
                    continue;
                }
                cmds.lock().unwrap().push(cmd.clone());
                if cmd.to_ascii_uppercase().starts_with("AT+CMGS") {
                    let _ = writer.write_all(b"\r\n> ");
                    awaiting_body = true;
                    continue;
                }
                respond(&mut writer, &cmd, &urcs, &mut state);
            }
        }
    });

    FakeModem {
        stop,
        handle: Some(handle),
        commands,
        urc_lines,
    }
}

/// Emit URCs on a separate URC channel at a fixed cadence.
pub fn urc_emitter(urc_master: File, every: Duration) -> (Arc<AtomicBool>, JoinHandle<u64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let mut writer = urc_master;
    let handle = std::thread::spawn(move || {
        let mut n: u64 = 0;
        while !stop_thread.load(Ordering::SeqCst) {
            std::thread::sleep(every);
            n += 1;
            let line = match n % 3 {
                0 => "+CSQ: 23,99\r\n".to_string(),
                1 => "+CGEV: ME PDN ACT 1\r\n".to_string(),
                _ => "+SIND: 1\r\n".to_string(),
            };
            if writer.write_all(line.as_bytes()).is_err() {
                break;
            }
        }
        n
    });
    (stop, handle)
}

/// What the fake CP has been told, so a lock can be read back.
#[derive(Default)]
struct ModemState {
    lte_words: Option<String>,
    nr_words: Option<String>,
    locked_cell: Option<(u32, u32)>,
    sa: u32,
    volte: u32,
}

fn respond(w: &mut File, cmd: &str, urcs: &AtomicU64, state: &mut ModemState) {
    // A URC first: a real CP does not wait for the command to finish, and a
    // demultiplexer that reads it as the reply is exactly the bug to catch.
    let _ = w.write_all(b"+CGEV: ME PDN ACT 1\r\n");
    urcs.fetch_add(1, Ordering::SeqCst);

    let upper = cmd.to_ascii_uppercase();
    let body: String = if upper == "AT" {
        "OK\r\n".into()
    } else if upper.starts_with("AT+CSQ") {
        "+CSQ: 23,99\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CESQ") {
        "+CESQ: 99,99,255,255,20,60,75,67,73\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CPIN?") {
        "+CPIN: READY\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CIMI") {
        "460011234567890\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CCID") {
        "+CCID: <iccid>\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CEREG?") {
        "+CEREG: 2,1,\"DE0400\",\"005BE001\",11\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CGREG?") || upper.starts_with("AT+CREG?") {
        "+CREG: 2,1,\"DE04\",\"005BE001\",7\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CGATT?") {
        "+CGATT: 1\r\nOK\r\n".into()
    } else if upper.starts_with("AT+C5GREG?") {
        "+C5GREG: 2,1,\"DE0400\",\"005BE001\",11\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CEUS?") {
        "+CEUS: 0\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CEMODE?") {
        "+CEMODE: 1\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CEUS=") || upper.starts_with("AT+CEMODE=") {
        "OK\r\n".into()
    } else if upper.starts_with("AT+COPS=") && upper.contains('?') {
        "+COPS: (2,\"CHN-UNICOM\",\"UNICOM\",\"46001\",7),(1,\"CHN-MOBILE\",\"CMCC\",\"46000\",7)\r\nOK\r\n".into()
    } else if upper.starts_with("AT+COPS?") {
        "+COPS: 0,2,\"46001\",11\r\nOK\r\n".into()
    } else if upper.starts_with("AT+COPS") {
        "OK\r\n".into()
    } else if upper.starts_with("AT+SPRAT?") {
        "+SPRAT: LTE 32\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CFUN?") {
        "+CFUN: 1\r\nOK\r\n".into()
    } else if upper.starts_with("AT+SFUN=") {
        "OK\r\n".into()
    } else if upper.starts_with("AT+CMGF") {
        "OK\r\n".into()
    } else if upper.starts_with("AT+CMGL") {
        "+CMGL: 1,\"REC READ\",\"+8613800138000\",,\"26/09/20,10:00:00+32\"\r\nhello\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CGACT?") {
        "+CGACT: 1,1\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CGCONTRDP") {
        "+CGCONTRDP: 1,5,\"3gnet\",\"10.105.136.142.255.0.0.0\",\"10.0.0.1\",\"58.240.57.33\",\"221.6.4.66\"\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CGDCONT") || upper.starts_with("AT+CGACT") {
        "OK\r\n".into()
    } else if upper.starts_with("AT+CGDATA") {
        "CONNECT\r\n".into()
    } else if upper.starts_with("AT+CLCC") {
        "+CLCC: 1,0,0,0,0,\"\",129\r\nOK\r\n".into()
    } else if upper.starts_with("AT+CUSD") {
        "+CUSD: 0,\"balance 12.34 CNY\",15\r\nOK\r\n".into()
    } else if upper.starts_with("AT+SPLBAND=1") {
        state.lte_words = Some(upper.trim_start_matches("AT+SPLBAND=1").trim_start_matches(',').to_string());
        "OK\r\n".into()
    } else if upper.starts_with("AT+SPLBAND=0") {
        // default: bands 1, 3 and 41 (words 49-64, 33-48, 17-32, 1-16, 65-80)
        let words = state.lte_words.clone().unwrap_or_else(|| "0,256,0,5,0".into());
        format!("+SPLBAND: {words}\r\nOK\r\n")
    } else if upper.starts_with("AT+SPLBAND=2") {
        state.nr_words = Some(upper.trim_start_matches("AT+SPLBAND=2").trim_start_matches(',').to_string());
        "OK\r\n".into()
    } else if upper.starts_with("AT+SPLBAND=3") {
        // default: n1 (value1 bit 0), n78 (value3 bit 8), n80 (super bit 2)
        let words = state.nr_words.clone().unwrap_or_else(|| "1,0,256,4".into());
        format!("+SPLBAND: {words}\r\nOK\r\n")
    } else if upper.starts_with("AT+SPFORCEFRQ") {
        if upper.ends_with(",3") {
            match state.locked_cell {
                Some((f, p)) => format!("+SPFORCEFRQ: 12,3,{f},{p}\r\nOK\r\n"),
                None => "+SPFORCEFRQ: 12,3\r\nOK\r\n".into(),
            }
        } else if upper.ends_with(",4") {
            state.locked_cell = None;
            "OK\r\n".into()
        } else {
            let nums: Vec<u32> = upper
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            if nums.len() >= 4 {
                state.locked_cell = Some((nums[nums.len() - 2], nums[nums.len() - 1]));
            }
            "OK\r\n".into()
        }
    } else if upper.starts_with("AT+SP5GRAN?") {
        format!("+SP5GRAN: {}\r\nOK\r\n", state.sa)
    } else if upper.starts_with("AT+SP5GRAN=") {
        state.sa = upper
            .trim_start_matches("AT+SP5GRAN=")
            .trim()
            .parse()
            .unwrap_or(0);
        "OK\r\n".into()
    } else if upper.starts_with("AT+CAVIMS?") {
        format!("+CAVIMS: {}\r\nOK\r\n", state.volte)
    } else if upper.starts_with("AT+CAVIMS=") {
        state.volte = upper
            .trim_start_matches("AT+CAVIMS=")
            .trim()
            .parse()
            .unwrap_or(0);
        "OK\r\n".into()
    } else if upper.starts_with("AT+SP5GCMDS") {
        "+SP5GCMDS: 0,0,1\r\nOK\r\n".into()
    } else {
        "ERROR\r\n".into()
    };
    let _ = w.write_all(body.as_bytes());
}

/// A profile whose channels are the given pty slave paths.
pub fn profile_toml(cmd: &Path, urc: Option<&Path>, extra: &str) -> String {
    let mut s = format!(
        "name = \"pty\"\ngeneration = \"test\"\nverified = false\n\n[channels]\ncmd = \"{}\"\n",
        cmd.display()
    );
    if let Some(u) = urc {
        s.push_str(&format!("urc = \"{}\"\n", u.display()));
    }
    s.push_str("\n[at]\npace_seconds = 0.01\ndefault_timeout = 2.0\nurc_gap_seconds = 1.0\n");
    s.push_str(extra);
    s
}

/// A scratch directory unique to one test.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("unisoc-cpd-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}
