//! Read-only probes for the counters the acceptance matrix is written in:
//! mailbox interrupts, data-path counters and the kernel log's CP assert count.
//!
//! Every probe returns `Option`: a profile is allowed to name a counter that
//! does not exist on a platform, and that must degrade to "not measured"
//! rather than fail a run.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Default)]
pub struct CounterDelta {
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub delta: Option<i64>,
}

impl CounterDelta {
    pub fn between(before: Option<i64>, after: Option<i64>) -> Self {
        let delta = match (before, after) {
            (Some(a), Some(b)) => Some(b - a),
            _ => None,
        };
        Self {
            before,
            after,
            delta,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct NetCounters {
    pub ifname: String,
    pub operstate: Option<String>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
    pub rx_packets: Option<u64>,
    pub tx_packets: Option<u64>,
    pub ipv4: Vec<String>,
}

impl NetCounters {
    pub fn rx_bytes_i(&self) -> Option<i64> {
        self.rx_bytes.map(|v| v as i64)
    }
    pub fn tx_bytes_i(&self) -> Option<i64> {
        self.tx_bytes.map(|v| v as i64)
    }
}

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Sum every CPU column of the interrupt lines whose text matches `needle`.
pub fn mailbox_irq_count(needle: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/interrupts").ok()?;
    let mut total: u64 = 0;
    let mut found = false;
    for line in text.lines() {
        if !line.contains(needle) {
            continue;
        }
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        for token in rest.split_whitespace() {
            match token.parse::<u64>() {
                Ok(v) => total += v,
                Err(_) => break,
            }
        }
        found = true;
    }
    if found {
        Some(total)
    } else {
        None
    }
}

pub fn net_counters(ifname: &str) -> Option<NetCounters> {
    let base = Path::new("/sys/class/net").join(ifname);
    if !base.is_dir() {
        return None;
    }
    let stat = |name: &str| -> Option<u64> {
        read_trim(&base.join("statistics").join(name))?.parse().ok()
    };
    let mut ipv4 = Vec::new();
    if let Ok(out) = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", ifname])
        .output()
    {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(addr) = line.split_whitespace().nth(3) {
                    ipv4.push(addr.to_string());
                }
            }
        }
    }
    Some(NetCounters {
        ifname: ifname.to_string(),
        operstate: read_trim(&base.join("operstate")),
        rx_bytes: stat("rx_bytes"),
        tx_bytes: stat("tx_bytes"),
        rx_packets: stat("rx_packets"),
        tx_packets: stat("tx_packets"),
        ipv4,
    })
}

/// How many times the kernel log matches `pattern` right now.
///
/// Uses `klogctl(SYSLOG_ACTION_READ_ALL)`, a non-destructive read, so a run
/// never costs a boot the evidence it is trying to collect.
pub fn kernel_log_matches(pattern: &str) -> Option<u64> {
    if pattern.is_empty() {
        return None;
    }
    // SYSLOG_ACTION_SIZE_BUFFER / SYSLOG_ACTION_READ_ALL, spelled out so this
    // builds against any libc crate version.
    const ACTION_SIZE_BUFFER: libc::c_int = 10;
    const ACTION_READ_ALL: libc::c_int = 3;
    let size = unsafe { libc::klogctl(ACTION_SIZE_BUFFER, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return None;
    }
    let size = size as usize;
    let mut buf = vec![0u8; size];
    let n = unsafe {
        libc::klogctl(
            ACTION_READ_ALL,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len() as libc::c_int,
        )
    };
    if n < 0 {
        return None;
    }
    buf.truncate(n as usize);
    let hay = String::from_utf8_lossy(&buf);
    Some(hay.matches(pattern).count() as u64)
}

/// First line of a small file, e.g. a sysfs attribute.
pub fn read_line(path: &str) -> Option<String> {
    read_trim(Path::new(path))
}

/// What a path is, without opening it.
///
/// A device node has `st_size == 0` by definition, so reporting a "size" for
/// `/dev/slog_ch` says "empty" when the node is in fact present and healthy --
/// which is exactly the wrong thing to tell an operator.  Size is therefore
/// only reported for a regular file, and everything else reports its kind.
#[derive(Debug, Clone, Serialize)]
pub struct NodeInfo {
    pub path: String,
    pub exists: bool,
    /// "char", "block", "file", "fifo", "dir", "other" or "absent".
    pub kind: String,
    pub size: Option<u64>,
}

impl NodeInfo {
    /// Human-readable state, the phrasing the diagnostics print.
    pub fn describe(&self) -> String {
        if !self.exists {
            return "absent".to_string();
        }
        match self.size {
            Some(n) => format!("{}, size {n}", self.kind),
            None => self.kind.clone(),
        }
    }
}

pub fn node_info(path: &str) -> NodeInfo {
    use std::os::unix::fs::FileTypeExt;

    let absent = || NodeInfo {
        path: path.to_string(),
        exists: false,
        kind: "absent".into(),
        size: None,
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return absent();
    };
    let ft = meta.file_type();
    let kind = if ft.is_file() {
        "file"
    } else if ft.is_char_device() {
        "char"
    } else if ft.is_block_device() {
        "block"
    } else if ft.is_fifo() {
        "fifo"
    } else if ft.is_dir() {
        "dir"
    } else {
        "other"
    };
    NodeInfo {
        path: path.to_string(),
        exists: true,
        kind: kind.into(),
        size: if ft.is_file() { Some(meta.len()) } else { None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_delta_is_none_when_unmeasured() {
        let d = CounterDelta::between(None, Some(5));
        assert_eq!(d.delta, None);
        let d = CounterDelta::between(Some(10), Some(25));
        assert_eq!(d.delta, Some(15));
    }

    #[test]
    fn mailbox_irq_is_readable_on_linux() {
        // Not an assertion about the value: this host may have no mailbox.
        let _ = mailbox_irq_count("mailbox");
    }

    #[test]
    fn kernel_log_probe_does_not_panic() {
        let _ = kernel_log_matches("CP assert");
    }

    /// The bug an on-device run exposed: a device node reported "size 0", which
    /// reads as "empty" instead of "present".
    #[test]
    fn a_device_node_reports_its_kind_not_a_size() {
        let info = node_info("/dev/null");
        assert!(info.exists);
        assert_eq!(info.kind, "char");
        assert_eq!(info.size, None, "a char device has no size to report");
        assert_eq!(info.describe(), "char");
    }

    #[test]
    fn a_regular_file_still_reports_its_size() {
        let dir = std::env::temp_dir().join("unisoc-cpd-nodeinfo-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("spool");
        std::fs::write(&f, b"0123456789").unwrap();
        let info = node_info(f.to_str().unwrap());
        assert_eq!(info.kind, "file");
        assert_eq!(info.size, Some(10));
        assert_eq!(info.describe(), "file, size 10");
    }

    #[test]
    fn a_missing_node_is_absent() {
        let info = node_info("/dev/definitely-not-here");
        assert!(!info.exists);
        assert_eq!(info.describe(), "absent");
    }
}
