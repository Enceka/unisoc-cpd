//! `voice` — the mixer half of a voice call.
//!
//! The CP owns the codec and the audio HAL owns the route; what a control
//! daemon can usefully own is the one switch the vendor RIL itself touches:
//! muting the speaker so an ended call does not leave a pop behind
//! (impl-ril/ril_call.c, `speaker_mute`).  The card, the control and the tool
//! all come from the profile -- this file names none of them -- and when the
//! image carries no mixer tool the action says so instead of pretending to have
//! muted anything, which is the measured state of this image today.

use super::{positionals, Capability, Outcome};
use crate::context::Context;
use anyhow::{bail, Result};
use std::process::Command;

pub struct Voice;

impl Capability for Voice {
    fn name(&self) -> &'static str {
        "voice"
    }

    fn summary(&self) -> &'static str {
        "in-call audio switch (the profile's mixer hook): status | mute | unmute"
    }

    fn native_only(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut Context, args: &[String]) -> Result<Outcome> {
        let pos = positionals(args);
        let action = pos.first().map(|s| s.as_str()).unwrap_or("status");
        // Cloned so the actions can borrow the context mutably afterwards.
        let voice = ctx.profile.voice.clone();
        let Some(card) = voice.card.clone() else {
            bail!(
                "profile {:?} names no [voice].card, so there is no mixer to drive",
                ctx.profile.name
            );
        };
        let Some(control) = voice.control.clone() else {
            bail!(
                "profile {:?} names no [voice].control, so there is no switch to drive",
                ctx.profile.name
            );
        };
        let tool = voice.tool.clone().unwrap_or_else(|| "amixer".into());

        match action {
            "status" => Ok(voice_status(&card, &control, &tool, voice.supported)),
            "mute" => {
                let (out, ok) = set_switch(&tool, &card, &control, "0");
                Ok(outcome(out, ok))
            }
            "unmute" => {
                let (out, ok) = set_switch(&tool, &card, &control, "1");
                Ok(outcome(out, ok))
            }
            other => bail!("voice: unknown action {other:?} (status | mute | unmute)"),
        }
    }
}

fn outcome(lines: Vec<String>, ok: bool) -> Outcome {
    if ok {
        Outcome::pass(lines)
    } else {
        Outcome::fail(lines)
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn run(cmd: &str, args: &[&str]) -> (bool, String) {
    match Command::new(cmd).args(args).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        ),
        Err(e) => (false, format!("{e}")),
    }
}

/// `amixer cget` output -> the switch's first value token (`on`, `off`, …), or
/// none when the answer carries no value line.
///
/// amixer prints two `values=`: the type line's is the *channel count*, the
/// value line's is the state, and only the value line begins with `:` -- which
/// is the difference the reader keys on, after the type line cost a test.
fn switch_value(text: &str) -> Option<String> {
    text.lines()
        .filter(|l| l.trim_start().starts_with(':'))
        .find(|l| l.contains("values="))
        .and_then(|l| l.split("values=").nth(1))
        // A stereo switch reports `on,on`: the first channel is the one the
        // mute is about.
        .map(|v| v.split(',').next().unwrap_or("").trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `cset` takes the same boolean words `cget` reports; `0`/`1` are accepted as
/// the spellings the vendor RIL used.
fn norm(value: &str) -> &str {
    match value {
        "1" => "on",
        "0" => "off",
        other => other,
    }
}

fn cget(tool: &str, card: &str, control: &str) -> (bool, String) {
    let arg = format!("name={control}");
    run(tool, &["-c", card, "cget", arg.as_str()])
}

/// Set the switch, then read it back: a switch is not set until it reads set,
/// which is the same rule the identity write path holds itself to.
fn set_switch(tool: &str, card: &str, control: &str, value: &str) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let argv = format!("{tool} -c {card} cset name={control} {value}");
    let (ok, text) = run(tool, &["-c", card, "cset", &format!("name={control}"), value]);
    if ok {
        out.push(format!("ok   {argv}"));
    } else {
        out.push(format!("FAIL {argv}"));
        if !text.is_empty() {
            out.push(format!("     {}", first_line(&text)));
        }
    }

    let mut ok = ok;
    if ok {
        let (_, back) = cget(tool, card, control);
        match switch_value(&back) {
            Some(v) if v == norm(value) => {
                out.push(format!("     reads back {v}"));
            }
            Some(v) => {
                ok = false;
                out.push(format!("FAIL reads back {v}, not {}", norm(value)));
            }
            None => {
                ok = false;
                out.push("FAIL the switch did not read back".to_string());
            }
        }
    }
    (out, ok)
}

fn voice_status(card: &str, control: &str, tool: &str, supported: bool) -> Outcome {
    let mut out = vec![
        format!("card: {card}"),
        format!("control: {control}"),
        format!("tool: {tool}"),
    ];
    let (_, cards) = run("cat", &["/proc/asound/cards"]);
    if !cards.is_empty() {
        out.push(format!("alsa cards: {}", first_line(&cards)));
    }

    let (ok, text) = cget(tool, card, control);
    if !ok {
        out.push(format!("switch: unreadable ({})", first_line(&text)));
        out.push(
            "voice: the mixer hook is unavailable on this image -- the RIL's own \
             mute call would fail here too"
                .to_string(),
        );
        return Outcome::fail(out);
    }
    match switch_value(&text) {
        Some(v) => out.push(format!("switch: {v}")),
        None => out.push("switch: not reported".to_string()),
    }
    if supported {
        out.push("voice: the full in-call audio path is wired".to_string());
    } else {
        // Saying which half is missing is the point: a muted speaker is not a
        // working voice channel, and the profile knows the difference.
        out.push(
            "voice: the mixer hook is half of the audio path; no UCM route is wired \
             yet ([voice].supported = false)"
                .to_string(),
        );
    }
    Outcome::pass(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_value_is_the_first_one_on_the_values_line() {
        let text = "\
numid=1,iface=MIXER,name='Speaker Playback Switch'
  ; type=BOOLEAN,access=rw------,values=2
  : values=off
";
        assert_eq!(switch_value(text).as_deref(), Some("off"));
        assert_eq!(switch_value("  : values=on,on").as_deref(), Some("on"));
        assert_eq!(switch_value("numid=1,iface=MIXER"), None);
        assert_eq!(switch_value(""), None);
    }

    /// The vendor RIL's spelling (`0`/`1`) and amixer's (`off`/`on`) must mean
    /// the same thing, or the read-back comparison fails every time.
    #[test]
    fn the_two_boolean_spellings_agree() {
        assert_eq!(norm("0"), "off");
        assert_eq!(norm("1"), "on");
        assert_eq!(norm("off"), "off");
    }
}
