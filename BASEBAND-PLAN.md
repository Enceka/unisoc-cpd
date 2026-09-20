# Plan: a device-independent control daemon for the Unisoc CP (baseband)

The AP-side contract is a property of the **baseband generation**, not of the board:
the SIPC channels, the AT/URC dialect, the CP boot handshake, the log/dump spools, the
time-sync channel and the mailbox quirks are the same wherever this CP family is
soldered on.  So the deliverable is a daemon that drives an Unisoc CP through a
**platform profile**; the E5 is the first test bench, and mu300-linux (same CP family,
port already exists) is the second.  Nothing in the core may be named after a board.

Working name: `unisoc-cpd` (placeholder -- rename freely; the plan only needs a stable
name for the binary and the service unit).

## 1. What already exists, and which part of it is generic

| asset | generic idea | e5-specific values (must live in the profile) |
| --- | --- | --- |
| chroot runner + shims (`android-run`, `logdw.py`, dev-node ownership, `comm` trick) | run vendor boot binaries on Linux | paths, the binary name, the loader's `comm` expectation |
| vendor bootstrap services (`modem_control`, `cp_diskserver`, `refnotify`) | CP boot, NV persistence, reference clock | which binaries, which partitions |
| AT broker (`atd.py`): one owner of the URC tty + the command tty | channel ownership, URC always drained, paced commands | `/dev/stty_nr0` / `/dev/stty_nr1` |
| bearer (`mobile-data`) | PDP bring-up, backoff, storm guard | APN source, interface name (`sipa_eth0`) |
| CP watchdog | AT-silence detection -> log/reboot, guard interval | thresholds |
| kernel side | SIPC, mailbox HAL, `slog_bridge`, `sprd_cp_dump`, `sipa` | partition names, module list, PMU/boot registers |

## 2. Objectives

* **G1 -- CP link stable.** 72 h with our daemon as the only AT/URC owner: zero CP
  asserts, zero AT silence, CP log continuity, mailbox interrupts still counting.
* **G2 -- Control plane native.** SIM/PIN, registration/signal, operator/band, SMS
  (MO/MT), USSD, `CFUN`, served by the daemon, answers matching Android on the same
  SIM and network.
* **G3 -- Data native.** Dual-stack PDP, data interface up, routing/DNS/NAT, backoff
  reconnect, suspend/resume, verified by counters and throughput.
* **G4 -- Voice CS native (network permitting).** MO/MT, DTMF, in-call audio through
  the platform's voice audio path, 5 min two-way.
* **G5 -- Side contracts ours.** CP log/dump spools drained by us, time-sync channel
  understood and served if required, NV persisted, mailbox silent-stop fixed.
* **G6 -- Android dependency removed.** With every vendor daemon disabled except the
  single CP-boot step the plan keeps, G1-G5 still hold.
* **G7 -- Portability.** The same binary passes A1-A11 on a second platform of the same
  CP family **by adding a profile only** (no core code change, no board `#ifdef`).

Non-goals / red lines:
**never two readers on one AT channel**.

## 3. Architecture: core + profile + hooks

```
unisoc-cpd
 |-- core/channel      owns the channel set; always drains; health counters
 |-- core/at           AT codec, URC demux, serialisation, pacing, retry, timeouts
 |-- core/capability   sim, register, data, sms, call, ussd, band, cfun, nv, diag
 |-- core/api          CLI first (unisoc-cpd <capability>), D-Bus later for MM/NM/Phosh
 |-- core/telemetry    per-run JSON: assert count, URC gaps, mailbox IRQ deltas,
 |                     data counters, AT request/response counts, PDP/call events
 |-- platform/         profile loading + optional hooks
      profiles/e5.toml        <-- the only file that knows about the E5
      profiles/mu300.toml     <-- second platform, same core
```

Each capability keeps two modes, `vendor` (still done by the vendor daemon) and
`native` (ours); the same acceptance test must pass in both before the vendor side is
switched off.

## 4. The portability boundary (exactly what a profile must express)

| profile field | e5 value (example) | why it is platform-specific |
| --- | --- | --- |
| `channels.urc` / `channels.cmd` | `/dev/stty_nr0`, `/dev/stty_nr1` | SIPC tty numbering |
| `channels.log` / `channels.dump` | `slog_bridge`, `sprd_cp_dump` | spool channel names |
| `channels.stime` | `/dev/stime_ch` | optional on some boards |
| `boot.method` | chroot `modem_control` | vendor loader protocol |
| `boot.partitions` | `nr_modem_a/b`, `nr_phy_*`, `nr_fixnv1_*` | partition naming and A/B slotting |
| `nv.persist` | `cp_diskserver` in chroot | NV layout differs per product |
| `mailbox` | `sprd-mailbox`, IRQ lines `64600000.mailbox` | SoC interrupt wiring |
| `data.ifname`, `data.apn_source` | `sipa_eth0`, APN from SIM/AT | data path is board/SDK specific |
| `voice.mixer[]` | VBC voice route + codec switches | audio routing is board specific |
| `telemetry.paths` | `/proc/interrupts`, `/sys/class/net/sipa_eth0/` | counters differ |

Hooks (small platform modules, only where data is not enough): voice audio routing,
NV persistence, CP boot.  Everything else must be data.

## 5. Workstreams

| # | workstream | concrete tasks | gate |
| --- | --- | --- | --- |
| W0 | contract capture | vendor-side traces (radio log, per-daemon open files, telephony dumps, in-call mixer) + the same probes on the Linux side; write the channel/command/timing tables into `docs/BASEBAND-CONTRACTS.md` | every capability has a written contract |
| W1 | channel layer + CP link | one-owner AT transport with pacing and URC drain, log/dump spool drains, mailbox fix, `stime_ch` investigation, telemetry | **72 h: 0 asserts, 0 silence** |
| W2 | control plane | SIM/PIN, registration, signal, operator/band, SMS, USSD from W0's sequences; API | matches Android |
| W3 | data | PDP v4/v6, interface bring-up, route/DNS/NAT, backoff, suspend resume | dual-stack soak + counters align |
| W4 | voice CS | call control + platform voice hook + `callaudiod`/`Calls` | 5 min two-way call |
| W5 | VoLTE/IMS (optional) | probe whether the vendor IMS binaries run under the runner | "works" or an explicit "not feasible here" |
| W6 | diagnostics/management | log decode, crash collection, NV backup, CP restart, power/thermal | CP restart without reboot |
| W7 | integration/UX | desktop status/PIN/airplane mode, packaging for a distro | usable from the session |
| W8 | portability | second platform profile (mu300), no core changes; `profile-check` tool | A12 passes |

## 6. Milestones

* **M0**: W0 done; telemetry harness running on the first platform.
* **M1**: W1 landed, 72 h soak passes on the first platform -- the gate for everything else.
* **M2**: W2 + W3: registration, SMS, data with the vendor AT path disabled.
* **M3**: W4 voice CS, if the network allows CS.
* **M4**: W6/W7 and a 7-day soak with all vendor daemons off except CP boot.
* **M5 (portability)**: W8 -- same binary, second platform, **profile only**.

## 7. Acceptance matrix

Every row runs in `vendor` mode (baseline) and `native` mode (ours); Android on the
same device/SIM/network is the oracle.

| # | capability | run | pass criteria |
| --- | --- | --- | --- |
| A1 | CP link | 72 h idle + periodic signal query | 0 asserts, URC gap < 5 s, mailbox IRQ counters increase, no CP reset |
| A2 | SIM/PIN | boot with PIN-locked SIM | state matches Android; wrong-PIN retries counted |
| A3 | registration | boot + polling | registered in Android's time, signal within 3 dB of Android |
| A4 | operator/band | scan, band lock/unlock | same network list, lock survives reboot |
| A5 | SMS | MO/MT against a fixed number | both delivered, no duplicates, sane timestamps |
| A6 | USSD | a balance query | answer equals Android's |
| A7 | data | PDP up, throughput test, 30 min | within 20 % of Android, no drop, counters move |
| A8 | reconnect | airplane-mode cycles | bearer back < 60 s, storm counter stays 0 |
| A9 | voice CS | MO/MT call | call completes, DTMF works, 5 min two-way audio |
| A10 | side contracts | 1 h | log/dump spools never full, `stime_ch` documented or implemented |
| A11 | no vendor | vendor daemons off except CP boot | A1-A9 still pass |
| A12 | portability | same binary, second platform | A1-A11 pass with a profile only; `profile-check` reports no core change |

## 8. Test rig

* One rig per platform, same harness: `unisoc-cpd mode vendor|native <capability>`
  switches a single capability; every run writes `runs/<platform>/<date>/summary.json`
  (assert count, URC gaps, mailbox IRQ deltas, data counters, AT counts, PDP/call
  events) so two platforms and two modes are directly comparable.
* Oracle runs: the same functional tests on Android with the vendor radio log
  captured, so a failure diffs command by command.
* Soak: systemd timer + the CP watchdog in `ACTION=log` while measuring (a watchdog
  reboot would hide the evidence), plus the platform's persistent log block for
  post-mortem.
* Rule: nothing on a test device is left changed after a run unless the run's commit
  says so.

## 9. Risks and stop conditions

| risk | mitigation | stop condition |
| --- | --- | --- |
| the CP expects a contract we cannot reimplement (time sync, keepalive) | W0/W1 first, Android as oracle | if the CP still stops with all spools drained, re-scope to "AT+data" and document |
| network is VoLTE-only | probe CS availability early | voice moves to W5 (IMS) or out of scope |
| hidden dependency on a vendor daemon | keep per-capability `vendor` mode; measure | document it as a permanent dependency |
| AT pacing/assert regression | pacing in the core + storm counter + surge test | > 1 assert in 72 h blocks the milestone |
| platform difference leaks into the core | `profile-check` + the A12 gate | any board `#ifdef`/hard-coded path fails the gate |
| IMEI/NV accident | NV read-only, no IMEI tooling | any NV write request stops the workstream |

## 10. First week

1. Capture the vendor side on the first platform (radio log, per-daemon open files,
   telephony dumps, in-call mixer) and the same probes on the Linux side; produce the
   differential table and `docs/BASEBAND-CONTRACTS.md`.
2. Land W1 (channel layer with the mailbox fix and the spool drains) and start the
   72 h soak.
3. Write `profiles/e5.toml` first, and immediately a second stub profile, so the core
   is built against two profiles from day one.
4. Skeleton of the core: channel + at + telemetry only, with the `vendor|native`
   switch and the run-summary writer.
