//! Raw AT through the channel owner.
//!
//! UFI-TOOLS' AT page asks for an arbitrary command, and the one place it can be
//! run is here: the daemon owns /dev/stty_nr1, paces it and serialises it, so a
//! client that asks by name keeps the one-reader rule intact.  A second process
//! poking the tty is exactly what the plan forbids -- and what the CP punishes
//! with "The queue was full".
//!
//! Output is deliberately bare: the response lines and the final code, nothing
//! prefixed, so a caller that parses AT (UFI-TOOLS strips AT echoes) does not
//! have to undress our formatting first.  `/opt/e5/e5-at` is the one-line shell
//! client for it, and the profile's `at_command` points UFI-TOOLS there.

use super::{positionals, Capability, Outcome};
use crate::context::Context;
use anyhow::{bail, Result};
use std::time::Duration;

pub struct At;

impl Capability for At {
    fn name(&self) -> &'static str {
        "at"
    }

    fn summary(&self) -> &'static str {
        "run one AT command through the channel owner (bare response lines)"
    }

    fn native_only(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let cmd = positionals(args).join(" ");
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            bail!("at: no command given (usage: unisoc-cpd --socket <sock> at \"AT+CSQ\")");
        }

        let session = ctx.at()?;
        let reply = session.command(&cmd, Duration::from_secs(8), &[], 0);

        let mut out = reply.lines.clone();
        out.push(reply.final_code.as_str());
        if reply.ok() {
            Ok(Outcome::pass(out))
        } else {
            Ok(Outcome::fail(out))
        }
    }
}
