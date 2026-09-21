# FINDINGS

The measured record of this repository's own work: what the daemon's sessions
on the bench taught, one numbered section per lesson, added as sessions
produce them.  Reasoning and dead ends stay: a withdrawn reading is kept
marked as withdrawn, because the next session will otherwise rediscover it.

Discipline, carried over from the plan: Android on the same SIM and network is
the oracle; every number below was read off the device, not inferred; nothing
on a test device is left changed unless the run's commit says so.  The bench is
an E5 handset of this CP generation, the plan's first platform.  Section 12's
measurements predate the daemon and were made through the AT broker it
replaced, on the same silicon; they are kept because every watchdog here keys
on them.

## 1. The first takeover (G2): the daemon as the only reader of the AT channel

_2026-09-20, handset booted into Android (slot a), rooted, `urild` the
incumbent owner.  Everything below was done over `adb` with `su`; the binary is
the static `aarch64-unknown-linux-musl` build pushed beside the e5 profile.
The session ran with `vendor.modem_control` left alone — the CP stays booted
when only the RIL is stopped._

### 1.1 Who holds the channel, before and after

A `/proc/*/fd` scan for `stty_nr0/nr1` is the honest statement of ownership:

* with Android up: exactly one holder, `/vendor/bin/hw/urild`
  (`init.svc.vendor.ril-daemon`); `slogmodem` runs but holds only `slog_*`;
* after `stop vendor.ril-daemon`: **no holder at all** — the channel is free,
  and the takeover is a plain open, not a race.

### 1.2 What worked, first try

As the only reader, every probe and every capability answered:

* `link --seconds 30 --interval 10`: 3/3 probes OK at ~200 ms, **0 timeouts,
  0 errors**, 16 URC lines with `max_gap 0.0 s`, mailbox IRQ delta 41;
* `sim`: `+CPIN: READY` — matches Android's `gsm.sim.state = LOADED,LOADED`;
* `serve` (100 s) + a client over the socket: `sim` and `register status`
  answered through the daemon, and `state` reported `channels.cmd.opens: 1`,
  `reopens: 0` — one open for the whole window, which is the entire point of
  the resident owner;
* the socket `urc` query returned decoded events — the CP pairs a `+CSQ` and a
  `+CESQ` URC roughly twice a second once registered — and the decoder read
  **50 of 50 lines** (`urc_lines 50, decoded 50` — 100 %, no `urc-other`);
* `band lock lte 1 41` / `band lock nr 41 78` both took and read back exactly
  (`+SPLBAND=0` → `0,256,0,1,0`, `+SPLBAND=3` → `0,0,272`);
* 0 CP asserts from beginning to end of the session.

### 1.3 The transfer rule

Restoring the vendor side, `start vendor.ril-daemon` was issued while the 100 s
`serve` was still alive: the fd scan then showed **the daemon and `urild`
holding the channels at the same time** — the plan's only red line, violated by
sequencing, not by the code (the flock is advisory, and `urild` never takes
it; it only coordinates this daemon's own instances).  Nothing broke — and the
session had one clean piece of evidence that the daemon *noticed*: its idle
probe failed exactly once, in that window.  The rule for every future
transfer, in both directions:

> **never `start` the other owner until our daemon has exited and the fd scan
> shows the channel free; never `serve` past the point the other side is told
> to start.**  Verify with the `/proc/*/fd` scan, not with an assumption.

### 1.4 Nothing left changed

The band experiment was reverted before the RIL came back: LTE re-locked to the
RIL's own set (read back `+SPLBAND: 0,482,2056,213,0` = bands
1,3,5,7,8,20,28,34,38,39,40,41) and NR unlocked (`+SPLBAND=2,0,0,0,0`, read
back `(none)`).  After `start vendor.ril-daemon`: `LOADED,LOADED`,
`46015,46001`, `NR_SA,LTE` — identical to the pre-session baseline — and the
closing `diag asserts` read 0.

## 2. Stack bring-up after a RIL shutdown: the SFUN pair is not enough

Stopping the RIL does not leave the modem runnable: its shutdown path parks the
radio at **`+CFUN: 0`** (`+CEREG: 2,0`, `+CSQ: 0,99`, every `+CESQ` field 255).
The recovery the contract already carried — `AT+SFUN=2`, `AT+SFUN=4` — sets
`+CFUN: 1` but **does not register**: five minutes of waiting stayed at
`+CEREG: 2,0` with no RF, and band locking (LTE b1/b41, NR n41/n78) changed
nothing.  What works is the full cold cycle the `cfun cold` capability now
runs:

    AT+CFUN=0 ; then AT+SFUN=2, AT+SFUN=4   →   75 s later:

    +CEREG: 2,1,"10002B","00592002",11      PS registered, home, AcT 11 = NR SA
    +CGATT: 1                               attached

— matching the Android oracle for that SIM (46015 广电, NR_SA).  The measurable
conclusion: **after a RIL shutdown, `SFUN=2/4` alone is not stack bring-up; the
`CFUN=0 → SFUN=2/4` cold cycle is.**  A CP that boots without a RIL at all
registers after the plain `SFUN` pair (the Linux-side sessions of §12), so it
is the *RIL-shutdown state* that needs the cold cycle, not the generation.
`cfun on` still tries the cheap pair first and only escalates to the cold
cycle when registration does not follow.

## 3. `255` in `+CESQ` is "not reported", not "-115 dBm"

The unregistered CP answers `+CESQ: 99,99,255,255,255,255,…`, and the literal
`idx-140` mapping turned 255 into "RSRP 115 dBm" — a nonsense number a reader
will believe.  `decode_cesq` returns `None` for a 255 field and the display
says `not reported`; a *reported* field still decodes (the same line's
SS-SINR 73 → 26.5 dB).

## 4. The SMS surface the RIL leaves hostile: `serve` re-arms it, and MT works end to end

_2026-09-20, same setup as §1, `serve` held open for the whole session with
clients on its socket._

The surface the daemon needs before a `+CMTI:` means anything was left
hostile by the RIL: `+CSCS: "HEX"` (under which `CMGS="<number>"` is not a
phone number) and `+CNMI: 0,0,0,1,0` (mt=0 — new messages are stored *without*
announcing them, so the `+CMTI:` path never starts).  `serve` now sets
`CMGF=1`, `CSCS="GSM"` and `CNMI=2,1,0,0,0` once at start-up and reports all
three in `state` (`text_mode`, `charset_gsm`, `mt_indication`).

**MT works end to end.**  A message sent to SIM1 announced itself with
`+CMTI: "SM",1`; the daemon read it between requests with `AT+CMGR=1`, and a
client saw sender, status, service-centre timestamp and body — the body
arriving as UCS2 hex (`"6D4B8BD5"` = 测试) under `CSCS="GSM"`, which the
daemon decodes to UTF-8.  Reading moved the message to `REC READ`, and
nothing was ever deleted: deletion stays with the operator, because a daemon
that tidies storage can destroy the evidence of its own misreading.

What the sink does when the announcement lands inside another command's reply
loop is §10.

## 5. The MO blockade was one octet of our own PDU

The MO SMS blockade: text-mode submit `+CMS ERROR: 313`; PDU mode
`+CMS ERROR: 302` on two different subscriptions, national and international
destinations alike — always with the CP registered on NR SA.  The whole
blockade was the submit's first octet: the encoder wrote **`0x11`**
(TP-VPF = relative), which promises a TP-VP octet between DCS and UDL that the
encoder never carried — so the CP read DCS as the VP, the UDL as the DCS, and
refused the result as operation not allowed.  The vendor's own submit,
captured in the radio log during a successful `IMS_SEND_SMS` as
`RIL-AT: AT> 0001000B…<pdu>^Z`, uses **`0x01`** (no validity period) and is
otherwise byte-for-byte the shape this encoder already produced (empty SMSC
field, TOA `0x81` national address, DCS `0x08` UCS2).

With the octet fixed the daemon's submit was accepted (`+CMGS: <mr>`, `OK`)
and the message arrived at the recipient, verified end to end on a second
subscription.  The earlier "MO rides IMS" reading is **withdrawn**:
`IMS_SEND_SMS` is control glue that urild converts into exactly this PDU-mode
`CMGS` on the AT channel — no IMS client is needed for SMS on this
generation, and A5's MO half is done.

## 6. An unbounded NR scan hangs this unit; the recipe that registers

With the RIL stopped the stack came up (`+CFUN: 1`) but would not register
until the bands were locked to **LTE b1/b41 + NR n41/n78** *and* the §2 cold
cycle was run — on this unit, an unbounded NR scan (the operator's band set)
hangs.  The locks stayed in place for the rest of the session.  Until a
general scan strategy exists, registration after a takeover means: lock the
known bands first, then bring the stack up.

## 7. The bearer: what a RIL teardown destroys, and what brings data back

With the RIL stopped the handset had no data — expected, because nothing
re-establishes the bearer.  All measured:

* **The RIL's teardown destroys the internet context.**  `AT+CGACT?` after the
  stop shows only cid 11 active, and `AT+CGCONTRDP=11` names it `ims` — the
  VoLTE context survives, the internet one does not.  (Its interface
  addresses linger on `sipa_eth0`, which reads as "up" and is a lie: a ping
  has no route.)
* **A fresh context works** — the same sequence contracts §4.1 carries:
  `CGDCONT=1,"IPV4V6","<apn>"` → `CGACT=1,1` → `CGCONTRDP` (address 10.x/8,
  DNS 43.239.172.x) → `CGDATA="M-ETHER",1` → `CONNECT`.
* On the Android side, three runtime obstacles were measured between that
  context and a tethered client: the policy rule `32000: from all
  unreachable` swallows any packet whose lookup misses an earlier table (the
  default route belongs in table `legacy_system`, with the on-link subnets
  beside it); `tetherctrl_FORWARD` carries a catch-all `DROP` that only
  accepts uplinks netd knows; and DHCP advertises nothing.  All three are the
  host OS's half of the work — the daemon's job ends at `CONNECT`, and the
  permanent home for routing/NAT is the image that deploys the daemon (the G3
  acceptance).

## 8. "Registered, but every packet drops": turn tx-checksumming off too

`data up` disables checksum offload on the data interface.  With only **rx**
off, the bearer answers `+CGCONTRDP` perfectly and drops every packet — which
is exactly what "registered but ping gets nothing" was.  **tx-checksumming
(and TSO/GSO) must be off as well as rx** on the data interface.

## 9. The APN is resolved, not hardcoded

The APN came from a single hardcoded constant, so a SIM from another carrier
would attach with a context the network does not route.  Resolution is now a
three-step ladder, every step observable, printed by `data apn` together with
its source:

1. the operator override in the deployed data config
   (`/etc/e5/mobile-data.conf`, `APN=<value>`),
2. the context the modem already has for the default cid (`AT+CGDCONT?`),
   which is what the CP itself negotiated,
3. a table keyed by the home MCCMNC from `AT+CIMI`.

`data up` only writes `CGDCONT` when the value did not come from the modem.
Verified on the bench (IMSI 46015, cid 1 = `cbnet`): an override and the
modem-carried value both resolve, and each reports where it came from.

## 10. A `+CMTI:` inside a reply loop: queue in the sink, drain between requests

A `+CMTI:` can arrive inside another command's reply loop — the URC sink
fires there too — and the one thing it must never do is send AT from inside
that loop, because the session's gate is held by the command in flight.  So
the sink only queues `(storage, index)`, and the `serve` loop drains the
queue between requests, on the same single thread that answers requests: the
MT read can never interleave with a client's run.  A repeated announcement of
an unread slot is one read, not two.

## 11. `+CME ERROR` is a final code

The AT layer's own definition of a final code must include `+CME ERROR`; the
offline pty tests caught the version that waited past it.  A vendor extension
can call something a final code that 3GPP does not — the fake CP carries both
spellings so the demux cannot regress.

## 12. Two different deaths: the CP assert and AT silence

_Measured on this generation on the bench's Linux side, 2026-09-18, through
the AT broker this daemon replaced; kept because every watchdog here keys on
it._

* **Unpaced AT asserts the CP.**  A boot brought the bearer up, and at about
  9.5 minutes of polled AT the CP stopped answering with its own words in the
  kernel log: `Modem Assert … The queue was full`.  A session with no AT at
  all passed 17 minutes with zero asserts — the trigger is the queue filling
  under unpaced commands, which is why the profile's `at.pace_seconds`
  exists and is not negotiable.
* **AT can die with no assert.**  In one window a bare `AT` answered, and
  nine minutes later both channels returned nothing at all — no `OK`, no
  `ERROR`, no URC, no process holding them, zero CP asserts in the kernel
  log — while the bearer kept passing traffic.  So "the AT channel is dead"
  and "the CP has asserted" are not the same event, and a watchdog keys on
  `state.last_ok_age_s` — the age of the last command the CP actually
  answered — not on asserts.

---

## 13. The AT server does not survive a no-reader window: the takeover has a deadline

_2026-09-21, two takeovers on one boot, same setup as §1._

The first takeover of the day was slow: `stop vendor.ril-daemon`, then about
**three minutes with nobody reading either channel** before `serve` started.
The CP's AT server was gone.  Seven commands, seven timeouts, zero response
lines — not even `ERROR` — and zero CP asserts in the kernel log.  The URC
channel's only output was the parked backlog: 20 lines, every one of them
signal-grade `rssi 99` (the §2 shutdown park), and then silence.  §25.3 of
yesterday showed the park leaving AT alive; today the idle window took the AT
server down with it — §12's second death, with the trigger now identifiable.

Recovery was the vendor path: serve killed, `start vendor.ril-daemon`, modem
back (`IN_SERVICE`, NR SA).  The second takeover was fast: `stop` and `serve`
within seconds.  Everything answered: **59 commands, 29 OK, 0 timeouts, one
open per channel, zero reopens, for the whole session — including the first
call (§14).**

The measurable rule: **between the RIL's death and the daemon's first open
there is a window — fine at seconds, dead at ~3 minutes — in which this CP's
AT server goes away on its own.**  A resident owner is not only about the
one-reader rule; the transfer has a deadline, and the exact length of the
window is unmeasured (n = 2).  Until it is measured, the procedure is: stop
the old owner and open the channels immediately; never leave the CP
unattended between them.

## 14. The first VoLTE call: IMS registers without the vendor stack, and the daemon answers

_2026-09-21, the fast takeover of §13, handset on the 广电 (46015) network._

* **The network is VoLTE-only here.**  The Android oracle reports the CS
  domain NOT registered on NR while PS is HOME, and the operator block
  carries `mVopsSupport = 1` (voice over PS supported).  On this network a
  call is IMS or nothing — the contracts' "CS only" voice row needs the
  VoLTE reading below.
* **Right after the RIL dies, IMS is gone too.**  The CP answers AT (fast
  takeover) but `+CIREG: 0,0,0` with `+CAVIMS: 1` — VoLTE enabled, not
  registered.  The surviving `ims` context (§7) carries no registration.
* **The daemon brings IMS up itself.**  `cfun cold` (§2's cycle) ran under
  the daemon: `+CFUN: 1`, then PS registered (`+CEREG: 2,1,…,11`, NR SA),
  then **`+CIREG: 0,1,0` — IMS registered, with urild dead and no vendor IMS
  daemon running.**  The CP carries its own IMS client; what the vendor stack
  contributes to registration is configuration and keep-alive, not the
  registration act.  (This withdraws the same-session assumption that IMS
  would need the vendor stack.)
* **The call itself, end to end under the daemon:** the network delivered an
  MT VoLTE call — `+CRING: VOICE` every 5 s, decoded as `urc-incoming-call`
  events; `ATA` answered with `OK` and the far end confirmed the call was
  up; the far end released, and `AT+CLCC` was clean afterwards.  (Note for
  tooling: this generation answers voice `ATA` with `OK`, not `CONNECT` —
  a caller that waits for `CONNECT` will read a successful answer as a
  failure.)

**Still open for A9:** in-call audio and the five-minute two-way run.
MO dial and the audio verdict are now measured (same session, the web
face's first hours): an MO VoLTE dial **connected** — the signaling half
of G4 works in both directions — and the far end heard **silence**, which
settles the open question: the profile's `voice.supported = false` is
truthful, not conservative.  The CP negotiates the call end to end; what
nobody routes is the handset's own codec into the CP (that wiring belongs
to the vendor audio stack we deliberately replaced), so W4's audio work is
a mixer/route problem (tinyalsa-class), not a signaling one.

The SMS surface needed re-learning this session: the RIL had parked the
teach-in charset at `HEX` again (the SMSC read back as hex-of-ASCII,
`+CSCS="GSM"` first, then re-arm with the SMSC the SIM reports), and a cold
cycle wipes the `+CNMI` MT indication armed at start (measured:
`0,0,0,1,0` after `cfun cold`), so serve now re-arms the surface after
every passing cfun.  With both fixed, an MO loopback submit delivered and
was captured by the `+CMTI` path into the daemon's inbox, end to end
under the daemon.

## 15. The measurement tree answers without a header, and slot 2 has no IMEI

_2026-09-21, Android side, the vendor RIL stopped for each window._

### 15.1 A parser that looked for a header threw every reading away

* **`AT+SPENGMD` answers with a bare payload line.**  `AT+SPENGMD=0,14,1`
  comes back as `78,0-627264,0-5,0--9500,…` followed by `OK` — **no
  `+SPENGMD:` prefix at all**.  The first version of the serving/neighbour
  parsers looked for the word `SPENGMD` in the answer, so every real serving
  cell and every neighbour list was discarded and printed as "not reported".
  A gap where there is data is the one failure this module exists to prevent;
  the fix is to take the first payload line and strip a header only if one is
  there.
* **The shapes, measured.**  LTE serving is 65 dash-separated fields, all zero
  when the UE is camped on NR SA.  The LTE neighbour query answers eight
  records of twelve comma-separated fields; the NR neighbour query answers
  **column-wise**, eight columns of N (band, ARFCN, PCI, RSRP, RSRQ, SINR, and
  two the helper ignores), with RSRP/RSRQ/SINR in hundredths.
* **Corroboration, not assumption, for the NR serving positions.**  Group 9 of
  `AT+SPENGMD=0,14,1` reads `0x28002`, which is exactly the CI `+C5GREG`
  reports (`…,"A00028002",11`), and group 8 is the gNB id that CI is prefixed
  with.  That is how indices 8 and 9 were confirmed rather than guessed.
* **The serving record's SINR field stays unread.**  The Android helper reads
  one at group 15 and the measured answer has `1` there (0.01 dB, not a
  signal); the plausible value sits at group 5 and nothing corroborates it.
  `+CESQ`'s SS-SINR measures the same quantity properly, so that is what the
  panel uses, and the field is left empty rather than filled with a guess.
* **`0` and "not reported" are different words.**  The neighbour parsers now
  return `Option<Vec<…>>`: `Some(empty)` is "read, nothing in range", `None` is
  "nobody could read this".  Printing `0` for the second kind is what put a lie
  on the page in the first place.

### 15.2 `AT+COPS?` on its own does not answer

* Measured: the plain query answers `+COPS: 0` — mode only, no operator, no
  AcT, which is why the operator field was empty.  The vendor RIL asks in all
  three name formats in one command
  (`AT+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?`) and gets three
  `+COPS:` lines back: long name, short name, numeric + AcT.  `operator status`
  does the same now, and prefers the CP's own name over the MCC-MNC table.
* `ATI` is not answered on this firmware (`+CME ERROR: 4`), and `AT+CGMR` is a
  five-line version block (`Platform Version: …`, `BASE  Version: …`,
  `HW Version: …`, a date).  The baseband bar takes the `BASE` line and the
  whole block stays in the raw echo.

### 15.3 There is no per-slot IMEI read, and slot 2 is unprovisioned

* **The AT surface has exactly one IMEI.**  `AT+CGSN` and `AT+SPIMEI?` answer
  with the primary card's 15 digits — compared, and the same value.  Every
  per-slot form is refused: `AT+SPACTCARD=<n>;AT+CGSN` and `AT+SPIMEICHECK?`
  with CME 65536014 ("not supported"), `AT+SPIMEI=<n>` with 21, `AT+CGSN=1|2|3`
  and the bare `AT+SPIMEI` with 65536014, `AT+SPCARDINFO=<n>` (0..6 and two
  other arities) with 50.  `AT+SPIMEICHECK` alone answers `+SPIMEICHECK: 0`, a
  status flag rather than an identity.  (The CP does carry a
  `%RSIMREQ: "IMEI"` request, but that is the CP asking the *AP* for an IMEI in
  the virtual-SIM case — the other direction.)
* **The diag items are the only path, and slot 2's is all zeros.**  `imei read`
  over `/dev/sdiag_nr` answers for all three items, and 5e82 (SIM 2) and 5e90
  (spare) are fifteen zeros: no second identity is provisioned on this unit,
  which is why "IMEI1" has nothing to show.
* **Fifteen zeros pass Luhn**, so the read-back's own validation cannot tell an
  unprovisioned item from a value — and the panel showed `000000000000000`
  beside the real IMEI.  `imei read` now reports an all-zero item as *not
  provisioned*: a fact about the handset, kept apart from a value and from a
  failed read.

### 15.4 Probing etiquette, learned the hard way

* **One owner, or the channel wedges.**  A probe that started a fresh process
  per command saw `TIMEOUT` on nearly every query and finally `CHANNEL DOWN`;
  the same commands answered at once as soon as one `serve` held the channel.
  The SIPC contract is one reader that never closes, and it means it.
* **A leftover daemon owns the socket and the channel.**  One probe measured
  nothing because an earlier run's `serve` still held `cmd.sock`: the new
  daemon refused to start and the client talked to the wedged old one.  Probe
  scripts now take a state directory of their own and kill leftovers first.
* **The UE falls off its registration within minutes of the RIL stopping.**
  Right after the takeover `AT+SPQ5GNCELLEX` lists real neighbours (the serving
  cell plus nine); minutes later it is `+CEREG: 1,0`, `+CSQ: 0,99` and
  `+CME ERROR: 3`.  A measurement has to be taken in that window, and a zero
  answer afterwards is the state, not the parser.
* **Repeated stop/start of the RIL leaves the SIM states degraded** (`LOADED` →
  `NOT_READY`, slot 2 slower to come back than slot 1).  Restarting
  `vendor.ril-daemon` recovers slot 1 within ~25 s; a reboot is the clean way
  back.


---

## 15. The daemon replaces the RIL's data face, and the phone keeps its internet

_2026-09-21, third boot of the day, the web face running on the handset._

With the vendor stack dead, the data path needed three things the RIL
normally does, in this order:

1. **`AT+CGATT=1` -- the attach nobody performs.**  After a cold cycle the
   CP registers (CEREG 2,1, C5GREG 0,1) but sits at `+CGATT: 0` forever:
   attach is a RIL decision, not a CP reflex.  A manual attach answered
   `+CME ERROR: 0` once while registration was still settling -- the same
   command succeeded minutes later.  (A light `AT+CFUN=0`/`AT+CFUN=1`
   cycle also recovered a boot where the full cold cycle left CEREG at
   2,0 -- the third boot of the day did not survive cold cycle #1.)
2. **`data up cbnet`** -- APN pinned by argument (the `/etc/e5` override
   file does not exist on Android, and the MCC-MNC table read came too
   early after the cold).  The action then drove CGDCONT/CGACT/
   `+CGDATA="M-ETHER"` and the CP handed the AP a real bearer: the kernel
   instantiated **`sipa_eth0`** with `10.33.251.55/8` from `+CGCONTRDP`.
3. **The NAT plan** -- `ip_forward`, default route in `legacy_system`
   (the policy-routing trap from FINDINGS 25.8), the
   `tetherctrl_FORWARD` ACCEPT pair, and `-o sipa_eth0 -j MASQUERADE`.

End state, measured on the handset: the host pings by name through the
bearer (`www.gov.cn`, 2/2, ~42 ms), the identity panel reports
`ip 10.33.251.55 / apn cbnet`, and a hotspot client would walk the same
masquerade.  The one honest gap: the CP does not answer the `SPENGMD`
measurement queries, so the web's serving-cell metrics report
`serving_supported: false` -- probe, report, do not pretend (W5).
