# 手动验收清单

没有图形界面。可用的"界面"有三个：

| 界面 | 形态 | 适合 |
|---|---|---|
| `envoy` CLI | 终端 | 日常操作、脚本、演示 |
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

MCP：

```bash
printf '%s\n' \
'{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
'{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run","arguments":{"machine":"local","command":"echo from-mcp"}}}' \
| $B mcp serve 2>/dev/null
# => 11 个工具；text 为 "from-mcp\n\njob N exit=0"
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

### B5 策略门禁

```bash
$B daemon stop
curl --unix-socket /tmp/envoy-test/envoy.sock http://x/ 2>&1 | head -1
# 不能直接裸 HTTP，仅用于确认 socket 存在；门禁本身用 MCP/远程测试
```

本机 socket 调用恒放行（同用户本就能跑同样命令）；`enabled` 和 allowlist 只约束远程调用方。
要验证拒绝路径见 H 段：目标机 `envoy disable` 后远程调用应返回 `refused: agent disabled ...`。

---

## C. 多机（需要 conduit 账号，本仓库无法代替）

前提（conduit 侧，两台设备都要）：

```bash
conduit init --coord-url https://HOST:7722 --pin PIN
conduit account create --invite CODE      # 第一台
conduit join CODE                          # 第二台
conduit status                             # 确认两台都在 active 设备表里
```

然后：

```bash
# 两台都启动 bridge
cd bridge && cargo run --release
# => envoy-bridge listening on ~/.envoy/bridge.sock
#    this device: laptop (91ff0e2c...)
#    peer: workstation (3fa9c1d2...)
```

目标机上放行发起方：

```bash
envoy enable
envoy allow workstation          # 名字，或设备 id
```

发起方：

```bash
envoy --on workstation run "uname -a"
envoy --on workstation submit "cargo build --release"
envoy --on workstation jobs
envoy --on workstation logs 3 --follow
envoy --on workstation cancel 3
```

预期失败路径（都应该是清晰报错，而不是裸 EOF）：

| 场景 | 预期 |
|---|---|
| 目标机 `envoy disable` | `... refused: agent disabled on this device (envoy enable)` |
| 目标机没 allow 你 | `... refused: caller <device_id> is not allowlisted here` |
| 名字写错 | `desktop is not an active device of this account; known: ...` |
| 两台设备同名 | `box matches 2 devices; use a device id instead; known: ...` |
| 目标机离线 | `opening a stream to ...: no session: peer offline or unreachable` |
| bridge 没起 | `connecting to the fabric bridge at .../bridge.sock (is envoy-bridge running?)` |

---

## D. 当前不该期待的行为

- **没有 GUI**：看不到仪表盘、图表、按钮。
- **没有 stdin**：任务拿不到交互输入，`stdin` 是 `/dev/null`。
- **没有暂停/恢复**：只有 `cancel`。
- **没有路径围栏**：相对路径落目标机 `$HOME`，绝对路径就是整机可读；"允许调用 ≈ 该用户的全部文件权限"。
- **只有一个默认并发组**：`--group` 可以分组，但每个组的并发上限目前都落到 `max_jobs`。
- **bridge 的端到端**只有在真实 conduit 账号下才可验证；本仓库的单测只覆盖设备名解析。
