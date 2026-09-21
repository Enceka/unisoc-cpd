//! Unsolicited result codes: the half of the control plane that is not an answer.
//!
//! A one-shot capability only ever sees replies.  Everything the network says on
//! its own — a SIM pulled out, a registration lost, a message delivered, a call
//! arriving — arrives as a URC, and a control plane that only answers questions
//! is not one: there is no MT SMS and no incoming call in it.  That is why G2
//! needs a resident owner (`serve`), and why this decoder exists as its own
//! module rather than as a corner of the AT layer.
//!
//! `classify` is deliberately called *behind* the session's own demultiplexer:
//! `core/at.rs` has already decided that the line belongs to the unsolicited
//! stream (it is the only place that can, because `+CSQ:` and `+CEREG:` are both
//! answers and URCs).  This module never second-guesses that decision — it only
//! decodes what it is handed, and keeps what it does not understand.
//!
//! The line shapes are the ones in `docs/BASEBAND-CONTRACTS.md` §9.2; the
//! measured URC dump those shapes come from is §3 of the same document.

use crate::telemetry::iso8601;
use serde::Serialize;

/// Which registration a `+C*REG` URC is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegDomain {
    /// Circuit-switched (`+CREG`).
    Cs,
    /// Packet-switched, GPRS (`+CGREG`).
    Ps,
    /// EPS (`+CEREG`).
    Eps,
    /// 5G (`+C5GREG`) — this generation's own.
    FiveG,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum Urc {
    /// `+SIND: 1`, `+SIND: 10,"SM",1,"FD",1`.
    #[serde(rename = "urc-sim-indication")]
    SimIndication { code: u32, detail: String },
    /// `+CPIN: READY` — the SIM state changed.
    #[serde(rename = "urc-sim-state")]
    SimState { state: String },
    /// `+CREG:`/`+CGREG:`/`+CEREG:`/`+C5GREG:`.  `status` is the second field of
    /// the measured `2,1,…` shape, `act` the fifth (7 = LTE, 11 = NR SA,
    /// 13 = EN-DC).
    #[serde(rename = "urc-registration")]
    Registration {
        domain: RegDomain,
        status: u32,
        act: Option<u32>,
    },
    /// `+CSQ:` or `+CESQ:`; whichever fields the line carried.
    #[serde(rename = "urc-signal")]
    Signal {
        rssi: Option<i32>,
        ber: Option<i32>,
        rsrp: Option<i32>,
        rsrq: Option<f64>,
        sinr: Option<f64>,
    },
    /// `+CGEV: …` — a bearer (PDN) event.
    #[serde(rename = "urc-bearer")]
    Bearer { text: String },
    /// `+CMTI: "SM",3` — a message arrived into storage.  This is the MT SMS
    /// path: nothing else announces an incoming message on this generation.
    #[serde(rename = "urc-new-message")]
    NewMessage { storage: String, index: u32 },
    /// `+CMGW: ME is full`.
    #[serde(rename = "urc-message-storage")]
    MessageStorage { text: String },
    /// `RING` or `+CRING: VOICE`.
    #[serde(rename = "urc-incoming-call")]
    IncomingCall { ring: String },
    /// `+CLIP: "+8613800138000",129,…`.
    #[serde(rename = "urc-caller-id")]
    CallerId {
        number: String,
        address_type: Option<u32>,
    },
    /// `+CLCCS: …` — this generation's per-call state line for VoLTE, parsed
    /// the way the vendor RIL parses it (impl-ril/ril_call.c,
    /// `callFromCLCCLineVoLTE`).  The state is carried verbatim because its
    /// values are the CP's own (1 idle, 2 calling, 5 alerting, 6 active, 12
    /// waiting, 13/14 held) and mapping them into RIL names here would be a
    /// guess until one has been measured on the unit.  On this firmware the
    /// VoLTE path suppresses `+CRING`-driven call-state events, which makes
    /// this line the one place a VoLTE call announces itself.
    #[serde(rename = "urc-call-state")]
    CallState {
        index: u32,
        is_mt: bool,
        media: Option<String>,
        cs_mode: Option<u32>,
        state: Option<u32>,
        mpty: Option<u32>,
        number_type: Option<u32>,
        ton: Option<u32>,
        number: Option<String>,
    },
    /// `+CUSD: 0,"balance 12.34 CNY",15`.
    #[serde(rename = "urc-ussd")]
    Ussd { status: u32, text: String },
    /// `+SPERROR: 14,27,"46001"` — this generation's own error report.
    #[serde(rename = "urc-sp-error")]
    SpError { code: Option<u32>, text: String },
    /// A line on the unsolicited stream this decoder does not know.  Kept
    /// verbatim: "the stream carried something we did not decode" is evidence.
    #[serde(rename = "urc-other")]
    Other { line: String },
}

impl Urc {
    /// The telemetry event kind this URC becomes.
    pub fn kind(&self) -> &'static str {
        match self {
            Urc::SimIndication { .. } => "urc-sim-indication",
            Urc::SimState { .. } => "urc-sim-state",
            Urc::Registration { .. } => "urc-registration",
            Urc::Signal { .. } => "urc-signal",
            Urc::Bearer { .. } => "urc-bearer",
            Urc::NewMessage { .. } => "urc-new-message",
            Urc::MessageStorage { .. } => "urc-message-storage",
            Urc::IncomingCall { .. } => "urc-incoming-call",
            Urc::CallState { .. } => "urc-call-state",
            Urc::CallerId { .. } => "urc-caller-id",
            Urc::Ussd { .. } => "urc-ussd",
            Urc::SpError { .. } => "urc-sp-error",
            Urc::Other { .. } => "urc-other",
        }
    }

    /// One human-readable line, the way a run summary records an event.
    pub fn detail(&self) -> String {
        match self {
            Urc::SimIndication { code, detail } => {
                if detail.is_empty() {
                    format!("+SIND {code}")
                } else {
                    format!("+SIND {code}: {detail}")
                }
            }
            Urc::SimState { state } => format!("SIM {state}"),
            Urc::Registration {
                domain,
                status,
                act,
            } => {
                let domain = match domain {
                    RegDomain::Cs => "CS",
                    RegDomain::Ps => "PS",
                    RegDomain::Eps => "EPS",
                    RegDomain::FiveG => "5G",
                };
                match act {
                    Some(act) => format!("{domain} registration {status} (act {act})"),
                    None => format!("{domain} registration {status}"),
                }
            }
            Urc::Signal {
                rssi,
                ber,
                rsrp,
                rsrq,
                sinr,
            } => {
                let mut parts = Vec::new();
                if let Some(v) = rssi {
                    parts.push(format!("rssi {v}"));
                }
                if let Some(v) = ber {
                    parts.push(format!("ber {v}"));
                }
                if let Some(v) = rsrp {
                    parts.push(format!("rsrp {v} dBm"));
                }
                if let Some(v) = rsrq {
                    parts.push(format!("rsrq {v:.1} dB"));
                }
                if let Some(v) = sinr {
                    parts.push(format!("sinr {v:.1} dB"));
                }
                if parts.is_empty() {
                    "signal".to_string()
                } else {
                    format!("signal {}", parts.join(", "))
                }
            }
            Urc::Bearer { text } => format!("bearer {text}"),
            Urc::NewMessage { storage, index } => {
                format!("new message in {storage} at index {index}")
            }
            Urc::MessageStorage { text } => format!("message storage: {text}"),
            Urc::IncomingCall { ring } => format!("incoming call ({ring})"),
            Urc::CallState {
                index,
                is_mt,
                media,
                state,
                number,
                ..
            } => {
                let dir = if *is_mt { "MT" } else { "MO" };
                let number = number
                    .as_ref()
                    .map(|n| format!(" {n}"))
                    .unwrap_or_default();
                format!(
                    "+CLCCS idx={index} dir={dir} media={} state={}{}",
                    media.as_deref().unwrap_or("-"),
                    state.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
                    number
                )
            }
            Urc::CallerId { number, .. } => format!("caller id {number}"),
            Urc::Ussd { status, text } => format!("USSD {status}: {text}"),
            Urc::SpError { code, text } => match code {
                Some(code) => format!("SPERROR {code}: {text}"),
                None => format!("SPERROR: {text}"),
            },
            Urc::Other { line } => format!("undecoded: {line}"),
        }
    }

    /// Decoded, as opposed to merely kept.
    pub fn is_decoded(&self) -> bool {
        !matches!(self, Urc::Other { .. })
    }
}

/// A decoded URC with the moment it was seen.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UrcEvent {
    pub at: String,
    pub urc: Urc,
}

impl UrcEvent {
    pub fn new(urc: Urc) -> Self {
        Self {
            at: iso8601(now_secs()),
            urc,
        }
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Everything after the first `:`, trimmed.
fn body(line: &str) -> &str {
    line.split_once(':').map(|(_, b)| b.trim()).unwrap_or("")
}

fn field(s: &str, i: usize) -> Option<&str> {
    s.split(',').nth(i).map(str::trim)
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim().to_string()
}

fn as_u32(s: Option<&str>) -> Option<u32> {
    s?.trim().parse().ok()
}

fn as_i32(s: Option<&str>) -> Option<i32> {
    s?.trim().parse().ok()
}

fn registration(line: &str, domain: RegDomain) -> Urc {
    let b = body(line);
    let parts: Vec<&str> = b.split(',').map(str::trim).collect();
    // With `<n>` present the status is the second field (`2,1,…`); without it,
    // the line is just the status.  The AcT is the fifth field of the shape this
    // generation was measured to send.
    let (status, act) = if parts.len() >= 2 {
        (
            as_u32(parts.get(1).copied()).unwrap_or(0),
            as_u32(parts.get(4).copied()),
        )
    } else {
        (as_u32(parts.first().copied()).unwrap_or(0), None)
    };
    Urc::Registration {
        domain,
        status,
        act,
    }
}

fn signal(line: &str) -> Urc {
    let b = body(line);
    if line.starts_with("+CESQ:") {
        let cesq = crate::capability::control::decode_cesq(line);
        let none = (None, None, None);
        let (rsrp, rsrq, sinr) = match cesq {
            Some(c) => (c.rsrp, c.rsrq, c.sinr),
            None => none,
        };
        Urc::Signal {
            rssi: as_i32(field(b, 0)),
            ber: as_i32(field(b, 1)),
            rsrp,
            rsrq,
            sinr,
        }
    } else {
        Urc::Signal {
            rssi: as_i32(field(b, 0)),
            ber: as_i32(field(b, 1)),
            rsrp: None,
            rsrq: None,
            sinr: None,
        }
    }
}

fn ussd(line: &str) -> Urc {
    let b = body(line);
    let status = as_u32(field(b, 0)).unwrap_or(0);
    // The text is quoted and may itself contain commas, so it is delimited by
    // the first and last quote rather than by the next comma.
    let text = match (b.find('"'), b.rfind('"')) {
        (Some(first), Some(last)) if last > first => unquote(&b[first..=last]),
        _ => String::new(),
    };
    Urc::Ussd { status, text }
}

/// Decode one line of the unsolicited stream.
///
/// `None` means "this is not URC material at all" (a final result code, an empty
/// line); anything shaped like a URC comes back as a variant, `Urc::Other`
/// included, so a caller can never mistake "we decoded it" for "there was
/// nothing there".
/// `+CLCCS:` — the field order the vendor parser reads: index, direction,
/// negotiation pair, media, mode, state, mpty, then the optional number block.
/// Everything past the number is left alone: the vendor parser reads a
/// `localHold` further out, but its position in this generation's answer has
/// not been measured, and a guessed index would read a priority as a boolean.
fn call_state(line: &str) -> Urc {
    let b = body(line);
    let number = field(b, 10).map(unquote).filter(|n| !n.is_empty());
    Urc::CallState {
        index: as_u32(field(b, 0)).unwrap_or(0),
        is_mt: field(b, 1).map(|f| f.trim() == "1").unwrap_or(false),
        media: field(b, 4).map(unquote).filter(|m| !m.is_empty()),
        cs_mode: as_u32(field(b, 5)),
        state: as_u32(field(b, 6)),
        mpty: as_u32(field(b, 7)),
        number_type: as_u32(field(b, 8)),
        ton: as_u32(field(b, 9)),
        number,
    }
}

pub fn classify(line: &str) -> Option<Urc> {
    let l = line.trim();
    if l.is_empty() {
        return None;
    }
    // A final result code is not URC material, and `core/at.rs` is the only
    // place that definition exists: `+CME ERROR: 3` starts with a `+` and has a
    // colon, so the shape check below would otherwise call it `other`.
    if crate::at::classify(l).is_some() {
        return None;
    }
    // `RING` is the one URC with no prefix at all.
    if l == "RING" {
        return Some(Urc::IncomingCall {
            ring: "RING".into(),
        });
    }
    if !(l.starts_with('+') || l.starts_with('^')) {
        return None;
    }
    // A URC's name is everything up to its first colon; a `+XXX` line with no
    // colon at all is kept as `other` rather than guessed at.
    let Some(colon) = l.find(':') else {
        return Some(Urc::Other {
            line: l.to_string(),
        });
    };
    let name = &l[..=colon];
    let b = body(l);

    Some(match name {
        "+SIND:" => Urc::SimIndication {
            code: as_u32(field(b, 0)).unwrap_or(0),
            detail: b
                .split_once(',')
                .map(|(_, rest)| rest.trim().to_string())
                .unwrap_or_default(),
        },
        "+CPIN:" => Urc::SimState {
            state: b.to_string(),
        },
        "+CREG:" => registration(l, RegDomain::Cs),
        "+CGREG:" => registration(l, RegDomain::Ps),
        "+CEREG:" => registration(l, RegDomain::Eps),
        "+C5GREG:" => registration(l, RegDomain::FiveG),
        "+CSQ:" | "+CESQ:" => signal(l),
        "+CGEV:" => Urc::Bearer {
            text: b.to_string(),
        },
        "+CMTI:" => Urc::NewMessage {
            storage: unquote(field(b, 0).unwrap_or("")),
            index: as_u32(field(b, 1)).unwrap_or(0),
        },
        "+CMGW:" => Urc::MessageStorage {
            text: b.to_string(),
        },
        "+CRING:" => Urc::IncomingCall {
            ring: unquote(b).to_uppercase(),
        },
        "+CLCCS:" => call_state(l),
        "+CLIP:" => Urc::CallerId {
            number: unquote(field(b, 0).unwrap_or("")),
            address_type: as_u32(field(b, 1)),
        },
        "+CUSD:" => ussd(l),
        "+SPERROR:" => Urc::SpError {
            code: as_u32(field(b, 0)),
            text: b.to_string(),
        },
        _ => Urc::Other {
            line: l.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line the measured URC dump carried (contracts §3) must at least be
    /// kept, and the ones G2 depends on must be decoded.
    #[test]
    fn the_measured_dump_decodes() {
        for (line, want) in [
            ("+SIND: 1", "urc-sim-indication"),
            ("+SIND: 10,\"SM\",1,\"FD\",1", "urc-sim-indication"),
            ("+CREG: 2", "urc-registration"),
            ("+CEREG: 2", "urc-registration"),
            ("+CSQ: 255,99", "urc-signal"),
            ("+CESQ: 99,99,255,255,255,255,75,67,73", "urc-signal"),
            ("+CGEV: ME PDN ACT 1", "urc-bearer"),
            ("+SPPCODATA: 1", "urc-other"),
            ("+CMGW: ME is full", "urc-message-storage"),
            ("+PRENWINFU:\"46001\"", "urc-other"),
            ("+ECIND: 3,0,0,1", "urc-other"),
        ] {
            let got = classify(line).unwrap_or_else(|| panic!("{line} was dropped"));
            assert_eq!(got.kind(), want, "{line} -> {got:?}");
        }
    }

    #[test]
    fn a_final_code_is_not_urc_material() {
        assert_eq!(classify("OK"), None);
        assert_eq!(classify("ERROR"), None);
        assert_eq!(classify("+CME ERROR: 3"), None);
        assert_eq!(classify(""), None);
        assert_eq!(classify("   "), None);
    }

    #[test]
    fn registration_carries_the_status_and_the_act() {
        let u = classify("+CEREG: 2,1,\"DE0400\",\"005BE001\",11").unwrap();
        assert_eq!(
            u,
            Urc::Registration {
                domain: RegDomain::Eps,
                status: 1,
                act: Some(11),
            }
        );
        // The short form the modem also sends: just the status.
        let u = classify("+CEREG: 2").unwrap();
        assert_eq!(
            u,
            Urc::Registration {
                domain: RegDomain::Eps,
                status: 2,
                act: None,
            }
        );
        assert!(matches!(
            classify("+CREG: 2,1,\"DE04\",\"005BE001\",7").unwrap(),
            Urc::Registration {
                domain: RegDomain::Cs,
                act: Some(7),
                ..
            }
        ));
        assert!(matches!(
            classify("+C5GREG: 2,1,\"DE0400\",\"005BE001\",11").unwrap(),
            Urc::Registration {
                domain: RegDomain::FiveG,
                status: 1,
                ..
            }
        ));
    }

    #[test]
    fn cesq_decodes_the_same_way_the_signal_capability_does() {
        // index 60 -> -80 dBm, index 20 -> -9.5 dB
        let u = classify("+CESQ: 99,99,255,255,20,60,75,67,73").unwrap();
        assert_eq!(
            u,
            Urc::Signal {
                rssi: Some(99),
                ber: Some(99),
                rsrp: Some(-89),
                rsrq: Some(-5.5),
                sinr: Some(-23.0 + 73.0 / 2.0),
            }
        );
    }

    #[test]
    fn csq_carries_the_raw_index() {
        let u = classify("+CSQ: 23,99").unwrap();
        assert_eq!(
            u,
            Urc::Signal {
                rssi: Some(23),
                ber: Some(99),
                rsrp: None,
                rsrq: None,
                sinr: None,
            }
        );
    }

    #[test]
    fn a_new_message_is_the_mt_sms_signal() {
        let u = classify("+CMTI: \"SM\",3").unwrap();
        assert_eq!(
            u,
            Urc::NewMessage {
                storage: "SM".into(),
                index: 3,
            }
        );
        assert_eq!(u.detail(), "new message in SM at index 3");
    }

    /// The VoLTE per-call line, in the shape the vendor parser reads: the
    /// number is optional and everything past it is left alone.
    #[test]
    fn a_clccs_line_is_decoded_verbatim() {
        let u = classify(
            "+CLCCS: 1,1,0,0,\"audio\",0,6,0,145,129,\"+8613800138000\",0,0,0,0,1",
        )
        .unwrap();
        assert_eq!(u.kind(), "urc-call-state");
        assert_eq!(
            u,
            Urc::CallState {
                index: 1,
                is_mt: true,
                media: Some("audio".into()),
                cs_mode: Some(0),
                state: Some(6),
                mpty: Some(0),
                number_type: Some(145),
                ton: Some(129),
                number: Some("+8613800138000".into()),
            }
        );
        assert_eq!(
            u.detail(),
            "+CLCCS idx=1 dir=MT media=audio state=6 +8613800138000"
        );
    }

    /// A call with no number yet (an early MT state) keeps the field absent,
    /// and the detail line does not end with a dangling separator.
    #[test]
    fn a_clccs_line_without_a_number_is_kept() {
        let u = classify("+CLCCS: 2,1,0,0,\"audio\",0,12,0").unwrap();
        match &u {
            Urc::CallState {
                index,
                is_mt,
                state,
                number,
                ..
            } => {
                assert_eq!(*index, 2);
                assert!(*is_mt);
                assert_eq!(*state, Some(12));
                assert!(number.is_none());
            }
            _ => panic!("not a call state"),
        }
        assert_eq!(u.detail(), "+CLCCS idx=2 dir=MT media=audio state=12");
    }

    #[test]
    fn an_incoming_call_is_recognised_with_and_without_a_prefix() {
        assert_eq!(
            classify("RING").unwrap(),
            Urc::IncomingCall {
                ring: "RING".into()
            }
        );
        assert_eq!(
            classify("+CRING: VOICE").unwrap(),
            Urc::IncomingCall {
                ring: "VOICE".into()
            }
        );
        let u = classify("+CLIP: \"+8613800138000\",129,,,,0").unwrap();
        assert_eq!(
            u,
            Urc::CallerId {
                number: "+8613800138000".into(),
                address_type: Some(129),
            }
        );
    }

    #[test]
    fn ussd_text_survives_a_comma_in_the_answer() {
        let u = classify("+CUSD: 0,\"balance 12.34, CNY\",15").unwrap();
        assert_eq!(
            u,
            Urc::Ussd {
                status: 0,
                text: "balance 12.34, CNY".into(),
            }
        );
        // A network notification with no text of its own is still an event.
        let u = classify("+CUSD: 2,\"\",15").unwrap();
        assert_eq!(
            u,
            Urc::Ussd {
                status: 2,
                text: String::new(),
            }
        );
    }

    #[test]
    fn inline_delivery_is_kept_but_not_claimed_as_decoded() {
        // `+CMT:` only happens once `AT+CNMI` asks for inline delivery, which
        // this generation's measured path does not; it must not look decoded.
        let u = classify("+CMT: ,25").unwrap();
        assert_eq!(u.kind(), "urc-other");
        assert!(!u.is_decoded());
        assert!(matches!(u, Urc::Other { line } if line == "+CMT: ,25"));
    }

    #[test]
    fn a_sim_indication_keeps_the_detail_after_the_code() {
        let u = classify("+SIND: 10,\"SM\",1,\"FD\",1").unwrap();
        assert_eq!(
            u,
            Urc::SimIndication {
                code: 10,
                detail: "\"SM\",1,\"FD\",1".into(),
            }
        );
        assert_eq!(classify("+SIND: 1").unwrap().detail(), "+SIND 1");
    }

    /// The JSON `kind` and the telemetry event kind must be the same word: one
    /// summary holds both, and two names for one event is how a reader ends up
    /// counting it twice.
    #[test]
    fn the_serialised_kind_is_the_telemetry_kind() {
        let samples = [
            Urc::SimIndication {
                code: 1,
                detail: String::new(),
            },
            Urc::SimState {
                state: "READY".into(),
            },
            Urc::Registration {
                domain: RegDomain::Eps,
                status: 1,
                act: None,
            },
            Urc::Signal {
                rssi: None,
                ber: None,
                rsrp: None,
                rsrq: None,
                sinr: None,
            },
            Urc::Bearer {
                text: String::new(),
            },
            Urc::NewMessage {
                storage: "SM".into(),
                index: 1,
            },
            Urc::MessageStorage {
                text: String::new(),
            },
            Urc::IncomingCall {
                ring: "RING".into(),
            },
            Urc::CallerId {
                number: String::new(),
                address_type: None,
            },
            Urc::Ussd {
                status: 0,
                text: String::new(),
            },
            Urc::SpError {
                code: None,
                text: String::new(),
            },
            Urc::Other {
                line: String::new(),
            },
        ];
        for urc in samples {
            let json = serde_json::to_value(&urc).unwrap();
            assert_eq!(json["kind"], urc.kind(), "{urc:?}");
        }
    }

    #[test]
    fn the_generations_own_error_report_is_decoded() {
        let u = classify("+SPERROR: 14,27,\"46001\"").unwrap();
        assert_eq!(
            u,
            Urc::SpError {
                code: Some(14),
                text: "14,27,\"46001\"".into(),
            }
        );
    }
}
