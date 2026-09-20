//! Channel layer: owns a character device, drains it forever, counts what it sees.
//!
//! The rule this module exists to enforce is the red line in the plan: **never
//! two readers on one AT channel.**  An `flock` on a per-channel lock file makes
//! a second owner fail loudly instead of silently stealing every other line.
//!
//! The reader is a thread, not a poll at call time, because the SIPC channel
//! hands over a *queue*: what nobody reads is handed to the next command as its
//! response.  An always-on reader is what keeps a URC burst out of a reply.

use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Another process already owns this channel.
#[derive(Debug)]
pub struct ChannelBusy(pub String);

impl std::fmt::Display for ChannelBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ChannelBusy {}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChannelMetrics {
    pub opens: u64,
    pub reopens: u64,
    pub read_errors: u64,
    pub rx_lines: u64,
    pub rx_bytes: u64,
    pub tx_lines: u64,
    pub tx_bytes: u64,
    pub last_rx_at: Option<f64>,
    pub last_tx_at: Option<f64>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Default)]
struct Counters {
    opens: AtomicU64,
    reopens: AtomicU64,
    read_errors: AtomicU64,
    rx_lines: AtomicU64,
    rx_bytes: AtomicU64,
    tx_lines: AtomicU64,
    tx_bytes: AtomicU64,
    last_rx_ms: AtomicU64,
    last_tx_ms: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> ChannelMetrics {
        let ms = |v: u64| if v == 0 { None } else { Some(v as f64 / 1000.0) };
        let m = ChannelMetrics {
            opens: self.opens.load(Ordering::Relaxed),
            reopens: self.reopens.load(Ordering::Relaxed),
            read_errors: self.read_errors.load(Ordering::Relaxed),
            rx_lines: self.rx_lines.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            tx_lines: self.tx_lines.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            last_rx_at: ms(self.last_rx_ms.load(Ordering::Relaxed)),
            last_tx_at: ms(self.last_tx_ms.load(Ordering::Relaxed)),
        };
        m
    }
}

struct Shared {
    fd: AtomicI32,
    queue: Mutex<VecDeque<String>>,
    cv: Condvar,
    counters: Counters,
    stop: AtomicBool,
    dead: AtomicBool,
}

/// Put a descriptor into raw mode (`stty raw -echo`) on the open fd.
///
/// A spool device may not be a tty at all; that is not an error.
fn set_raw(fd: RawFd) {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return;
        }
        t.c_iflag = 0;
        t.c_oflag = 0;
        t.c_lflag = 0;
        t.c_cflag = (t.c_cflag | libc::CS8 | libc::CREAD | libc::CLOCAL)
            & !(libc::PARENB | libc::CSTOPB);
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 0;
        let _ = libc::tcsetattr(fd, libc::TCSANOW, &t);
    }
}

fn flock_exclusive(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(ChannelBusy(format!(
                "another process owns this channel (lock {})",
                path.display()
            ))
            .into());
        }
        bail!("flock {}: {err}", path.display());
    }
    let _ = f.set_len(0);
    let _ = write!(f, "{}\n", std::process::id());
    Ok(f)
}

pub struct SerialChannel {
    pub path: PathBuf,
    pub name: String,
    lock_dir: PathBuf,
    reopen_backoff: Duration,
    exclusivity: bool,
    shared: Arc<Shared>,
    file: Mutex<Option<File>>,
    lock_file: Mutex<Option<File>>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl SerialChannel {
    pub fn new(
        path: impl Into<PathBuf>,
        name: impl Into<String>,
        lock_dir: impl Into<PathBuf>,
        reopen_backoff: Duration,
        exclusivity: bool,
    ) -> Self {
        Self {
            path: path.into(),
            name: name.into(),
            lock_dir: lock_dir.into(),
            reopen_backoff,
            exclusivity,
            shared: Arc::new(Shared {
                fd: AtomicI32::new(-1),
                queue: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
                counters: Counters::default(),
                stop: AtomicBool::new(false),
                dead: AtomicBool::new(false),
            }),
            file: Mutex::new(None),
            lock_file: Mutex::new(None),
            reader: Mutex::new(None),
        }
    }

    pub fn is_open(&self) -> bool {
        self.shared.fd.load(Ordering::Relaxed) >= 0
    }

    pub fn healthy(&self) -> bool {
        self.is_open() && !self.shared.dead.load(Ordering::Relaxed)
    }

    pub fn metrics(&self) -> ChannelMetrics {
        self.shared.counters.snapshot()
    }

    fn acquire_ownership(&self) -> Result<()> {
        if !self.exclusivity {
            return Ok(());
        }
        let lock_path = self.lock_dir.join(format!("{}.owner.lock", self.name));
        let f = flock_exclusive(&lock_path)?;
        *self.lock_file.lock().unwrap() = Some(f);
        Ok(())
    }

    fn release_ownership(&self) {
        self.lock_file.lock().unwrap().take();
    }

    pub fn open(&self) -> Result<()> {
        if self.is_open() {
            return Ok(());
        }
        self.acquire_ownership()?;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(&self.path);
        let file = match file {
            Ok(f) => f,
            Err(e) => {
                self.release_ownership();
                bail!("cannot open {}: {e}", self.path.display());
            }
        };
        let fd = file.as_raw_fd();
        set_raw(fd);

        self.shared.stop.store(false, Ordering::SeqCst);
        self.shared.dead.store(false, Ordering::SeqCst);
        self.shared.fd.store(fd, Ordering::SeqCst);
        self.shared.counters.opens.fetch_add(1, Ordering::Relaxed);
        let opens = self.shared.counters.opens.load(Ordering::Relaxed);
        if opens > 1 {
            self.shared.counters.reopens.fetch_add(1, Ordering::Relaxed);
        }
        *self.file.lock().unwrap() = Some(file);

        let shared = Arc::clone(&self.shared);
        let handle = thread::Builder::new()
            .name(format!("{}-reader", self.name))
            .spawn(move || reader_loop(shared))
            .expect("spawn reader thread");
        *self.reader.lock().unwrap() = Some(handle);
        Ok(())
    }

    /// Stop the reader and drop the descriptor, keeping ownership of the lock.
    fn teardown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.reader.lock().unwrap().take() {
            let _ = h.join();
        }
        let file = self.file.lock().unwrap().take();
        self.shared.fd.store(-1, Ordering::SeqCst);
        drop(file);
    }

    pub fn close(&self) {
        self.teardown();
        self.release_ownership();
    }

    /// Reopen after the device went away.  Never panics; reports success.
    pub fn reopen(&self) -> bool {
        self.teardown();
        thread::sleep(self.reopen_backoff);
        match self.open() {
            Ok(()) => true,
            Err(e) => {
                log_line(&format!("{}: reopen failed: {e}", self.name));
                false
            }
        }
    }

    /// Reopen if not healthy.  Returns whether the channel is usable now.
    pub fn ensure_open(&self) -> bool {
        if self.healthy() {
            return true;
        }
        self.reopen()
    }

    pub fn take_pending(&self) -> Vec<String> {
        let mut q = self.shared.queue.lock().unwrap();
        q.drain(..).collect()
    }

    /// Discard whatever is queued after letting the line settle.
    pub fn drain(&self, settle: Duration) -> Vec<String> {
        thread::sleep(settle);
        self.take_pending()
    }

    pub fn read_line(&self, timeout: Duration) -> Option<String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut q = self.shared.queue.lock().unwrap();
        loop {
            if let Some(line) = q.pop_front() {
                return Some(line);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (guard, _) = self.shared.cv.wait_timeout(q, remaining).unwrap();
            q = guard;
        }
    }

    /// Write bytes with no terminator (the SMS body, terminated by the caller).
    pub fn write_payload(&self, text: &str) -> Result<()> {
        let guard = self.file.lock().unwrap();
        let file = guard.as_ref().ok_or_else(|| anyhow::anyhow!("{} is not open", self.name))?;
        let fd = file.as_raw_fd();
        let n = unsafe { libc::write(fd, text.as_ptr() as *const libc::c_void, text.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            self.shared.dead.store(true, Ordering::SeqCst);
            bail!("write to {} failed: {err}", self.path.display());
        }
        self.shared.counters.tx_bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(())
    }

    pub fn write_line(&self, text: &str) -> Result<()> {
        let guard = self.file.lock().unwrap();
        let file = guard.as_ref().ok_or_else(|| anyhow::anyhow!("{} is not open", self.name))?;
        let fd = file.as_raw_fd();
        let payload = format!("{text}\r");
        let n = unsafe { libc::write(fd, payload.as_ptr() as *const libc::c_void, payload.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            self.shared.dead.store(true, Ordering::SeqCst);
            bail!("write to {} failed: {err}", self.path.display());
        }
        self.shared.counters.tx_lines.fetch_add(1, Ordering::Relaxed);
        self.shared.counters.tx_bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.shared
            .counters
            .last_tx_ms
            .store((now_secs() * 1000.0) as u64, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for SerialChannel {
    fn drop(&mut self) {
        self.teardown();
        self.release_ownership();
    }
}

fn reader_loop(shared: Arc<Shared>) {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    while !shared.stop.load(Ordering::SeqCst) {
        let fd = shared.fd.load(Ordering::SeqCst);
        if fd < 0 {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, 250) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            shared.counters.read_errors.fetch_add(1, Ordering::Relaxed);
            shared.dead.store(true, Ordering::SeqCst);
            break;
        }
        if r == 0 {
            continue;
        }
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
                _ => {
                    shared.counters.read_errors.fetch_add(1, Ordering::Relaxed);
                    shared.dead.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
        if n == 0 {
            continue;
        }
        shared.counters.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
        buf.extend_from_slice(&chunk[..n as usize]);

        loop {
            let cut = buf
                .iter()
                .position(|b| *b == b'\r' || *b == b'\n')
                .map(|i| (i, buf[i]));
            let Some((idx, sep)) = cut else { break };
            let line: Vec<u8> = buf.drain(..=idx).collect();
            let text = String::from_utf8_lossy(&line[..idx]).trim().to_string();
            let _ = sep;
            if text.is_empty() {
                continue;
            }
            {
                let mut q = shared.queue.lock().unwrap();
                q.push_back(text);
            }
            shared.counters.rx_lines.fetch_add(1, Ordering::Relaxed);
            shared
                .counters
                .last_rx_ms
                .store((now_secs() * 1000.0) as u64, Ordering::Relaxed);
            shared.cv.notify_all();
        }

        // A `>` continuation prompt (`AT+CMGS`, `AT+CMGW`) is a complete token
        // in AT even though the modem never puts a CR after it -- it arrives as
        // `\r\n> ` and would otherwise sit in the buffer for ever, timing the
        // command out.
        let tail = String::from_utf8_lossy(&buf);
        if tail.trim_end().ends_with('>') {
            let text = tail.trim().to_string();
            buf.clear();
            if !text.is_empty() {
                {
                    let mut q = shared.queue.lock().unwrap();
                    q.push_back(text);
                }
                shared.counters.rx_lines.fetch_add(1, Ordering::Relaxed);
                shared
                    .counters
                    .last_rx_ms
                    .store((now_secs() * 1000.0) as u64, Ordering::Relaxed);
                shared.cv.notify_all();
            }
        }
    }
}

fn log_line(msg: &str) {
    eprintln!("unisoc-cpd: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;

    /// A pty pair is the only honest stand-in for an SIPC tty: a real tty with
    /// two ends, whose driver hands lines over in bursts.
    fn pty_pair() -> (File, PathBuf) {
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        assert!(master >= 0, "posix_openpt failed");
        unsafe {
            assert_eq!(libc::grantpt(master), 0);
            assert_eq!(libc::unlockpt(master), 0);
        }
        let mut name = [0i8; 256];
        let rc = unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) };
        assert_eq!(rc, 0, "ptsname_r failed");
        let cstr = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
        (unsafe { File::from_raw_fd(master) }, PathBuf::from(cstr.to_str().unwrap()))
    }

    fn master_writer(m: &File) -> std::fs::File {
        let fd = unsafe { libc::dup(m.as_raw_fd()) };
        assert!(fd >= 0);
        unsafe { std::fs::File::from_raw_fd(fd) }
    }

    #[test]
    fn reader_collects_lines_from_the_device() {
        let (master, slave_path) = pty_pair();
        let ch = SerialChannel::new(
            &slave_path,
            "test-cmd",
            std::env::temp_dir().join("unisoc-cpd-test-locks"),
            Duration::from_millis(50),
            false,
        );
        ch.open().expect("open pty slave");
        let mut w = master_writer(&master);
        w.write_all(b"+CSQ: 22,99\r\n").unwrap();
        w.write_all(b"OK\r\n").unwrap();
        assert_eq!(ch.read_line(Duration::from_secs(2)).as_deref(), Some("+CSQ: 22,99"));
        assert_eq!(ch.read_line(Duration::from_secs(2)).as_deref(), Some("OK"));
        assert_eq!(ch.metrics().rx_lines, 2);
        ch.close();
    }

    #[test]
    fn a_second_owner_is_refused() {
        let (master, slave_path) = pty_pair();
        let dir = std::env::temp_dir().join("unisoc-cpd-test-locks-2");
        let _ = std::fs::remove_dir_all(&dir);
        let a = SerialChannel::new(&slave_path, "excl", &dir, Duration::from_millis(10), true);
        a.open().expect("first owner");
        let b = SerialChannel::new(&slave_path, "excl", &dir, Duration::from_millis(10), true);
        let err = b.open().unwrap_err();
        assert!(err.downcast_ref::<ChannelBusy>().is_some(), "got {err:?}");
        a.close();
        drop(master);
    }

    #[test]
    fn write_line_appends_cr() {
        let (master, slave_path) = pty_pair();
        let ch = SerialChannel::new(
            &slave_path,
            "test-w",
            std::env::temp_dir().join("unisoc-cpd-test-locks-3"),
            Duration::from_millis(10),
            false,
        );
        ch.open().unwrap();
        ch.write_line("AT").unwrap();
        let mut r = master_writer(&master);
        let mut buf = [0u8; 16];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"AT\r");
        ch.close();
    }
}
