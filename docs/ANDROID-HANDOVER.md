# Android 侧基带接管方案

> 适用对象：E5 真机（Android 13，SDK 33，root 可用），用 `unisoc-cpd serve` 取代
> 厂商 `urild` 成为 AT 通道的唯一读者，并由它完成注册、承载与 NAT。
> 依据：`docs/FINDINGS.md` §1–§2（交接规程与栈冷启动），本文记录 2026-09-21
> 三轮冷状态实测（换卡、重启、半途冷启动后遗症）后的完整修订版。
>
> **自动化脚本已被本文替代**：流程对时序敏感（后台进程挂起、冷启动时长），
> 脚本两次在生产设备上表现为"卡住"；以下每一步都是可直接复制执行的单条命令。

---

## 1. 前提

```sh
# 设备上已部署（/data/local/tmp/ucpd/）：
#   unisoc-cpd        最新静态 aarch64 构建（tools/build-aarch64.sh 产出）
#   e5.toml           profile（含 [data.nat] 段）
#   state/            锁与 socket（自动生成）
adb devices        # 设备在线
```

状态目录固定在 `/data/local/tmp/ucpd`，**所有命令都在该目录下执行**；
不要依赖 `--state-dir` 默认值 `/run/unisoc-cpd`——那是 Linux 侧的默认，
Android 上 `/run` 只读。

## 2. 标准交接流程（冷启动全流程）

### 第 1 步：取 root

```sh
adb root                       # 重启后 adbd 会掉回非 root，必须重取
sleep 3 && adb wait-for-device
adb shell id                   # 确认 uid=0(root)
```

### 第 2 步：停厂商 RIL

```sh
adb shell "stop vendor.ril-daemon; sleep 1; ps -A -o NAME | grep urild"
```

**必须用 `stop`，不能用 `pkill`**：urild 是 init 服务，kill 后会被立刻拉起。
`stop` 一次到位，init 不会重生。

### 第 3 步：清理残留（重启用，可跳过；异常恢复时必做）

```sh
adb shell "kill \$(cat /data/local/tmp/ucpd/state/cmd.owner.lock) 2>/dev/null; \
rm -f /data/local/tmp/ucpd/state/cmd.sock \
      /data/local/tmp/ucpd/state/cmd.owner.lock \
      /data/local/tmp/ucpd/state/urc.owner.lock"
```

锁文件里记录持有者 PID；serve 被强杀不会解锁（SIGTERM 不回卷，契约 §9 明说），
所以这里要手动清。

### 第 4 步：启动 serve

```sh
adb shell "( cd /data/local/tmp/ucpd && setsid ./unisoc-cpd serve \
             --profile e5.toml --mode native --state-dir state \
             --runs-dir runs > serve.log 2>&1 < /dev/null & ) ; echo launched"
```

**注意子 shell 包装**：`adb shell "cmd &"` 以 `&` 结尾时，adb 会等后台进程的
文件描述符关闭才返回，表现为脚本"卡住"。`( ... & ) ; echo` 让 adb 立即拿到 EOF。

### 第 5 步：确认 serve 与通道

```sh
adb shell "ps -A -o PID,NAME | grep unisoc-cpd"
adb shell "cd /data/local/tmp/ucpd && timeout 20 ./unisoc-cpd --profile e5.toml \
           --mode native --socket state/cmd.sock cfun status"
```

此步可能 `TIMEOUT`——**不是故障**。重启后立即接管时，CP 自己的 AT 任务
往往还没初始化完（URC 通道已经在流，nr1 命令通道稍后才就绪）。等 30–60 秒重试，
直到读到 `+CFUN: 0`。

### 第 6 步：冷启动 CP（关键步）

RIL 的停机路径会把射频停在 `+CFUN: 0`（FINDINGS §2）。恢复靠 `cfun cold`
（`AT+CFUN=0` → `AT+SFUN=2` → `AT+SFUN=4`）：

```sh
adb shell "cd /data/local/tmp/ucpd && timeout 150 ./unisoc-cpd --profile e5.toml \
           --mode native --socket state/cmd.sock cfun cold"
```

**超时必须给足 150 秒。** 实测冷启动整序列需要几十秒；用 20 秒超时会把它
掐死在半途——CP 显示 `+CGACT: 1,1`、`+CGCONTRDP` 有 IP，但用户面隧道
（UPF 会话）没建起来，症状是 **TX 正常增长、RX 几乎为零**的"僵尸承载"
（见 §4 排查表）。这个状态 `data down/up` 救不回来，只能重新完整冷启动。

### 第 7 步：等注册

```sh
sleep 80
adb shell "cd /data/local/tmp/ucpd && timeout 30 ./unisoc-cpd --profile e5.toml \
           --mode native --socket state/cmd.sock register status"
```

预期：`cereg: 2,1,<TAC>,<CI>,11`（2,1 = 已注册/家庭网络；**AcT 11 = NR SA**），
`c5greg` 同样 `x,1`。没注册上就再等，5 分钟仍不行查 SIM/信号。

### 第 8 步：起承载（自动配置在此发生）

```sh
adb shell "cd /data/local/tmp/ucpd && timeout 90 ./unisoc-cpd --profile e5.toml \
           --mode native --socket state/cmd.sock data up"
```

- **APN 自动选择**：按 IMSI 前缀匹配 profile 的 APN 表（中国移动
  `46000/46002…` → `cmnet`），输出会注明 `source: imsi 46000`；
- **DNS 自动上报**：`+CGCONTRDP` 带出的运营商 DNS（112.4.12.200 / 112.4.1.36）
  如实打印，并写入运行摘要；
- **NAT 自动安装**：`[data.nat].enabled = true` 时 `data up` 自动执行
  主机侧 8 步（两张路由表的默认路由、on-link 路由、MASQUERADE、
  tetherctrl_FORWARD 双向 ACCEPT 对），输出 `nat: 8 of 8 step(s) installed`。

### 第 9 步：验证

```sh
adb shell "ip -o -4 addr show dev sipa_eth0"      # 应看到全新 IP（10.x/8）
adb shell "ping -c 3 -W 3 223.5.5.5"              # 0% 丢包即成功
```

**IP 与上一轮会话不同是好消息**：说明拿到的是新建 PDU 会话，不是僵尸。

### 第 10 步：Web 界面

```sh
adb shell "( cd /data/local/tmp/ucpd && setsid ./unisoc-cpd --profile e5.toml \
             --mode native --socket state/cmd.sock web 0.0.0.0:7887 \
             > web.log 2>&1 < /dev/null & ) ; echo launched"
```

- 热点客户端：`http://192.168.43.1:7887`
- 手机本机浏览器：`http://127.0.0.1:7887`
- 电脑：`adb forward tcp:7887 tcp:7887` 后 `http://127.0.0.1:7887`

---

## 3. DNS：现状与边界（Android 13）

`data up` 之后 **IP 层全通，但域名解析可能失败**——这不是 NAT 的 bug，
而是两层问题：

| 层 | 谁负责 | 现状 |
|---|---|---|
| 数据面（路由/MASQUERADE/转发） | `data nat`（本守护进程） | ✅ 自动完成 |
| 解析面（OS 用哪个 DNS） | netd/DnsResolver（框架） | ❌ 无配置路径 |

具体原因（SDK 33 实测）：

* 解析由 netd 的 per-network 配置驱动，而 ConnectivityService 根本不知道
  我们这张网（`dumpsys connectivity` → `Active default network: none`）；
* `ndc resolver setnetdns` 在此版本已移除（DNS 配置改为 binder 接口，调用方
  是框架）——手动喂 DNS 没有 shell 路径；
* 旧方案 `setprop net.dns1/2` 实测无效（解析器不再读属性）；
* `ndc network create 100` / `interface add` / `default set` 可以成功（fwmark
  路由随之生效），但**不解决 DNS**——只注册了网络，没有 DNS 配置通道。

**当前结论**：

* 热点客户端的 DNS，可用一条 iptables DNAT 解决（把客户端发往
  `192.168.43.1:53` 的查询重定向到运营商 DNS），是否纳入 `[data.nat]`
  作为 profile 可选项，**待定**；
* 手机自身的解析，需要框架层配合（NetworkAgent / ConnectivityService），
  守护进程在契约 §10 的边界内**只如实上报 DNS、不抢解析器**——这一条
  维持不变，除非所有平台路径都穷尽后仍无解，再评估本地转发器方案。

## 4. 故障排查表

| 症状 | 原因 | 处置 |
|---|---|---|
| 能力请求返回 `CHANNEL DOWN` | urild 活着，在吃 tty 字节 | `stop vendor.ril-daemon`（§2 第 2 步） |
| serve.log 刷 `reopen failed: another process owns this channel` | 通道会话僵死（通常伴随 CP 复位） | 杀 serve（按锁内 PID）、清锁与 socket、重启 serve（第 3–4 步） |
| 所有 AT `TIMEOUT`，但 `diag urc` 仍在流 | CP 的 AT 任务未就绪（重启后立即接管） | 等 30–60 秒重试；URC 在流说明 CP 活着 |
| 所有 AT `TIMEOUT` 且无 URC | CP 未启动或通道模式不对 | 确认 root 与 `stop vendor.ril-daemon` 后重来 |
| 注册失败（`cereg: 2,0`） | 射频还停在 `+CFUN: 0` | `cfun cold`（第 6 步） |
| `ping` IP 100% 丢包，接口统计 **TX 涨 / RX 不动** | 僵尸 PDU 会话（半途被掐的冷启动后遗症）：信令 active 但用户面隧道未建立 | 重新 `cfun cold`（完整 150 秒），`data up` 后确认 **IP 变了** |
| IP 直连通、域名解不了 | 解析器无 DNS 配置（§3） | 平台限制，见 §3 的结论 |

## 5. 幂等与回退

* 全流程**可重复执行**：已注册就不再等、NAT 规则 `-C` 先查不会重复插、
  `cfun cold` 只在 `+CFUN: 0` 时需要；
* 回退 = 重启设备（init 拉起 urild，一切回到厂商状态）；`data nat off`
  只撤本守护进程装的规则，不动 `ip_forward` 与主默认路由；
* 长期接管（开机自动）属于 Linux 侧 systemd 方案（`units/`），Android 侧
  目前以本流程手动交接为准。
