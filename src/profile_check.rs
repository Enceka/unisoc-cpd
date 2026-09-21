//! `profile-check` — the A12 gate, as a command instead of a promise.
//!
//! "The same binary passes A1-A11 on a second platform by adding a profile
//! only" is two claims:
//!
//!   1. every profile in the tree loads and carries the keys the core needs;
//!   2. no platform-specific name leaked into the core.
//!
//! The second one is checkable: take every name a profile is allowed to know
//! (device nodes, partitions, the interface) and look for it in `src/`, with
//! comments and `#[cfg(test)]` code stripped, because a test is allowed to
//! name a fixture.

use crate::profile::{self, Profile};
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Vocabulary of the problem domain, not names of a platform.  A profile is
/// allowed to call a partition `modem`; the core is allowed to talk about the
/// modem.  Everything else a profile names is treated as platform-specific.
const GENERIC_TOKENS: &[&str] = &["modem", "nv", "data", "log", "dump", "spool", "boot"];

/// Names every profile in the tree is allowed to know.
fn tokens_of(p: &Profile) -> Vec<String> {
    fn add(tokens: &mut BTreeSet<String>, s: &str) {
        let s = s.trim().to_ascii_lowercase();
        if s.len() >= 4
            && !s.contains('{')
            && !s.starts_with('/')
            && !GENERIC_TOKENS.contains(&s.as_str())
        {
            tokens.insert(s);
        }
    }

    let mut tokens: BTreeSet<String> = BTreeSet::new();

    // The profile's own name is the most telling token, so it is checked even
    // when it is short (`e5`).
    let name = p.name.trim().to_ascii_lowercase();
    if !name.is_empty() {
        tokens.insert(name);
    }
    for path in [
        Some(p.channels.cmd.as_str()),
        p.channels.urc.as_deref(),
        p.channels.log.as_deref(),
        p.channels.dump.as_deref(),
        p.channels.stime.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(base) = Path::new(path).file_name() {
            add(&mut tokens, &base.to_string_lossy());
        }
    }
    for value in p.channels.spool.values() {
        if let Some(base) = Path::new(value).file_name() {
            add(&mut tokens, &base.to_string_lossy());
        }
    }
    for part in &p.boot.partitions {
        add(&mut tokens, part);
    }
    if let Some(ifname) = &p.data.ifname {
        add(&mut tokens, ifname);
    }
    if let Some(template) = &p.data.ifname_template {
        if let Some(literal) = template.split('{').next() {
            add(&mut tokens, literal);
        }
    }
    // The NAT names are as platform-specific as the interface is: a routing
    // table, a firewall chain and the LAN interfaces are things only the
    // profile is allowed to know, since `src/nat.rs` builds commands out of
    // them without ever naming one.
    for value in [
        p.data.nat.forward_chain.as_deref(),
        p.data.nat.route_table.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        add(&mut tokens, value);
    }
    for client in &p.data.nat.clients {
        add(&mut tokens, client);
    }
    tokens.into_iter().collect()
}

fn strip_line(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return None;
    }
    // Drop a trailing comment when the quotes before it are balanced, so a
    // `//` inside a string literal is left alone.
    if let Some(idx) = line.rfind("//") {
        let before = &line[..idx];
        if before.matches('"').count() % 2 == 0 {
            return Some(before);
        }
    }
    Some(line)
}

/// `hay` contains `needle` as a whole identifier-like word.
fn contains_word(hay: &str, needle: &str) -> bool {
    let hay = hay.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(pos) = hay[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0
            || !hay.as_bytes()[start - 1].is_ascii_alphanumeric()
                && hay.as_bytes()[start - 1] != b'_';
        let after_ok = end == hay.len()
            || !hay.as_bytes()[end].is_ascii_alphanumeric() && hay.as_bytes()[end] != b'_';
        if before_ok && after_ok {
            return true;
        }
        from = end;
        if from >= hay.len() {
            break;
        }
    }
    false
}

fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_sources(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn scan_core(src_root: &Path, tokens: &[String]) -> Vec<String> {
    let mut findings = Vec::new();
    let mut files = Vec::new();
    if rust_sources(src_root, &mut files).is_err() {
        return findings;
    }
    files.sort();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // A test module may name a fixture; the core may not.
        let body = match text.find("#[cfg(test)]") {
            Some(idx) => &text[..idx],
            None => text.as_str(),
        };
        for (n, raw) in body.lines().enumerate() {
            let Some(line) = strip_line(raw) else {
                continue;
            };
            for token in tokens {
                if contains_word(line, token) {
                    let shown = file
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default();
                    findings.push(format!(
                        "{}:{}: core names {token:?}: {}",
                        shown,
                        n + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    findings
}

/// Returns true when the tree passes.  Everything it saw is printed.
pub fn run(profiles: Option<&Path>, verbose: bool) -> Result<bool> {
    let dir = profile::profiles_dir(profiles);
    let mut profiles_found: Vec<Profile> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    println!("profiles in {}", dir.display());
    if !dir.is_dir() {
        problems.push(format!("no profile directory at {}", dir.display()));
    } else {
        let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
            .collect();
        names.sort();
        for path in names {
            match profile::load_file(&path) {
                Ok(p) => {
                    println!(
                        "  {:<10} {:<20} verified={:<5} cmd={}",
                        p.name,
                        p.generation,
                        if p.verified { "yes" } else { "no" },
                        p.channels.cmd
                    );
                    profiles_found.push(p);
                }
                Err(e) => {
                    println!("  {}: INVALID", path.display());
                    problems.push(format!("{}: {e:#}", path.display()));
                }
            }
        }
    }
    if profiles_found.len() < 2 {
        problems.push(format!(
            "only {} profile(s): the portability gate needs a second one",
            profiles_found.len()
        ));
    }

    let mut tokens: BTreeSet<String> = BTreeSet::new();
    for p in &profiles_found {
        for t in tokens_of(p) {
            tokens.insert(t);
        }
    }
    let tokens: Vec<String> = tokens.into_iter().collect();
    if verbose {
        println!("platform tokens checked: {}", tokens.join(", "));
    }

    let src_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let findings = scan_core(&src_root, &tokens);
    println!("core scan of {}", src_root.display());
    if findings.is_empty() {
        println!("  no platform names in the core");
    } else {
        for f in &findings {
            println!("  {f}");
        }
    }
    problems.extend(findings);

    if problems.is_empty() {
        println!("profile-check: pass");
        Ok(true)
    } else {
        println!("profile-check: {} problem(s)", problems.len());
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_boundaries_are_respected() {
        assert!(contains_word("let x = ifname;", "ifname"));
        assert!(!contains_word("let x = my_ifname_x;", "ifname"));
        assert!(!contains_word("let x = ifnames;", "ifname"));
        assert!(contains_word("sipa_eth0 up", "sipa_eth0"));
    }

    #[test]
    fn trailing_comments_are_stripped_but_urls_are_not() {
        assert_eq!(strip_line("let a = 1; // e5"), Some("let a = 1; "));
        assert_eq!(strip_line("// e5"), None);
        assert_eq!(
            strip_line("let u = \"http://x/e5\";"),
            Some("let u = \"http://x/e5\";")
        );
    }

    #[test]
    fn tokens_cover_nodes_partitions_and_the_name() {
        let p = Profile::from_toml_str(
            r#"
name = "ab"
[channels]
cmd = "/dev/stty_nr1"
[data]
ifname_template = "sipa_eth{cid}"
[channels.spool]
mystery = "/dev/weird_node"
"#,
        )
        .unwrap();
        let t = tokens_of(&p);
        assert!(t.contains(&"stty_nr1".to_string()));
        assert!(t.contains(&"sipa_eth".to_string()));
        assert!(t.contains(&"weird_node".to_string()));
        // a short profile name is still checked: it is the whole point
        assert!(t.contains(&"ab".to_string()));
    }

    #[test]
    fn generic_vocabulary_is_not_a_platform_name() {
        let p = Profile::from_toml_str(
            r#"
name = "fixture"
[channels]
cmd = "/dev/modem"
[boot]
partitions = ["modem", "zebra_partition"]
"#,
        )
        .unwrap();
        let t = tokens_of(&p);
        assert!(!t.contains(&"modem".to_string()));
        assert!(t.contains(&"zebra_partition".to_string()));
    }
}
