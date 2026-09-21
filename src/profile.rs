//! Platform profiles.
//!
//! A profile is the only thing in this program that may know a board's names:
//! device nodes, partitions, interface names, spool channels, vendor commands.
//! The core asks the profile; it never hard-codes a platform.  `tools/profile-check`
//! enforces that boundary mechanically.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// URC prefixes this CP family emits.  Measured on the first platform
/// (`docs/BASEBAND-CONTRACTS.md`) and shared by the generation, not the board.
pub const DEFAULT_URC_PREFIXES: &[&str] = &[
    "+SIND:",
    "+ECIND:",
    "+CGEV:",
    "+SPPCODATA:",
    "+CREG:",
    "+CGREG:",
    "+CEREG:",
    "+CSQ:",
    "+CESQ:",
    "+CTZV:",
    "+CMTI:",
    "+CMT:",
    "+CDS:",
    "+CLIP:",
    "+CRING:",
    "+CCWA:",
    "+CUSD:",
    "+CPIN:",
    "+SPERROR:",
    "+SPERRLOG:",
    "^CONN:",
];

#[derive(Debug, Clone, Deserialize)]
pub struct Channels {
    /// The AT command channel.  Required: without it there is no daemon.
    pub cmd: String,
    /// The URC channel: the modem's unsolicited output.  It must be drained
    /// continuously once opened, so it is owned, not polled.
    #[serde(default)]
    pub urc: Option<String>,
    /// Spool channels (log/dump families).  Names are platform data.
    #[serde(default)]
    pub log: Option<String>,
    #[serde(default)]
    pub dump: Option<String>,
    #[serde(default)]
    pub stime: Option<String>,
    #[serde(default)]
    pub spool: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AtOptions {
    /// Minimum gap between two writes to the command channel.  The CP asserts
    /// when its queue is filled by unpaced AT (docs/FINDINGS.md 12).
    pub pace_seconds: f64,
    pub default_timeout: f64,
    pub reopen_backoff: f64,
    pub idle_probe_seconds: f64,
    /// A URC gap longer than this counts as a health event.
    pub urc_gap_seconds: f64,
    pub urc_prefixes: Vec<String>,
}

impl Default for AtOptions {
    fn default() -> Self {
        Self {
            pace_seconds: 0.3,
            default_timeout: 6.0,
            reopen_backoff: 1.0,
            idle_probe_seconds: 60.0,
            urc_gap_seconds: 5.0,
            // The generation's prefixes are the default even for a profile that
            // says nothing, and for callers that build these options directly.
            urc_prefixes: DEFAULT_URC_PREFIXES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl AtOptions {
    fn finalize(&mut self) {
        let mut merged: Vec<String> = DEFAULT_URC_PREFIXES.iter().map(|s| s.to_string()).collect();
        for p in &self.urc_prefixes {
            if !merged.contains(p) {
                merged.push(p.clone());
            }
        }
        self.urc_prefixes = merged;
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Mailbox {
    pub driver: String,
    /// Substring identifying the mailbox lines in `/proc/interrupts`.
    pub irq_match: String,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self {
            // Documentation only; the platform names its own driver.  "mailbox"
            // is the generic token the interrupt table uses.
            driver: String::new(),
            irq_match: "mailbox".into(),
        }
    }
}

/// How the host has to be told about a bearer its own network stack did not
/// bring up, and who is behind that bearer (see `src/nat.rs`).
///
/// Every name here is a platform name -- a table, a chain, a LAN interface --
/// which is why they are profile keys rather than literals in the core: the A12
/// gate in `profile_check` scans the core for exactly this vocabulary.  The one
/// thing deliberately absent is the clients' subnets, because what a platform
/// handed out at run time is a fact about a running system, not a setting.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Nat {
    /// The owner's decision, like `nv.readonly`: while this is false, `data up`
    /// leaves the host's networking alone and `data nat on` refuses.
    pub enabled: bool,
    /// The chain tethering ends in, where the platform has one: it carries a
    /// catch-all DROP and an ACCEPT pair per uplink its own stack brought up.
    pub forward_chain: Option<String>,
    /// The routing table unmarked traffic actually reaches, where the platform
    /// routes by policy.  Empty means `main` is the whole story.
    pub route_table: Option<String>,
    /// The LAN interfaces tethered clients arrive on.
    pub clients: Vec<String>,
    /// Metric for the host's own default route through the bearer.
    pub metric: u32,
}

impl Default for Nat {
    fn default() -> Self {
        Self {
            // Off unless a profile asks for it: this rewrites the host's
            // networking, so the default has to be the do-nothing one.
            enabled: false,
            forward_chain: None,
            route_table: None,
            clients: Vec::new(),
            metric: 100,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DataPath {
    pub ifname: Option<String>,
    pub ifname_template: Option<String>,
    pub cid: u32,
    pub apn_source: Option<String>,
    pub vendor_script: Option<String>,
    pub nat: Nat,
}

impl DataPath {
    pub fn interface(&self, cid: Option<u32>) -> Option<String> {
        if let Some(t) = &self.ifname_template {
            return Some(t.replace("{cid}", &(cid.unwrap_or(self.cid.max(1))).to_string()));
        }
        self.ifname.clone()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Boot {
    pub method: String,
    pub runner: Option<String>,
    pub partitions: Vec<String>,
    pub slot_suffixes: Vec<String>,
    pub active_slot: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Voice {
    pub supported: bool,
    pub mixer: Vec<String>,
    /// The ALSA card the voice route lives on, as `amixer -c` accepts it: an
    /// index or a name.  Measured hook of the vendor RIL
    /// (impl-ril/ril_call.c `speaker_mute`).
    pub card: Option<String>,
    /// The control the RIL toggles, `"Speaker Playback Switch"`.
    pub control: Option<String>,
    /// The mixer tool.  The RIL calls `alsa_amixer`; the Linux rootfs would use
    /// `amixer` from alsa-utils -- and an image that ships neither is a fact
    /// `voice` reports rather than papers over.
    pub tool: Option<String>,
}

/// One NV item that holds part of the device identity (see `capability::imei`).
#[derive(Debug, Clone, Deserialize)]
pub struct ImeiItem {
    /// The slot the item belongs to, ZERO-BASED as the CP and Android name
    /// them: SIM slot 1 is index 0 (IMEI0), SIM slot 2 is index 1 (IMEI1).
    /// This is also the value substituted for `{index}` in a write template.
    pub index: u32,
    /// The item id as four hex digits, exactly as the diag frame carries it.
    pub id: String,
    /// The AT channel this slot's identity is read on, when the slot has its
    /// own.  Measured on the unit from the vendor RIL's own numbering
    /// (impl-ril/common/atchannel.h): the channels are dealt out per card --
    /// URC, then two command channels -- so a second slot's identity is read
    /// on its channel, not on the first card's.
    pub channel: Option<String>,
}

/// The identity surface of this platform: what to probe, and — only once it
/// has been verified on the device — the write template.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Imei {
    /// Read-form commands `imei probe` sends when this list is non-empty;
    /// the core's default probe list is used when it is empty.
    pub probes: Vec<String>,
    /// The per-slot AT read, used when a diag identity item says nothing:
    /// `{index}` is the zero-based slot.  Absent means "no AT read contract",
    /// and `imei read` then reports the item's own outcome and nothing more.
    pub read_command: Option<String>,
    /// The write command template; `{imei}` and `{index}` are substituted.
    /// Empty/absent means "no verified write contract on this platform", and
    /// `imei write` refuses — nothing is guessed from probe answers.
    pub write_command: Option<String>,
}

impl Imei {
    /// The template, when it is set and not blank.  Owned, so a caller can
    /// hold it while using `ctx` mutably for the backup and the write.
    pub fn write_template(&self) -> Option<String> {
        self.write_command
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// The per-slot AT read, when the profile names one.
    pub fn read_template(&self) -> Option<String> {
        self.read_command
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// How the modem's NV is persisted on this platform, and where its identity
/// items live.  `readonly` is the owner's decision recorded in data: every
/// write path (`nv restore`, `imei write`) refuses while it is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Nv {
    pub persist: String,
    pub node: Option<String>,
    /// The diag character node identity items are read from (contracts §8).
    pub diag_node: Option<String>,
    /// The NV items holding the IMEI values, keyed by slot index.
    pub imei_items: Vec<ImeiItem>,
    pub readonly: bool,
}

impl Default for Nv {
    fn default() -> Self {
        Self {
            persist: String::new(),
            node: None,
            diag_node: None,
            imei_items: Vec::new(),
            // Read-only is the default: writes need an explicit owner decision
            // in the profile, and even then only through the guarded paths.
            readonly: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TelemetryProfile {
    /// Extra counters worth sampling around a run.
    pub paths: Vec<String>,
    /// What a CP assert looks like in the kernel log.
    pub assert_pattern: String,
    /// A URC gap longer than this is a health event.
    pub urc_gap_threshold: f64,
}

impl Default for TelemetryProfile {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            assert_pattern: "CP assert".into(),
            urc_gap_threshold: 5.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub generation: String,
    /// Set by the profile author once the platform has passed its acceptance run.
    #[serde(default)]
    pub verified: bool,
    pub channels: Channels,
    #[serde(default)]
    pub at: AtOptions,
    #[serde(default)]
    pub mailbox: Mailbox,
    #[serde(default)]
    pub data: DataPath,
    #[serde(default)]
    pub boot: Boot,
    #[serde(default)]
    pub voice: Voice,
    #[serde(default)]
    pub nv: Nv,
    #[serde(default)]
    pub imei: Imei,
    #[serde(default)]
    pub telemetry: TelemetryProfile,
    /// capability -> vendor command, for `--mode vendor`.
    #[serde(default)]
    pub vendor: BTreeMap<String, String>,
    #[serde(skip, default)]
    pub path: Option<PathBuf>,
}

impl Profile {
    pub fn platform_id(&self) -> &str {
        &self.name
    }

    pub fn vendor_command(&self, capability: &str) -> Option<&str> {
        self.vendor.get(capability).map(|s| s.as_str())
    }

    /// A profile that is not on disk (tests, fixtures).
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let mut p: Profile = toml::from_str(text).context("profile is not valid TOML")?;
        if p.name.trim().is_empty() {
            bail!("profile has an empty name");
        }
        p.at.finalize();
        Ok(p)
    }
}

pub fn load_file(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read profile {}", path.display()))?;
    let mut p =
        Profile::from_toml_str(&text).with_context(|| format!("in profile {}", path.display()))?;
    p.path = Some(path.to_path_buf());
    Ok(p)
}

/// Where the profiles live, relative to the source tree, not the cwd.
pub fn default_dir() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.join("platform").join("profiles")
}

pub fn profiles_dir(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(p) = std::env::var("UNISOC_CPD_PROFILES") {
        return PathBuf::from(p);
    }
    default_dir()
}

/// Resolve `--profile NAME|PATH`, or `$UNISOC_CPD_PROFILE`, against a directory.
pub fn resolve(spec: Option<&str>, dir: Option<&Path>) -> Result<Profile> {
    let dir = profiles_dir(dir);
    let spec = match spec {
        Some(s) => Some(s.to_string()),
        None => std::env::var("UNISOC_CPD_PROFILE").ok(),
    };

    let spec = match spec {
        Some(s) => s,
        None => {
            let mut found: Vec<String> = Vec::new();
            if dir.is_dir() {
                for entry in std::fs::read_dir(&dir)? {
                    let name = entry?.file_name().to_string_lossy().to_string();
                    if let Some(stem) = name.strip_suffix(".toml") {
                        found.push(stem.to_string());
                    }
                }
            }
            found.sort();
            match found.len() {
                0 => bail!("no profiles in {}", dir.display()),
                1 => found.remove(0),
                _ => bail!(
                    "several profiles in {} ({}); pick one with --profile",
                    dir.display(),
                    found.join(", ")
                ),
            }
        }
    };

    if spec.contains(std::path::MAIN_SEPARATOR) || spec.ends_with(".toml") {
        return load_file(Path::new(&spec));
    }
    load_file(&dir.join(format!("{spec}.toml")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
name = "fixture"
[channels]
cmd = "/dev/null"
"#;

    #[test]
    fn minimal_profile_gets_defaults() {
        let p = Profile::from_toml_str(MINIMAL).unwrap();
        assert_eq!(p.name, "fixture");
        assert_eq!(p.at.pace_seconds, 0.3);
        assert!(p.at.urc_prefixes.iter().any(|s| s == "+CGEV:"));
        assert_eq!(p.mailbox.irq_match, "mailbox");
    }

    #[test]
    fn profile_urc_prefixes_extend_the_defaults() {
        let p = Profile::from_toml_str(
            r#"
name = "x"
[channels]
cmd = "/dev/null"
[at]
urc_prefixes = ["+MYURC:"]
"#,
        )
        .unwrap();
        assert!(p.at.urc_prefixes.iter().any(|s| s == "+MYURC:"));
        assert!(p.at.urc_prefixes.iter().any(|s| s == "+CGEV:"));
    }

    #[test]
    fn missing_cmd_is_an_error() {
        assert!(Profile::from_toml_str("name = \"x\"\n").is_err());
    }

    #[test]
    fn ifname_template_substitutes() {
        let p = Profile::from_toml_str(
            r#"
name = "x"
[channels]
cmd = "/dev/null"
[data]
ifname_template = "sipa_eth{cid}"
cid = 3
"#,
        )
        .unwrap();
        assert_eq!(p.data.interface(None).unwrap(), "sipa_eth3");
        assert_eq!(p.data.interface(Some(1)).unwrap(), "sipa_eth1");
    }

    /// The sipa family numbers its interfaces one below the CID, which a plain
    /// `{cid}` template cannot express -- those platforms name the interface
    /// outright instead.  This test pins that decision.
    #[test]
    fn explicit_ifname_wins_over_the_template() {
        let p = Profile::from_toml_str(
            r#"
name = "x"
[channels]
cmd = "/dev/null"
[data]
ifname = "sipa_eth0"
cid = 1
"#,
        )
        .unwrap();
        assert_eq!(p.data.interface(None).unwrap(), "sipa_eth0");
    }
}
