# unisoc-cpd

面向 Unisoc CP（基带）的设备无关控制守护进程，依据 [`BASEBAND-PLAN.md`](BASEBAND-PLAN.md) 实现。

> English: [`README.md`](README.md)

AP 侧所面对的接口契约由**基带代次**决定，而非由主板决定：SIPC 通道、AT/URC
方言、CP 启动握手、日志/转储 spool、时间同步通道以及 mailbox 的行为特性，在同一代
CP 上与所在板卡无关。因此本项目的核心代码不出现任何平台名称。板级信息——设备节点、
分区、网卡名、spool 通道、厂商命令——全部位于 `platform/profiles/` 下的 profile 中；
一旦这一边界被越过，`unisoc-cpd profile-check` 即判定失败。

```
unisoc-cpd                     单一二进制，由一份 profile 驱动
├── src/channel.rs             独占持有通道，持续排空，并统计所见数据
├── src/at.rs                  AT 编解码、URC 分流、串行化、限速、超时
├── src/urc.rs                 URC 解码：控制面中由 CP 主动上报的部分
├── src/unisoc_at.rs           本代 CP 的专有 AT 扩展（频段/小区/5G/IMS）
├── src/nat.rs                 承载的主机侧：规则与执行计划的生成、回读校验
├── src/capability/            link、sim、cfun、register、signal、operator、
│                              band、nr、ims、sms、ussd、call、data、nv、diag、serve
├── src/telemetry.rs           每次运行的 JSON 摘要：assert、URC 间隔、mailbox 增量、计数器
├── src/profile_check.rs       A12 门禁，以命令形式提供
├── platform/profiles/e5.toml      唯一包含 E5 信息的文件
├── platform/profiles/mu300.toml   第二个平台，共用同一核心（桩，未验证）
├── docs/BASEBAND-CONTRACTS.md     W0：通道、命令与时序契约（§9：G2）
└── docs/FINDINGS.md               实测结论，逐条编号
```

## 构建

```sh
cargo build --release          # 主机
cargo test                     # 172 个测试，无需设备
```

测试针对运行在 **pty** 上的模拟 CP 执行。pty 是 SIPC tty 唯一可信的替身：它是真实的
双端 tty，驱动按突发方式交付数据行，模拟的调制解调器在每条应答中插入 URC——因此
应答/URC 分流、限速、超时以及 `>` 续行提示符在每一条命令上都会被覆盖，而不仅限于
正常路径。

设备端（Debian trixie arm64）的构建见 `tools/build-aarch64.sh`。可靠的交叉编译目标是
`aarch64-unknown-linux-musl`：静态二进制不依赖目标系统的 glibc 版本，这也是对计划中
“同一二进制，第二个平台”的最直接实现。该脚本即完整的构建流程，其中一个不直观的步骤是
链接器必须使用 `rust-lld` 而非主机的 GNU ld，因为 `--fix-cortex-a53-843419` 是主机 ld
不接受的 AArch64 选项。

## 运行

```
unisoc-cpd [--profile NAME] [--mode native|vendor] <capability> [args]
unisoc-cpd mode native <capability>      # 测试台使用的写法
unisoc-cpd capabilities                  # 列出支持的能力
unisoc-cpd profiles                      # 列出已知平台
unisoc-cpd profile-check                 # A12 门禁
```

示例：

```sh
unisoc-cpd --profile e5 --mode native link --seconds 60      # CP 链路健康检查
unisoc-cpd --profile e5 --mode native link --seconds 259200  # 72 小时浸泡测试（A1）
unisoc-cpd --profile e5 --mode native sim
unisoc-cpd --profile e5 --mode native band status
unisoc-cpd --profile e5 --mode native band lock nr 78
unisoc-cpd --profile e5 --mode native data up                 # 建立承载；[data.nat].enabled 时同时配置主机侧
unisoc-cpd --profile e5 --mode native data nat status         # 路由、转发规则对、伪装规则——实测读回，而非推定
unisoc-cpd --profile e5 --mode vendor register               # 厂商基线
unisoc-cpd --profile e5 --mode native nv list                # 只读
unisoc-cpd --socket /run/unisoc-cpd/cmd.sock web 0.0.0.0:7887  # W7：浏览器界面
```

`--mode vendor` 执行 profile 为该能力指定的厂商命令并记录其输出，从而可以用同一验收
测试分别检验厂商守护进程与本项目，并对比两份摘要。`link` 与 `serve` 仅支持 native
模式：通道持有情况的测量不存在可供对比的厂商实现。

## 常驻持有者（G2）

`serve` 承担 Android 中 RIL 的角色：开机时一次性接管通道并持续持有，不间断读取主动
上报流，空闲时探测 CP，并通过 unix socket 响应能力请求。由于一条 AT 通道只能有一个
读者，能力要么自行打开通道（未运行 `serve` 时），要么通过 `--socket` 请求守护进程，
二者不可兼得：

```sh
unisoc-cpd --profile e5 --mode native serve --socket /run/unisoc-cpd/cmd.sock

unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock sim
unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock state    # 守护进程自身状态
unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock urc      # 已解码的 URC 事件
unisoc-cpd --mode native --socket /run/unisoc-cpd/cmd.sock messages # 守护进程自行读取的短信（MT）
```

`serve` 会自行处理 `+CMTI:`：调制解调器通告新短信后，守护进程在两次请求之间以
`AT+CMGR` 读取（只读，从不删除；PDU 模式不解码，并如实标注），客户端通过 `messages`
获取结果。发送使用 `sms send <号码> <正文>`（MO 路径，其 `>` 续行提示符由 AT 层处理）。

请求的对象始终是**能力**，而非原始 AT 字符串：经 socket 转发原始 AT 会使“单一读者”
规则脱离唯一负责执行它的进程。契约——URC 到事件的映射表、请求/响应格式，以及有意
暂不实现的部分——见 `docs/BASEBAND-CONTRACTS.md` §9。

## 遥测

每次运行写入 `runs/<platform>/<date>/<capability>.<mode>.<hhmmss>.json`，并将同一条记录
追加到 `runs/<platform>/<date>/summary.json`：

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

这是验收矩阵的机器可读形式：A1 的四项条件分别对应 `cp_asserts.delta`、
`urc.max_gap_s`、`mailbox_irq.delta` 与 `channels.cmd.reopens`。

## 在 E5 上部署

浸泡测试以 systemd 定时器运行（`units/`），这同时满足计划中“看门狗重启不得掩盖证据”
的要求：

```sh
install -Dm755 target/aarch64-unknown-linux-musl/release/unisoc-cpd /usr/local/bin/unisoc-cpd
install -Dm644 platform/profiles/e5.toml /etc/unisoc-cpd/e5.toml
cp -r units/* /etc/systemd/system/ && systemctl daemon-reload
systemctl enable --now unisoc-cpd-soak.timer
```

接管控制面（G2）使用常驻单元而非浸泡定时器。该单元对现有 AT 代理声明了
`Conflicts=`，由 systemd 负责停止它们——“接管”即指此，而非与之共存：

```sh
systemctl enable --now unisoc-cpd.service
```

现有的 shell 代理 `e5-atd` 运行期间持有 `/dev/stty_nr1`，此时 `unisoc-cpd` 会拒绝成为
第二个读者，并以退出码 3 明确报错。请先停止厂商侧服务：

```sh
systemctl stop e5-mobile-data-watch e5-mobile-data e5-atd
```

## 状态

| | |
|---|---|
| 核心（channel/at/telemetry/profile/CLI） | 已实现，172 个测试通过 |
| 能力 | `link`、`sim`、`register`、`signal`、`operator`、`cfun`、`band`、`nr`、`ims`、`sms`、`ussd`、`call`、`data`、`nv`、`diag`、`serve`、`web` |
| URC 解码（契约 §9.2） | 已实现并测试；`+ECIND:`/`+CMT:`/`+CDS:` 有意保留原文，不做不完整的解码 |
| G2 控制面（`serve`） | 已实现并通过离线测试，并已**常驻于手机**：Android 侧（2026-09-20 起）在停止厂商 RIL 后，守护进程持续数小时持有两条通道——URC 流解码 50/50，socket 客户端（`sim`、`band`、`web`）均经其应答，0 次 CP assert，`state` 报告 `last_ok_age_s`；Linux 侧（e5-linux，2026-09-25 起）由 systemd 在**开机时**接管，`e5-bearer-up` 经其建立承载。待完成：72 小时浸泡 |
| W0 契约 | `docs/BASEBAND-CONTRACTS.md`，本代 AT 扩展已完成调研并有单元测试 |
| A12 `profile-check` | 通过（两个 profile，核心无平台名称） |
| aarch64 构建 | 静态 `aarch64-unknown-linux-musl`，由 `tools/build-aarch64.sh` 构建 |
| 真机只读验证 | 已在手机上验证：`diag spools`（8 个节点，均为 `char`）、`diag mailbox`、`diag asserts`（0 次 CP assert）、`nv list`（7 个分区）。每次运行的摘要均为 `at.commands: 0`、`channels.cmd.opens: 0`——厂商 RIL 持有 AT 通道期间有意不触碰该通道 |
| 真机 AT | **Android 侧已实测**（2026-09-20）：`stop vendor.ril-daemon` 后守护进程成为两条通道的唯一读者——`link`、`sim`、`band`、`serve` 及 socket 客户端全部通过，0 次 CP assert，URC 解码 50/50；交接规程与协议栈冷启动要求见 FINDINGS §1–§2。**Linux 侧开机接管已运行**（2026-09-25 起，e5-linux）；尚未完成浸泡，尚无 profile 标记为 `verified` |
| 信号测量 | `+CSQ` 与 `+CESQ` 均已解码，含 NR SA 扩展字段（SS-RSRP/RSRQ/SINR）；RSSI 由 CSQ 索引换算为 dBm（`-113 + 2·n`），索引 99 报告为未知，不换算为数值 |
| log/dump spool 排空、`stime_ch` | 未实现（W1，待完成） |
| 语音 | 尚无音频路由（`voice.supported = false`）；信令部分已实测——`ims status` 读取 `+CIREG`（IMS 注册门禁），`call` 仅在信令层完成拨号/接听/挂断；**守护进程下已接通第一通 MT VoLTE 来电**（FINDINGS §14） |
| W7 web 界面 | `web` 通过 HTTP 提供守护进程的功能：收件箱与删除、发送短信、拨号/接听/挂断、实时 URC 流、来电横幅，以及高级信息、APN、信号指标、网络、频段/小区锁定、IMEI 与原始 AT 控制台面板——本身是 socket 客户端，从不持有通道（`units/unisoc-cpd-web.service`）。页面采用 Material You，单文件、可离线：无需构建，也不从网络加载样式表或字体（它可能是访问设备的唯一途径） |
| 承载的主机侧（`data nat`） | 已实现并**在 Android 侧实测**（2026-09-21，RIL 已停止）：`on` 完成 8/8 步，再次执行 `on` 每条规则仍只有一份，`status` 读回为 `nat: complete`；实测前两张路由表均无默认路由，实测后 `ping 223.5.5.5` 应答 2/2（契约 §10）。Linux 侧自带的 `nat.nft` 不受影响 |

## 附录 A：能力参考

| 能力 | 用途 | 子命令与参数 |
|---|---|---|
| `link` | W1 门禁：独占 AT/URC 通道，按固定间隔探测，统计 CP assert、URC 间隔与 mailbox 中断增量 | `--seconds N`（默认 10）、`--interval S`（默认 30）、`--timeout S`（默认 5）、`--probe 命令`（默认 `AT`）；72 小时浸泡为 `--seconds 259200` |
| `sim` | SIM 在位与 PIN 状态 | 无参数为查询；`<PIN>` 解锁；`identity` 读取 IMSI/ICCID（只读） |
| `cfun` | 射频功能 | `status`、`on`、`off`、`reset`、`cycling`（后者会使 SIM 丢失直至重启，因此返回失败并注明原因） |
| `register` | 电路域/分组域注册与 UE 用途设置 | `status`（含 `+C5GREG?`）、`uemode`、`data-centric`、`voice-centric` |
| `signal` | 信号质量 | 无参数；解码 `+CSQ` 与 `+CESQ`，含 NR SA 扩展字段 |
| `operator` | 网络选择 | `status`、`scan`、`auto`、`manual <MCCMNC>` |
| `band` | 频段锁定与小区锁定 | `status`、`lock <lte\|nr> <频段...>`、`unlock <lte\|nr\|all>`、`cell-lock <lte\|nr> <频点> <PCI>`、`cell-unlock <lte\|nr\|all>` |
| `nr` | 5G SA/NSA 偏好与 5G 注册 | `status`、`sa on`、`sa off` |
| `ims` | VoLTE/VoNR 探测与开关 | `status`、`volte on\|off`、`vonr on\|off` |
| `sms` | 短信 | `list`、`read <序号>`、`delete <序号\|all>`、`send <号码> <正文>` |
| `ussd` | USSD 会话 | `<业务码>`；无参数为取消会话 |
| `call` | 语音呼叫控制 | `list`、`dial <号码>`、`answer`、`hangup`、`dtmf <按键>` |
| `data` | PDP 上下文与承载网卡 | `status`、`up [APN]`、`down`、`apn`、`contexts`、`set-apn <APN>`、`clear-apn`、`save-apn <APN>`、`nat on\|off\|status\|plan` |
| `imei` | 设备身份（带防护） | `read [--index N]`、`probe`、`write <IMEI> --index N --yes` |
| `nv` | NV 视图与带防护的备份/恢复 | `list`、`hash <分区> [--head 字节数]`、`backup [--dir D]`、`restore <分区> <镜像> --yes` |
| `diag` | 诊断 | `channels`、`spools`、`mailbox`、`asserts`、`urc`、`all` |
| `serve` | G2：常驻持有者 | `[--socket 路径]`、`[--seconds N]`；客户端使用 `--socket` 加能力名，或请求 `state`、`urc`、`messages` |
| `web` | W7：浏览器界面 | `web [地址:端口]`（须配合 `--socket`） |

所有写操作均回读校验：例如 `band lock` 写入后立即读回，未生效即判定失败——调制解调器
接受命令与配置实际生效是两回事。

强制约束：**一条 AT 通道不允许有两个读者。** 每条 AT 通道持有独占 `flock`，第二个实例
以退出码 3 明确失败，而不会静默地分走一部分数据——SIPC 通道对未读取的内容会排队，
第二个读者不会报错，只会导致双方都无法获得完整应答。身份（IMEI）与 NV 写入经由带防护
的路径进行（`imei read`/`probe`/`write`、`nv backup`/`restore`，见契约第 8 节）：需在
profile 中显式启用、写前备份、写后回读。

本代 CP 的专有指令（频段/小区锁定、5G SA/NSA、VoLTE/VoNR、UE 用途设置）来源于对该
调制解调器自身 AT 接口的调研，而非厂商文档。位掩码表与编解码位于 `src/unisoc_at.rs` 并有
单元测试，契约见 `docs/BASEBAND-CONTRACTS.md` §4.2（来源标记为 researched）。

## 附录 B：术语

| 术语 | 含义 |
|---|---|
| CP | Communication Processor，基带处理器（相对于应用处理器 AP） |
| AP | Application Processor，运行 Linux 的一侧 |
| SIPC | Unisoc 的处理器间通信机制；`/dev/stty_nr*` 为其 tty 接口 |
| URC | Unsolicited Result Code，调制解调器主动上报的结果码，如 `+CGEV:`、`+CEREG:` |
| AT 通道 / URC 通道 | 本代分为两条：`nr1` 为命令通道，`nr0` 为 URC 通道，后者必须持续读取 |
| 限速（pacing） | 两条命令之间的最小间隔；不限速会塞满 CP 的队列并触发 `CP assert ... The queue was full` |
| AcT | Access Technology，接入技术；`+CEREG` 第 5 个字段，11 为 NR SA，13 为 EN-DC/NSA |
| PDP / 承载 | 数据上下文及其网卡（`sipa_eth0`） |
| spool | CP 向 AP 方向的日志/转储通道 |
| profile | 描述某一平台全部板级信息的 TOML 文件 |
| native / vendor 模式 | 前者由本项目执行，后者由厂商守护进程执行、本项目仅记录 |
| 浸泡（soak） | 长时间连续运行（计划要求 72 小时），检验 0 次 assert 与 AT 无静默 |

## 许可

MIT，与本仓库其余部分一致。`src/unisoc_at.rs` 中的本代专有 AT 指令来自对该代 CP 自身
AT 接口的调研；相应契约记录于 `docs/BASEBAND-CONTRACTS.md`。
