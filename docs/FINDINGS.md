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
