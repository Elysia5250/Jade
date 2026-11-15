# 手动验收清单

没有图形界面。可用的"界面"有三个：

| 界面 | 形态 | 适合 |
|---|---|---|
| `envoy` CLI | 终端 | 日常操作、脚本、演示 |
| `envoy ui` | 终端 TUI（opencode 布局） | 手动看任务、跟日志、取消 |
| `envoy mcp serve` | stdio JSON-RPC | Claude/Cursor 等 MCP 客户端 |
| Unix socket | 二进制协议 | 自己写前端时的底层接口 |

下面按"单机 5 分钟 / 语义验证 / 多机"三段走，每段都给了预期输出。全程不需要联网。

```bash
cargo build --release
export ENVOY_DATA_DIR=/tmp/envoy-test     # 别污染 ~/.envoy
B=target/release/envoy                     # 或 target/debug/envoy
```

---

## A. 单机 5 分钟

```bash
$B daemon start          # => daemon started (.../envoy.sock)
$B status                # => enabled/allow/max_jobs/daemon/jobs/protocol: v2

$B run "echo hello; echo err >&2; exit 3"
# => err / hello / exit=3，进程退出码也是 3
echo $?
# => 3

$B submit "sleep 60"
# => 只打印一个 job id 和 "queued #N ..." 就返回，不等

$B jobs
# => ID/STATE/GROUP/OWNER/LOG/COMMAND 表格，该任务 running

$B logs N --follow        # 换个终端也行
$B cancel N               # 另一个终端；logs 那边打印 [#N exit 137]

$B write /tmp/envoy-test/x.txt "hello"
$B read  /tmp/envoy-test/x.txt            # => hello，stderr 报 bytes/mode
$B ls    /tmp/envoy-test
$B copy  local /tmp/envoy-test/x.txt local /tmp/envoy-test/y.txt

$B daemon stop
```

TUI（需要真终端）：

```bash
$B ui
# 左边 job 列表，右边输出，底部命令框，footer 键位提示
# 按 n 输入 echo hi 回车；选中任务按 c 取消；ctrl+p 打开命令面板；ctrl+x q 退出
```

MCP：

```bash
printf '%s\n' \
'{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
'{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"exec","arguments":{"command":"echo from-mcp","wait_s":5}}}' \
| $B mcp serve 2>/dev/null
# => 6 个工具：exec/read/write/edit/ls/job
#    text 为 "from-mcp\n\n[job N exit=0]"
```

---

## B. 语义验证（这才是这次重构的重点）

### B1 断线不等于取消

```bash
ID=$($B submit "sleep 20" 2>/dev/null)
$B logs $ID --follow & W=$!
sleep 0.5; kill -9 $W        # 模拟终端被杀 / 笔记本合盖
$B jobs | tail -1            # => 仍然 running
$B cancel $ID                # => done(137)
```

预期：跟日志的进程死掉，任务照跑。**只有 `cancel` 会杀进程组。**

### B2 日志游标可续读

```bash
$B submit "echo one; sleep 1; echo two" >/dev/null 2>&1
sleep 2
ID=$($B jobs | tail -1 | awk '{print $1}')
$B logs $ID --offset 0     # 全部输出
$B logs $ID --offset 4     # 从第 4 字节开始
```

预期：第二次只打印后半段，且不会重复/丢失记录。

### B3 并发上限与排队

```bash
$B daemon stop && $B daemon start
# 把 max_jobs 改成 1
python3 - <<'EOF'
import json,os
p=os.path.join(os.environ["ENVOY_DATA_DIR"],"agent.json")
c=json.load(open(p)); c["max_jobs"]=1; json.dump(c,open(p,"w"),indent=2)
EOF
$B submit "sleep 5" ; $B submit "echo second"
$B jobs
```

预期：第一条 `running`，第二条 `queued`；5 秒后第二条自动开始。
`max_jobs` 是每次请求重读的，不用重启守护进程。

### B4 重启恢复

```bash
$B submit "sleep 30"
$B daemon stop           # 停守护进程时会杀掉正在跑的进程组
$B daemon start
$B jobs | tail -1        # => done(127)，结果说明是 daemon 重启导致
```

预期：不会出现永久 `running` 的僵尸条目，也不会让 `logs --follow` 无限挂住。

### B4.5 单实例锁

```bash
$B serve &            # 第一个实例拿到 <data-dir>/envoy.lock
sleep 1
$B serve              # => Error: another envoy daemon already owns ... ; 第二个直接退出
$B daemon status      # => running
```

预期：第二个实例不会删掉正在使用的 socket，也不会共享 `jobs/state.json`。

### B5 策略门禁

```bash
$B daemon stop
curl --unix-socket /tmp/envoy-test/envoy.sock http://x/ 2>&1 | head -1
# 不能直接裸 HTTP，仅用于确认 socket 存在；门禁本身用 MCP/远程测试
```

本机 socket 调用恒放行（同用户本就能跑同样命令）；`enabled` 和 allowlist 只约束远程调用方。
要验证拒绝路径见 H 段：目标机 `envoy disable` 后远程调用应返回 `refused: agent disabled ...`。

---

## C. 多机（需要 `conduit run` + 账号）

前提：两台机器都完成 conduit 侧初始化，并且 **`conduit run` 在跑**（它独占设备密钥、
QUIC 端口和 control.sock；不要再起第二个引擎）。

```bash
# 两台都做
conduit init --coord-url https://HOST:7722 --pin PIN
conduit account create --invite CODE     # 第一台
conduit join CODE                        # 第二台
conduit run                              # 常驻

# 两台都做：agent 守护进程 + 3 个空闲 accept 槽
envoy serve
# => envoy daemon listening on ~/.envoy/envoy.sock
#    conduit: watching ~/.conduit/control.sock (3 accept slot(s))
```

发现设备（任一台）：

```bash
envoy devices
# => ID / NAME / DNS / VIP / STATUS / AGENT / ME
# => AGENT=yes 表示该机器当前通告了 service 16（可被调度）
envoy status
# => conduit: connected (...), this device workstations (3fa9...)
#    network:  direct · tun tun0 · 1 peer(s) · vip 100.64.0.1
#    service:  envoy online=true listeners=3
```

目标机放行（用 `envoy devices` 里的 `device_id`）：

```bash
envoy enable
envoy allow 91ff0e2c88aabb11
```

发起方：

```bash
envoy --on laptop run "uname -a"
envoy --on 100.64.0.2 jobs          # 也接受 virtual ip
envoy --on laptop submit "cargo build --release"
envoy --on laptop logs 3 --follow
envoy --on laptop cancel 3
```

预期失败路径（都应该是清晰报错，而不是裸 EOF）：

| 场景 | 预期 |
|---|---|
| `conduit run` 没起 | `connecting to conduit at .../control.sock (is conduit run up?)` |
| 目标机 `envoy disable` | `... refused: agent disabled on this device (envoy enable)` |
| 目标机没 allow 你 | `... refused: caller <device_id> is not allowlisted here` |
| 名字写错 | `... refused: device not found: X [device_not_found, HTTP 404]` |
| 两台设备同名 | `... refused: ambiguous device name 'X' ... [ambiguous_target, HTTP 409]` |
| 目标机离线 | `... refused: ... [peer_offline, HTTP 503]` |
| 目标机没跑 `envoy serve` | `... refused: <name> does not advertise service 16 [service_not_registered, HTTP 503]`（约 1–3 秒内生效，依赖 discovery） |

TUI 多机切换（conduit 已连接时）：

```bash
envoy ui
# header 显示 "2 peer(s)"；ctrl+p → "Switch to laptop (…)" 回车即切目标
# 切换后左下 job 列表清空，后续命令都发往新目标
```

> conduit 的 `/v1/services/{id}/open|accept` 与 `/v1/events` 已落地，本清单已在
> 本地 coord + 两台设备上真机跑通。`crates/envoy-service/tests/conduit_control.rs`
> 另外用 mock 覆盖出站整链路、accept 交接与 allowlist、404/409 透传、
> status/devices 解析。

## D. 当前不该期待的行为

- **没有 GUI**：只有终端 TUI，没有浏览器/桌面窗口。
- **没有 stdin**：任务拿不到交互输入，`stdin` 是 `/dev/null`。
- **没有暂停/恢复**：只有 `cancel`。
- **没有路径围栏**：相对路径落目标机 `$HOME`，绝对路径就是整机可读；"允许调用 ≈ 该用户的全部文件权限"。
- **只有一个默认并发组**：`--group` 可以分组，但每个组的并发上限目前都落到 `max_jobs`。
- **L3 直连不做**：契约已把 agent 通道收敛到 `control.sock` 的 WS 桥；TUN 只给用户自己的工具。
- **`envoy/bridge/` 已弃用**：它链接 conduit crate 并起第二个引擎，与 `conduit run` 冲突。
- **TUI 的机器列表来自 `/v1/devices` + `/v1/events`**；conduit 没起时只显示 `local`。
