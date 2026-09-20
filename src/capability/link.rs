//! `link` — the W1 gate.
//!
//! One owner of the AT and URC channels, a probe on a fixed cadence, and the
//! counters the acceptance matrix is written in: CP asserts, URC gaps, mailbox
//! interrupt deltas.  `link --seconds 259200` is the 72 h soak; run it for ten
//! seconds and it is still the same measurement, which is what makes it a rig
//! rather than an anecdote.

use super::{flag_value, Capability, Outcome};
use crate::context::Context;
use crate::probes;
use anyhow::Result;
use std::time::{Duration, Instant};

pub struct Link;

impl Capability for Link {
    fn name(&self) -> &'static str {
        "link"
    }

    fn summary(&self) -> &'static str {
        "own the AT/URC channels, probe on a cadence, count CP asserts, URC gaps and mailbox IRQs"
    }

    fn native_only(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let seconds: f64 = flag_value(args, "--seconds")
            .unwrap_or("10")
            .parse()
            .map_err(|_| anyhow::anyhow!("--seconds expects a number"))?;
        let interval: f64 = flag_value(args, "--interval")
            .unwrap_or("30")
            .parse()
            .map_err(|_| anyhow::anyhow!("--interval expects a number"))?;
        let timeout: f64 = flag_value(args, "--timeout")
            .unwrap_or("5")
            .parse()
            .map_err(|_| anyhow::anyhow!("--timeout expects a number"))?;
        let probe = flag_value(args, "--probe").unwrap_or("AT").to_string();

        let mailbox_before = probes::mailbox_irq_count(&ctx.profile.mailbox.irq_match);
        let session = ctx.at()?;

        let mut out: Vec<String> = Vec::new();
        out.push(format!(
            "owning cmd={} urc={}",
            ctx.profile.channels.cmd,
            ctx.profile
                .channels
                .urc
                .clone()
                .unwrap_or_else(|| "(none)".into())
        ));
        ctx.event(
            "link",
            format!("channels acquired; probe={probe} every {interval}s for {seconds}s"),
        );

        let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        let mut probes_done: u64 = 0;
        let mut failures: u64 = 0;
        let mut first = true;

        loop {
            let reply = session.probe_with(&probe, Duration::from_secs_f64(timeout));
            probes_done += 1;
            if reply.ok() {
                ctx.event(
                    "probe",
                    format!("{probe} ok in {} ms", reply.elapsed.as_millis()),
                );
                if first {
                    for line in &reply.lines {
                        out.push(format!("  {line}"));
                    }
                    out.push(format!("  {}", reply.final_code.as_str()));
                    first = false;
                } else {
                    out.push(format!(
                        "probe {probes_done} ok ({} ms)",
                        reply.elapsed.as_millis()
                    ));
                }
            } else {
                failures += 1;
                let code = reply.final_code.as_str();
                ctx.event("probe-fail", format!("{probe} -> {code}"));
                out.push(format!("probe {probes_done} FAILED -> {code}"));
                for line in &reply.lines {
                    out.push(format!("  {line}"));
                }
            }

            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            std::thread::sleep(remaining.min(Duration::from_secs_f64(interval.max(0.05))));
            if Instant::now() >= deadline {
                break;
            }
        }

        // The pump needs a moment to have seen the last URCs before we sample.
        std::thread::sleep(Duration::from_millis(300));

        let mailbox_after = probes::mailbox_irq_count(&ctx.profile.mailbox.irq_match);
        let mailbox_delta = match (mailbox_before, mailbox_after) {
            (Some(a), Some(b)) => Some(b as i64 - a as i64),
            _ => None,
        };
        let m = session.metrics();

        out.push(format!("probes {probes_done}, failures {failures}"));
        out.push(format!(
            "AT commands {} ok {} errors {} timeouts {} retries {}",
            m.commands, m.ok, m.errors, m.timeouts, m.retries
        ));
        out.push(format!(
            "URC lines {} max gap {:.1}s gaps over threshold {}",
            m.urc_lines, m.max_urc_gap_s, m.urc_gaps_over_threshold
        ));
        match mailbox_delta {
            Some(d) => out.push(format!(
                "mailbox irq {} -> {} (delta {d})",
                mailbox_before.unwrap_or(0),
                mailbox_after.unwrap_or(0)
            )),
            None => out.push("mailbox irq: not measured on this platform".to_string()),
        }

        let pass = failures == 0 && m.probe_failures == 0;
        if failures > 0 {
            ctx.note(format!("{failures} probe(s) went unanswered"));
        }
        if mailbox_delta == Some(0) {
            ctx.note("mailbox interrupt count did not move during the run".to_string());
        }

        let outcome = if pass {
            Outcome::pass(out)
        } else {
            Outcome::fail(out)
        };
        Ok(outcome)
    }
}
