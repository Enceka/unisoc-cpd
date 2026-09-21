# unisoc-cpd 中文文档

面向 Unisoc CP（基带）的设备无关控制守护进程。设计依据是 [`BASEBAND-PLAN.md`](BASEBAND-PLAN.md)。

> 英文版见 [`README.md`](README.md)，接口契约见 [`docs/BASEBAND-CONTRACTS.md`](docs/BASEBAND-CONTRACTS.md)。

---

## 1. 这是什么

一句话：**一个二进制，靠一份 profile 驱动一颗 Unisoc CP。**

核心判断是：AP 侧看到的契约属于**基带代次**，而不是主板。SIPC 通道编号、AT/URC 方言、CP 启动握手、日志/转储通道、时间同步通道、mailbox 的脾气——这些在同一代 CP 上不管焊到哪块板子都一样。所以：

* `core/`（核心）**永远不出现任何板级名字**；
* 一块板子知道的一切——设备节点、分区、网卡名、spool 通道、厂商命令——全部写在 `platform/profiles/<平台>.toml` 里。

这条边界不是靠自觉，而是靠命令检查：`unisoc-cpd profile-check` 会把每个 profile 允许知道的名字拿去搜 `src/`，一旦核心代码里出现就判失败。

## 2. 目录结构

```
unisoc-cpd / ucpd              一个二进制，一份 profile
├── src/channel.rs             通道层：独占持有、持续排空、健康计数
├── src/at.rs                  AT 编解码、URC 分流、串行化、限速、超时
├── src/urc.rs                 URC 解码：控制面的主动一半（MT 短信/来电/注册/信号）
├── src/unisoc_at.rs           本代 CP 自己的 AT 扩展（频段/小区/5G/IMS）
├── src/capability/            各项能力（见第 6 节）；serve 是常驻持有者
├── src/telemetry.rs           每次运行的 JSON 摘要
├── src/profile_check.rs       A12 可移植性门禁（做成命令）
├── platform/profiles/e5.toml     唯一知道 E5 的文件
├── platform/profiles/mu300.toml  第二个平台，同一核心（桩，未验证）
├── units/                     systemd 单元：浸泡（timer）+ 常驻持有者
├── tools/build-aarch64.sh     交叉编译脚本
├── docs/BASEBAND-CONTRACTS.md 通道/命令/时序契约（§9 是 G2 的控制面契约）
└── docs/FINDINGS.md           本仓库自己实测得出的结论，逐条编号
```

## 3. 构建与测试

```sh
cargo build --release        # 主机
cargo test                   # 119 个测试，不需要设备
```

测试跑在一个 **pty 上的伪 CP** 上。pty 是 SIPC tty 唯一诚实的替身：真 tty、两端、驱动按突发交付行。伪 CP 会在**每一条应答里插入一条 URC**，所以"URC 与应答分流"、"限速"、"超时"、`>` 续行提示符这四条路径**每条命令都会被走到**，而不是只在顺利路径上被走到。

交叉编译到设备（Debian trixie arm64）：

```sh
rustup target add aarch64-unknown-linux-musl
tools/build-aarch64.sh
```

选 `aarch64-unknown-linux-musl` 是因为**静态链接的二进制不受对端 glibc 版本影响**——这也是计划里"同一个二进制、第二个平台"最干净的解释。脚本里唯一不显然的一步是链接器：musl 目标由 rustc 自己链入自带的 musl 目标文件，但它驱动的链接器仍须认得 AArch64 选项，而主机的 GNU ld 会拒绝 `--fix-cortex-a53-843419`（那是 lld 的选项），所以脚本把目标指向工具链自带的 `rust-lld`。

主机没有交叉工具链时，也可以在设备上本地编译（设备自带 rootfs 是 Debian arm64）：

```sh
tools/build-aarch64.sh --on-device 用户@主机
```

## 4. 命令行

```
unisoc-cpd [选项] <能力> [参数...]
```

| 选项 | 说明 |
|---|---|
| `--profile <名字\|路径>` | 指定 profile，省略时若目录下只有一个则自动选中 |
| `--profiles-dir <目录>` | profile 所在目录（默认 `platform/profiles`） |
| `--mode native\|vendor` | 默认 `native` |
| `--runs-dir <目录>` | 运行摘要写入位置（默认 `./runs`） |
| `--state-dir <目录>` | 通道独占锁位置（默认 `/run/unisoc-cpd`） |
| `--socket <路径>` | 交给常驻守护进程：对其它能力是“去问它”，对 `serve` 是“在这里听” |
| `--no-telemetry` | 不写运行摘要 |
| `-v` | 详细输出 |

计划里的测试台拼写 `unisoc-cpd mode vendor|native <能力>` 与 `--mode` 等价，两种都接受。

三个不带 AT 的子命令：

```sh
unisoc-cpd capabilities     # 列出所有能力
unisoc-cpd profiles         # 列出所有平台 profile
unisoc-cpd profile-check    # 跑 A12 门禁，通过返回 0
```

### 常驻持有者（G2）

`serve` 是 Android 那一侧 `urild` 坐的那个位置：开机独占两条通道、持续读
URC、空闲时探活，并在 unix socket 上以 JSON 应答能力请求。**只允许一条 AT
通道一个读者**，所以能力要么直连通道，要么问守护进程，不能两者并存：

```sh
unisoc-cpd --profile e5 --mode native serve --socket /run/unisoc-cpd/cmd.sock

# 另一个终端：通道已被守护进程持有，能力必须问它，而不是自己开通道
unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock sim
unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock register status
unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock state   # 守护进程状态（含 last_ok_age_s）
unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock urc     # 最近解码出的 URC 事件
unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock messages # 守护进程自己读到的短信（MT）
```

`serve` 会自己对 `+CMTI:` 起反应：modem 一宣布新短信，守护进程就在两个请求
之间用 `AT+CMGR` 把它读出来（只读，不删；PDU 模式不解码、明说），客户端用
`messages` 随时取。发短信走 `sms send <号码> <正文>`（MO 路径，`AT+CMGS` 的
`>` 续行提示符在 AT 层处理）。

契约（URC→事件表、请求/应答、哪些是有意不做的）在
[`docs/BASEBAND-CONTRACTS.md`](docs/BASEBAND-CONTRACTS.md) §9。

### 两种模式

* **`native`**：我们持有通道、我们驱动调制解调器。
* **`vendor`**：仍然让厂商守护进程干活，我们只执行 profile 里为该能力配置的命令并记录它的输出。

同一个验收用例在两种模式下都要跑通，厂商那一侧才能关掉——所以两种模式写出的摘要是**可以直接对比**的。

**退出码**：`0` 通过 ／ `1` 失败 ／ `2` 用法或配置错误 ／ `3` 环境问题（通道被他人占用、设备打不开）。

## 5. 唯一红线

代码里强制执行，不是约定：

1. **一条 AT 通道绝不出现两个读者。** 每个 AT 通道加独占 `flock`；第二个实例会**明确失败并返回退出码 3**，而不是静默地抢走一半数据。之所以是红线：SIPC 通道对没人读的内容是**排队**的，第二个读者不会报错，只会让两边都拿不到完整应答。

身份（IMEI）与 NV 的写入不在红线之列：它们走的是受护栏的路径（`imei read`/`probe`/`write`、`nv backup`/`restore`，见契约文档第 8 节）——profile 显式开启、备份先行、写后回读，而不是一纸禁令。

## 6. 能力一览

| 能力 | 用途 | 子命令 |
|---|---|---|
| `link` | **W1 门禁**：独占 AT/URC 通道、按节奏探活、统计 CP assert / URC 间隔 / mailbox 中断增量 | `--seconds N`（默认 10）`--interval S`（默认 30）`--timeout S`（默认 5）`--probe 命令`（默认 `AT`）。72 小时浸泡就是 `--seconds 259200` |
| `sim` | SIM 在位与 PIN 状态 | 无参＝查询；`<PIN>`＝解锁；`identity`＝读 IMSI/ICCID（只读） |
| `cfun` | 射频开关 | `status` `on` `off` `reset` `cycling`（后者会把 SIM 弄丢到重启，故返回失败并注明） |
| `register` | 电路/分组注册、UE 用途设置 | `status`（含 `+C5GREG?`）`uemode` `data-centric` `voice-centric` |
| `signal` | 信号质量，解码 RSRP/RSRQ/SINR | 无参 |
| `operator` | 选网 | `status` `scan` `auto` `manual <MCCMNC>` |
| `band` | **频段锁定与小区锁定** | `status` `lock <lte\|nr> <频段...>` `unlock <lte\|nr\|all>` `cell-lock <lte\|nr> <频点> <PCI>` `cell-unlock <lte\|nr\|all>` |
| `nr` | 5G SA/NSA 偏好与 5G 注册 | `status` `sa on` `sa off` |
| `ims` | VoLTE/VoNR 探测与开关（计划 W5：能用就说能用） | `status` `volte on\|off` `vonr on\|off` |
| `sms` | 短信 | `list` `read <序号>` `delete <序号\|all>` `send <号码> <正文>` |
| `ussd` | USSD 会话 | `<业务码>`；无参＝取消会话 |
| `call` | 语音 CS 控制 | `list` `dial <号码>` `answer` `hangup` `dtmf <按键>` |
| `data` | PDP 与承载网卡 | `status` `up [APN]` `down` `apn` |
| `imei` | 设备身份（带护栏） | `read [--index N]` `probe` `write <串号> --index N --yes` |
| `nv` | NV 视图与受护栏的备份/恢复 | `list` `hash <分区>` `[--head 字节数]` `backup [--dir D]` `restore <分区> <镜像> --yes` |
| `diag` | 诊断 | `channels` `spools` `mailbox` `asserts` `urc` `all` |
| `serve` | **G2**：常驻持有者，代替 Android 的 RIL 坐住通道 | `[--socket 路径]` `[--seconds N]`；客户端用 `--socket` + `<能力>`，或问它 `state` / `urc` |

写操作都会**回读校验**：`band lock` 写完立刻读回，没生效就不算通过——"调制解调器收下了命令"和"锁定真的生效了"是两回事。

## 7. 常用示例

```sh
# 通道健康（十秒版；换 --seconds 259200 就是 72 小时浸泡）
unisoc-cpd --profile e5 --mode native link --seconds 60

# 控制面
unisoc-cpd --profile e5 --mode native sim
unisoc-cpd --profile e5 --mode native register status
unisoc-cpd --profile e5 --mode native signal

# 频段：先看，再锁，再确认读回
unisoc-cpd --profile e5 --mode native band status
unisoc-cpd --profile e5 --mode native band lock nr 78
unisoc-cpd --profile e5 --mode native band unlock all

# 承载
unisoc-cpd --profile e5 --mode native data up
unisoc-cpd --profile e5 --mode native data status

# 厂商基线（做差分用）
unisoc-cpd --profile e5 --mode vendor register

# 只读排查：不碰 AT 通道
unisoc-cpd --profile e5 --mode native diag spools
unisoc-cpd --profile e5 --mode native nv list
```

## 8. 运行摘要（遥测）

每次运行写 `runs/<平台>/<日期>/<能力>.<模式>.<时分秒>.json`，并把同一条记录追加到 `runs/<平台>/<日期>/summary.json`。这就是验收矩阵的机读形式——A1 的四个条件分别对应：

| A1 要求 | 摘要里的字段 |
|---|---|
| 0 次 CP assert | `cp_asserts.delta` |
| URC 间隔 < 5 s | `urc.max_gap_s` |
| mailbox 中断计数增长 | `mailbox_irq.delta` |
| 通道未重建（通道健康） | `channels.cmd.reopens` |

样例：

```json
{
  "platform": "e5", "mode": "native", "capability": "link",
  "status": "pass", "exit_code": 0,
  "at":        { "commands": 5, "ok": 5, "timeouts": 0, "probes": 5, "probe_failures": 0 },
  "channels":  { "cmd": { "opens": 1, "reopens": 0, "rx_lines": 10 } },
  "urc":       { "lines": 5, "max_gap_s": 0.5, "gaps_over_threshold": 0 },
  "mailbox_irq": { "before": 32760, "after": 32784, "delta": 24 },
  "cp_asserts":  { "before": 0, "after": 0, "delta": 0 }
}
```

说明几点：

* `probes` 与 `commands` **分开计数**。前者回答"我们多久问一次 CP 还活着没有"，后者回答"我们往 CP 发了多少 AT"——只有前者允许驱动看门狗。
* `mailbox_irq` / `cp_asserts` 在**测不到的平台上是 `null`**，而不是 0。该平台没有对应计数器时，宁可写"未测量"也不写"没有"。
* 非请求行（URC）与应答行分开记账，`urc.tail` 会保留最近若干条原文，供事后取证。

## 9. 部署到设备

浸泡用 systemd timer，单元在 `units/`：

```sh
install -Dm755 target/aarch64-unknown-linux-musl/release/unisoc-cpd /usr/local/bin/unisoc-cpd
install -Dm644 platform/profiles/e5.toml /etc/unisoc-cpd/e5.toml
cp -r units/* /etc/systemd/system/ && systemctl daemon-reload
systemctl enable --now unisoc-cpd-soak.timer
```

**接管控制面**（G2）用常驻单元而不是浸泡 timer：它声明了对旧 AT 代理的
`Conflicts=`，启动时由 systemd 替我们停掉它们——这正是“接管”而不是“并存”：

```sh
systemctl enable --now unisoc-cpd.service
```

**切换所有权前必须先停掉旧的 AT 代理**，否则本程序会（按设计）拒绝启动：

```sh
systemctl stop e5-mobile-data-watch e5-mobile-data e5-atd
```

`units/unisoc-cpd-soak.service` 和 `units/unisoc-cpd.service` 里都用 `Conflicts=`
声明了这三个单元，但手动切换时要记得它们。

## 10. 当前状态（不夸大）

| 项目 | 状态 |
|---|---|
| 核心（channel / at / telemetry / profile / CLI） | 已实现，119 个测试通过 |
| `profile-check`（A12 门禁） | 通过（两个 profile，核心无平台名） |
| URC 解码（契约 §9.2） | 已实现并测试；`+ECIND:`/`+CMT:`/`+CDS:` 有意保持原样，不解码就不假装解码 |
| **G2 控制面（`serve`）** | 代码与离线测试就绪：常驻独占两条通道、URC 事件、socket 上的能力请求、空闲探活、`state`（含 `last_ok_age_s`）。**未上真机**——还没当过 `/dev/stty_nr1` 的主人 |
| aarch64 构建 | 静态 `aarch64-unknown-linux-musl`，`tools/build-aarch64.sh` 可复现 |
| **真机只读验证** | ✅ 已做：`diag spools`（8 个节点，全部 `char`）、`diag mailbox`、`diag asserts`（0 次 assert）、`nv list`（7 个分区）。每次运行的摘要都显示 `at.commands: 0`、`channels.cmd.opens: 0`，即**全程没有打开过 AT 通道** |
| **真机接管 AT** | ✅ **Android 侧已做**（2026-09-20）：`stop vendor.ril-daemon` 后守护进程成为两条通道的唯一读者，`link`/`sim`/`band`/`serve`+socket 全部实测通过，0 次 CP assert，URC 解码 50/50；交接规程与栈冷启动要求见 FINDINGS §1–§2。Linux 侧还没在开机时当过主人，没跑过浸泡，profile 仍 `verified = false` |
| log/dump spool 排空、`stime_ch` | ❌ 未实现（W1 遗留） |
| 语音 CS | 音频路由仍无（`voice.supported = false`）；信令一半已实测——`ims status` 读 `+CIREG`（IMS 注册门禁），`call` 纯信令拨/接/挂；**守护进程之下的第一通 VoLTE 来电已接通**（FINDINGS §14） |
| W7 web 界面 | `web` 已落地：浏览器收发短信、接打电话、实时 URC 事件流、来电全屏横幅——只是 socket 客户端，绝不碰通道（`units/unisoc-cpd-web.service`） |

## 11. AT 指令的来源

本代 CP 特有的那批指令（频段/小区锁定、5G SA/NSA、VoLTE/VoNR、UE 用途设置）是**通过研究这颗调制解调器自身的 AT 接口得到的**，不是来自厂商文档，也还没有发往真机。因此：

* 位掩码表与编解码都在 `src/unisoc_at.rs`，并有单元测试；
* 契约写在 `docs/BASEBAND-CONTRACTS.md` §4.2，来源标记为 **researched**；
* 凡是写操作一律回读校验；
* 把它们当成"该先试的形状"，而不是"已测的行为"。

## 12. 术语表

| 词 | 含义 |
|---|---|
| **CP** | Communication Processor，即基带处理器（相对 AP 应用处理器） |
| **AP** | Application Processor，跑 Linux 的这一侧 |
| **SIPC** | Unisoc 的处理器间通信通道；`/dev/stty_nr*` 就是它的 tty 视图 |
| **URC** | Unsolicited Result Code，调制解调器主动上报，如 `+CGEV:`、`+CEREG:` |
| **AT 通道 / URC 通道** | 本代分两个：`nr1` 是干净的命令通道，`nr0` 是 URC 通道，必须持续读 |
| **限速（pacing）** | 两条命令之间的最小间隔。不限速会把 CP 的队列塞满，触发 `CP assert ... The queue was full` |
| **AcT** | Access Technology，接入制式。`+CEREG` 的第 5 个字段：11 = NR SA，13 = EN-DC/NSA |
| **PDP / 承载** | 数据上下文及其网卡（`sipa_eth0`） |
| **spool** | CP 往 AP 方向的日志/转储通道 |
| **profile** | 一份 TOML，描述某个平台的全部板级事实 |
| **native / vendor 模式** | 前者我们干，后者厂商守护进程干、我们只记录 |
| **浸泡（soak）** | 长时间（计划要求 72 小时）连续运行，检 0 assert、0 AT 静默 |

## 13. 许可

MIT，与仓库其余部分一致。
