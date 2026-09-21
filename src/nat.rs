//! The host side of a bearer the platform's own stack does not know about.
//!
//! On a platform whose network stack keeps its own opinion about which uplinks
//! exist, a bearer this daemon brings up is a stranger to it, in three separate
//! ways:
//!
//!   * the policy rules send unmarked traffic to a table whose default route
//!     points nowhere, so the host itself has no way out even though the
//!     interface has an address;
//!   * the chain tethering ends in carries a catch-all DROP and an ACCEPT pair
//!     for every uplink the platform's own stack brought up -- and none for
//!     this one, so the clients' packets die there;
//!   * nothing masquerades the clients behind a bearer the platform never
//!     handed out.
//!
//! None of those three is about this CP, and none of them is the same on every
//! platform: what differ are the table and the chain.  Those names come from
//! the profile (see `lib.rs` on the A12 gate) and never from this file; a host
//! that has no such chain says so by leaving the key out of its profile.
//!
//! Nothing here runs anything.  `install` and `remove` build a command list,
//! the `parse_*`/readers below read the output of read-only probes, and both
//! halves are therefore printable, diffable and testable off the device they
//! are meant for -- which is the only way this could be written at all, since
//! the device this matters on is one where a wrong rule takes the network away.

use std::net::Ipv4Addr;

/// A tethered LAN: the interface its clients' packets arrive on, and the subnet
/// they come from.
///
/// The subnet is deliberately *not* a profile key.  What a platform handed out
/// is a fact about a running system, and a copy of it in a file ages into a
/// route for the wrong network; it is read off the interface when the plan is
/// built, and an interface whose address cannot be read does not appear here at
/// all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub ifname: String,
    pub subnet: String,
}

/// Everything the command list needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The bearer the clients leave through.
    pub bearer: String,
    /// The bearer's own on-link network, when its address could be read.  The
    /// de-NATed return path of a client looks up the same tables as its way
    /// out, and the on-link route has to be in them for that to resolve.
    pub bearer_subnet: Option<String>,
    /// The routing table unmarked traffic reaches on this platform.  `None`
    /// means `main` is the whole story, which is the case on a host with no
    /// policy routing.
    pub route_table: Option<String>,
    /// The chain tethering ends in on this platform.  `None` means this host
    /// has no such chain and nothing is missing from it.
    pub forward_chain: Option<String>,
    pub clients: Vec<Client>,
    /// The metric on the host's own default route: high enough that a wired
    /// default appearing later wins over the bearer.
    pub metric: u32,
}

impl Plan {
    pub fn new(bearer: impl Into<String>) -> Self {
        Self {
            bearer: bearer.into(),
            bearer_subnet: None,
            route_table: None,
            forward_chain: None,
            clients: Vec::new(),
            metric: 100,
        }
    }
}

/// One command, and what makes running it twice safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Idempotent as written: `ip route replace` replaces, `sysctl -w` assigns,
    /// and a delete that has nothing to delete fails harmlessly.
    Once(Vec<String>),
    /// `iptables` has no replace form -- only `-A`, which duplicates, and `-C`,
    /// which asks.  So the check comes first and the add only runs when it
    /// fails.
    Ensure { check: Vec<String>, add: Vec<String> },
}

impl Step {
    /// The command that changes something, as a line a human can read.
    pub fn line(&self) -> String {
        match self {
            Step::Once(argv) => argv.join(" "),
            Step::Ensure { add, .. } => add.join(" "),
        }
    }

    /// What `data nat plan` prints: the command, and for an `Ensure` what it
    /// asks first.
    pub fn describe(&self) -> String {
        match self {
            Step::Once(argv) => argv.join(" "),
            Step::Ensure { check, add } => {
                format!("{}   (only if `{}` fails)", add.join(" "), check.join(" "))
            }
        }
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// The commands that give the host and its clients a way out through `bearer`.
pub fn install(plan: &Plan) -> Vec<Step> {
    let bearer = plan.bearer.as_str();
    let metric = plan.metric.to_string();
    let mut steps = Vec::new();

    // Forwarding has to be on at all before anything below means anything.
    steps.push(Step::Once(argv(&[
        "sysctl",
        "-w",
        "net.ipv4.ip_forward=1",
    ])));

    // The host's own egress.  This is the whole story on a host without policy
    // routing, and only half of it on one with.
    steps.push(Step::Once(argv(&[
        "ip",
        "route",
        "replace",
        "default",
        "dev",
        bearer,
        "metric",
        metric.as_str(),
    ])));

    if let Some(table) = plan.route_table.as_deref() {
        // The other half: unmarked traffic on a policy-routing host is sent to
        // this table by a rule, and a default route in `main` is not in it.
        steps.push(Step::Once(argv(&[
            "ip", "route", "replace", "default", "dev", bearer, "table", table,
        ])));
        if let Some(subnet) = plan.bearer_subnet.as_deref() {
            steps.push(Step::Once(argv(&[
                "ip", "route", "replace", subnet, "dev", bearer, "table", table,
            ])));
        }
        for client in &plan.clients {
            steps.push(Step::Once(argv(&[
                "ip",
                "route",
                "replace",
                client.subnet.as_str(),
                "dev",
                client.ifname.as_str(),
                "table",
                table,
            ])));
        }
    }

    // The clients' way out of the host: their packets arrive with a private
    // source address and the network has never heard of it.
    steps.push(Step::Ensure {
        check: argv(&[
            "iptables",
            "-t",
            "nat",
            "-C",
            "POSTROUTING",
            "-o",
            bearer,
            "-j",
            "MASQUERADE",
        ]),
        add: argv(&[
            "iptables",
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-o",
            bearer,
            "-j",
            "MASQUERADE",
        ]),
    });

    if let Some(chain) = plan.forward_chain.as_deref() {
        for client in &plan.clients {
            // Both directions: out to the bearer, and back to the client.  The
            // pair is inserted at the head of the chain, ahead of the catch-all
            // DROP that is the reason these are needed.
            let lan = client.ifname.as_str();
            for (from, to) in [(lan, bearer), (bearer, lan)] {
                steps.push(Step::Ensure {
                    check: argv(&["iptables", "-C", chain, "-i", from, "-o", to, "-j", "ACCEPT"]),
                    add: argv(&[
                        "iptables", "-I", chain, "1", "-i", from, "-o", to, "-j", "ACCEPT",
                    ]),
                });
            }
        }
    }

    steps
}

/// The reverse of `install`, for a bearer that is going away.
///
/// Two things it deliberately does not do.  It does not delete the host's main
/// default route: that route is the bearer's, and `data down` removes it as
/// part of tearing the bearer down, while `data nat off` on a live bearer would
/// otherwise take the host's own egress with it.  And it does not put
/// `ip_forward` back to 0, because this process did not decide that it should
/// be 1 in the first place -- something else on the host may be forwarding too,
/// and a NAT action is not the place to take that away.
pub fn remove(plan: &Plan) -> Vec<Step> {
    let bearer = plan.bearer.as_str();
    let mut steps = Vec::new();

    if let Some(chain) = plan.forward_chain.as_deref() {
        // The reverse order, so the rule removed first is the one that was
        // inserted last; `iptables -D` deletes the first match, and with
        // distinct pairs the order only matters to a reader.
        for client in &plan.clients {
            let lan = client.ifname.as_str();
            for (from, to) in [(lan, bearer), (bearer, lan)] {
                steps.push(Step::Once(argv(&[
                    "iptables", "-D", chain, "-i", from, "-o", to, "-j", "ACCEPT",
                ])));
            }
        }
    }
    steps.push(Step::Once(argv(&[
        "iptables",
        "-t",
        "nat",
        "-D",
        "POSTROUTING",
        "-o",
        bearer,
        "-j",
        "MASQUERADE",
    ])));

    if let Some(table) = plan.route_table.as_deref() {
        for client in &plan.clients {
            steps.push(Step::Once(argv(&[
                "ip",
                "route",
                "del",
                client.subnet.as_str(),
                "dev",
                client.ifname.as_str(),
                "table",
                table,
            ])));
        }
        if let Some(subnet) = plan.bearer_subnet.as_deref() {
            steps.push(Step::Once(argv(&[
                "ip", "route", "del", subnet, "dev", bearer, "table", table,
            ])));
        }
        steps.push(Step::Once(argv(&[
            "ip", "route", "del", "default", "dev", bearer, "table", table,
        ])));
    }

    steps
}

// ------------------------------------------------------------------ reading

/// The first IPv4 address in `ip addr show` output, with its prefix length.
///
/// `inet6` is a different line and is not this one: an `Ipv4Addr` built out of
/// an IPv6 address is not a wrong value so much as a wrong family, and a route
/// built from it would be nonsense in a table nothing looks up.
pub fn parse_addr(text: &str) -> Option<(Ipv4Addr, u8)> {
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            if *word != "inet" {
                continue;
            }
            let Some(token) = words.get(i + 1) else {
                continue;
            };
            let Some((addr, prefix)) = token.split_once('/') else {
                continue;
            };
            if let (Ok(addr), Ok(prefix)) = (addr.parse::<Ipv4Addr>(), prefix.parse::<u8>()) {
                return Some((addr, prefix));
            }
        }
    }
    None
}

/// The network an address and its prefix describe, e.g. `192.168.43.1/24` ->
/// `192.168.43.0/24`.
///
/// A route to the address itself works by accident here and stops working as
/// soon as the platform hands out a different host address in the same subnet,
/// which it does on every reconnect.
pub fn network(addr: Ipv4Addr, prefix: u8) -> String {
    let prefix = prefix.min(32);
    let mask: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    format!("{}/{prefix}", Ipv4Addr::from(u32::from(addr) & mask))
}

fn flag_value<'a>(words: &[&'a str], flag: &str) -> Option<&'a str> {
    words
        .iter()
        .position(|w| *w == flag)
        .and_then(|i| words.get(i + 1))
        .copied()
}

/// `ip route show` output -> the interface its default route leaves through.
pub fn default_dev(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim().trim_end_matches('\\').trim();
        if !line.starts_with("default") {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if let Some(dev) = flag_value(&words, "dev") {
            return Some(dev.to_string());
        }
    }
    None
}

/// `iptables -t nat -S POSTROUTING` output -> is `bearer` masqueraded?
///
/// A MASQUERADE with no `-o` counts: it covers this bearer too, and reporting
/// it as missing would have an operator add a rule that is already covered.
pub fn masqueraded(text: &str, bearer: &str) -> bool {
    text.lines().any(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        let target = flag_value(&words, "-j") == Some("MASQUERADE");
        let out = flag_value(&words, "-o");
        target && out.is_none_or(|dev| dev == bearer || dev.is_empty())
    })
}

/// `iptables -S <chain>` output -> is there an ACCEPT from `from` to `to`?
pub fn accepted(text: &str, from: &str, to: &str) -> bool {
    text.lines().any(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        flag_value(&words, "-j") == Some("ACCEPT")
            && flag_value(&words, "-i") == Some(from)
            && flag_value(&words, "-o") == Some(to)
    })
}

/// `ip rule show` output -> the priorities of the rules that look up `table`.
///
/// Empty is worth reporting on its own: routes can be added to a table no rule
/// ever reaches, and that table then reads as "installed" while carrying
/// nothing.  `10000: from all fwmark 0xc0000/0xd0000 lookup legacy_system` is
/// the shape this reads.
pub fn table_priorities(text: &str, table: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let priority = words.first()?.trim_end_matches(':').parse().ok()?;
            let lookup = words.iter().position(|w| *w == "lookup")?;
            (*words.get(lookup + 1)? == table).then_some(priority)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        let mut plan = Plan::new("wwan0");
        plan.bearer_subnet = Some("10.0.0.0/8".to_string());
        plan.route_table = Some("table_of_rules".to_string());
        plan.forward_chain = Some("tether_chain".to_string());
        plan.clients = vec![Client {
            ifname: "lan0".to_string(),
            subnet: "172.16.5.0/24".to_string(),
        }];
        plan
    }

    fn lines(steps: &[Step]) -> Vec<String> {
        steps.iter().map(|s| s.line()).collect()
    }

    #[test]
    fn install_covers_the_three_things_a_stranger_bearer_is_missing() {
        let lines = lines(&install(&plan()));
        assert!(lines.iter().any(|l| l == "sysctl -w net.ipv4.ip_forward=1"));
        // the host's own way out, in both the table it looks up and the one it
        // actually reaches
        assert!(lines.iter().any(|l| l == "ip route replace default dev wwan0 metric 100"));
        assert!(lines
            .iter()
            .any(|l| l == "ip route replace default dev wwan0 table table_of_rules"));
        // the on-link route the de-NATed return path needs
        assert!(lines
            .iter()
            .any(|l| l == "ip route replace 10.0.0.0/8 dev wwan0 table table_of_rules"));
        // the client's own return route, and the masquerade
        assert!(lines
            .iter()
            .any(|l| l == "ip route replace 172.16.5.0/24 dev lan0 table table_of_rules"));
        assert!(lines
            .iter()
            .any(|l| l == "iptables -t nat -A POSTROUTING -o wwan0 -j MASQUERADE"));
        // the pair, in both directions
        assert!(lines
            .iter()
            .any(|l| l == "iptables -I tether_chain 1 -i lan0 -o wwan0 -j ACCEPT"));
        assert!(lines
            .iter()
            .any(|l| l == "iptables -I tether_chain 1 -i wwan0 -o lan0 -j ACCEPT"));
    }

    #[test]
    fn a_host_without_a_tether_chain_or_policy_table_gets_only_the_generic_half() {
        let mut bare = Plan::new("wwan0");
        bare.clients = vec![Client {
            ifname: "lan0".to_string(),
            subnet: "172.16.5.0/24".to_string(),
        }];
        let lines = lines(&install(&bare));
        assert!(lines.iter().any(|l| l.contains("MASQUERADE")));
        assert!(!lines.iter().any(|l| l.contains("table_of_rules")));
        assert!(!lines.iter().any(|l| l.contains("tether_chain")));
        // and with no bearer subnet there is nothing to add a route for
        assert!(!lines
            .iter()
            .any(|l| l.starts_with("ip route replace 10.0.0.0/8")));
    }

    #[test]
    fn iptables_steps_ask_before_they_add() {
        let steps = install(&plan());
        let masquerade = steps
            .iter()
            .find(|s| s.line().contains("MASQUERADE"))
            .expect("a masquerade step");
        match masquerade {
            Step::Ensure { check, .. } => {
                assert_eq!(check[3], "-C");
                assert!(check.contains(&"POSTROUTING".to_string()));
            }
            Step::Once(_) => panic!("an append is not idempotent; it must be an Ensure"),
        }
    }

    #[test]
    fn remove_takes_the_rules_out_and_leaves_forwarding_alone() {
        let lines = lines(&remove(&plan()));
        assert!(lines
            .iter()
            .any(|l| l == "iptables -t nat -D POSTROUTING -o wwan0 -j MASQUERADE"));
        assert!(lines
            .iter()
            .any(|l| l == "iptables -D tether_chain -i lan0 -o wwan0 -j ACCEPT"));
        assert!(lines
            .iter()
            .any(|l| l == "ip route del default dev wwan0 table table_of_rules"));
        assert!(
            !lines.iter().any(|l| l.contains("ip_forward")),
            "forwarding is not ours to switch off"
        );
        assert!(
            !lines.iter().any(|l| l == "ip route del default dev wwan0"),
            "the main default route belongs to the bearer, not to NAT"
        );
    }

    #[test]
    fn an_address_line_yields_its_host_address_and_prefix() {
        let text = "\
2: dummy0    inet 10.9.9.9/32 scope global dummy0\\       valid_lft forever\n\
32: lan0    inet 192.168.43.1/24 brd 192.168.43.255 scope global lan0\\       valid_lft forever\n";
        assert_eq!(
            parse_addr(text),
            Some((Ipv4Addr::new(10, 9, 9, 9), 32)),
            "the first address in the listing is the interface's"
        );
        // an interface with only IPv6 has no IPv4 address to route
        assert_eq!(parse_addr("32: lan0    inet6 fe80::1/64 scope link\n"), None);
        assert_eq!(parse_addr("1: lo    inet 127.0.0.1/8 scope host lo\n"), Some((Ipv4Addr::LOCALHOST, 8)));
    }

    #[test]
    fn a_network_is_masked_out_of_the_address_the_platform_handed_us() {
        assert_eq!(
            network(Ipv4Addr::new(192, 168, 43, 1), 24),
            "192.168.43.0/24"
        );
        assert_eq!(network(Ipv4Addr::new(10, 41, 128, 46), 8), "10.0.0.0/8");
        assert_eq!(network(Ipv4Addr::new(10, 1, 2, 3), 32), "10.1.2.3/32");
        // a /0 is legal and must not shift by 32
        assert_eq!(network(Ipv4Addr::new(10, 1, 2, 3), 0), "0.0.0.0/0");
    }

    #[test]
    fn the_default_route_reader_takes_both_shapes() {
        assert_eq!(
            default_dev("default dev wwan0 metric 100 \n"),
            Some("wwan0".to_string())
        );
        assert_eq!(
            default_dev("default via 10.0.0.1 dev wwan0 proto static\n"),
            Some("wwan0".to_string())
        );
        assert_eq!(default_dev("10.0.0.0/8 dev wwan0 scope link\n"), None);
    }

    #[test]
    fn masquerade_is_read_with_and_without_an_output_interface() {
        let ours = "-P POSTROUTING ACCEPT\n-A POSTROUTING -o wwan0 -j MASQUERADE\n";
        assert!(masqueraded(ours, "wwan0"));
        assert!(!masqueraded(ours, "lan0"));
        // a rule wider than us already covers the bearer
        assert!(masqueraded("-A POSTROUTING -j MASQUERADE\n", "wwan0"));
        assert!(!masqueraded("-A POSTROUTING -o lan0 -j MASQUERADE\n", "wwan0"));
    }

    #[test]
    fn the_forward_pair_is_read_per_direction() {
        let chain = "\
-N tether_chain\n\
-A tether_chain -i lan0 -o wwan0 -j ACCEPT\n\
-A tether_chain -j DROP\n";
        assert!(accepted(chain, "lan0", "wwan0"));
        assert!(!accepted(chain, "wwan0", "lan0"), "the return path is a rule of its own");
        assert!(!accepted(chain, "lan0", "wwan9"));
    }

    #[test]
    fn the_rules_reaching_a_table_are_reported_by_priority() {
        let rules = "\
0:\tfrom all lookup local\n\
10000:\tfrom all fwmark 0xc0000/0xd0000 lookup table_of_rules\n\
11000:\tfrom all iif lo oif lan0 uidrange 0-0 lookup lan0\n\
32000:\tfrom all unreachable\n";
        assert_eq!(table_priorities(rules, "table_of_rules"), vec![10000]);
        // a table nothing looks up is the failure this reader exists to catch
        assert!(table_priorities(rules, "nobody_looks_here").is_empty());
    }

    #[test]
    fn the_printed_plan_says_what_each_step_asks_first() {
        let steps = install(&plan());
        let ensure = steps
            .iter()
            .find(|s| matches!(s, Step::Ensure { .. }))
            .expect("a check-then-add step");
        assert!(ensure.describe().contains("(only if `"));
        let once = steps
            .iter()
            .find(|s| matches!(s, Step::Once(_)))
            .expect("a plain step");
        assert!(!once.describe().contains("(only if `"));
    }
}
