# Envoy 后续接手执行手册（Phase 3 ~ Phase 8）

> **文档定位**：本手册面向后续接手执行的 AI Agent，提供自包含、零歧义的工程规范、阶段拆解、具体改法、高危防踩坑清单、回归测试用例与量化验收指标。  
> **配合文档**：[AGENTS.md](../../AGENTS.md)（全仓工程约束）、[hardening-and-modernization.md](./hardening-and-modernization.md)（总审计台账与进度勾选框）、[architecture.md](../architecture.md)、[decisions/](../decisions/README.md)。

---

## 1. 给接手 Agent 的首轮必读与行为协议

1. **每次只做一个 Phase，严禁跨阶段修改**：用户点名哪个 Phase，只做那一个 Phase；完成后更新 `docs/plans/hardening-and-modernization.md` 的 `- [x]` 勾选框与 `## Next`，立即停止并汇报，等待用户检视后再接下一阶段。
2. **先读后改，严禁猜代码或偷懒**：修改任何文件前必须先完整阅读目标函数上下文（及本文档该 Phase 的 Pitfalls）。
3. **禁止 Git 破坏性/未授权操作**：未经用户明确要求，**严禁**执行 `git commit`、`git push`、`git checkout -- .`、`git reset --hard`。
4. **禁止削弱测试与掩盖告警**：不得删除、注释或放宽任何现有测试断言，不得滥用 `#[allow(clippy::...)]` 掩盖真实 warning。
5. **严禁伪造工具结果**：若某个工具（如本机无 `rustfmt`/`shellcheck`）未安装，必须在汇报中如实说明“未运行及原因”，绝对不可声称已通过。

### 可直接复制给接手 Agent 的首轮启动提示词

```text
请先阅读以下 3 个文件，再开始工作：
1. AGENTS.md
2. docs/plans/handoff-phase3-to-phase8.md
3. docs/plans/hardening-and-modernization.md

严格遵守：
- Phase 1 和 Phase 2 改动已在工作区（未 commit），绝对不要回退其中的任何安全不变量（见 handoff 第 2.3 节）。
- 本次只执行 **Phase X**（按我指定的阶段编号），完成后按第 7 节跑全套门禁命令、更新 hardening-and-modernization.md，然后停下汇报等我检视，不要擅自进入下一 Phase，不要 git commit。
```

---

## 2. 当前仓库状态、环境事实与禁止回退的安全基线

### 2.1 当前代码与测试基线（接手起点）

- **Git 基线**：`dda2b42`（`v0.3.0`）。
- **工作区现状**：Phase 1（`P1-1` ~ `P1-4`）与 Phase 2（`P2-1` ~ `P2-5`）已全部实现并验证，**尚未 commit**，修改集中在工作区中（切勿丢弃工作区修改）。
- **已验证测试基线（2026-11-19 实测全部通过，共 279 passed, 0 failed）**：
  - `envoy-agent`: 146 passed
  - `envoy-cli`: 60 passed（bin） + 2 passed（`tests/stress_headless.rs`）
  - `envoy-core`: 40 passed + 2 passed（`tests/stress_jobs.rs`）
  - `envoy-proto`: 5 passed
  - `envoy-service`: 14 passed + 6（`conduit_control.rs`）+ 3（`remote_policy.rs`）+ 1（`stress_server.rs`）
- **其他基线检查**：`cargo clippy --workspace --all-targets -- -D warnings` 0 warnings；`cargo check --workspace --all-targets` 通过；`python3 scripts/check-file-sizes.py` 退出码 0。

### 2.2 本机环境事实

- 操作系统：Linux（支持 `/usr/bin/bwrap` 与 Landlock）。
- 工具缺失：**当前环境未安装 `rustfmt` / `cargo-fmt` 与 `shellcheck`**（`which` 返回空）。撰写 Rust 代码务必手动保持标准 4 空格缩进与简洁格式；撰写 shell 脚本手动严格自检引号与可移植性；报告如实说明。

### 2.3 绝对禁止回退的 12 条安全不变量（Phase 1 & 2 成果）

后续任何重构（尤其 Phase 3/4/7/8 改写 `exec`、`server`、`search`、`tools` 时）**必须保留**：

1. **`bwrap` 隔离顺序（`crates/envoy-core/src/sandbox/bwrap.rs`）**：必须含 `--unshare-pid --unshare-ipc --unshare-uts --new-session --die-with-parent`；`$HOME/.envoy`、`.conduit`、`.ssh`、`.aws` 等 `--tmpfs` 遮蔽必须在所有 `--bind` 之后；`spec.env` 必须通过 `bwrap --setenv` 注入内部，不得设置在宿主机 `bwrap` 进程上；sentinel 必须在 `0700` 私人目录并 fail closed。
2. **Landlock 与能力模型（`crates/envoy-core/src/sandbox/{mod.rs,landlock.rs}`）**：`FS_READ` 绝不包含 `LANDLOCK_ACCESS_FS_REMOVE_DIR`；`FS_WRITE` 包含 `REMOVE_DIR` / `TRUNCATE` / `IOCTL_DEV`；`ro_paths` 绝不加入 `"/"`；纯 Landlock 后端的 `SandboxCapabilities::enforces_workspace_boundary()` 必须保持 `false`。
3. **Seatbelt（`crates/envoy-core/src/seatbelt.rs`）**：必须阻断 `.envoy`/`.conduit` 读写与控制 Unix socket `network-outbound`，递归大小写无关阻断 `.env*` 读写。
4. **本地进程与文件权限（`envoy-core/src/fs.rs`, `envoy-agent/src/{daemon.rs,runtime/listen.rs,auth/mod.rs}`）**：私有目录 `0700`、敏感文件走 `write_atomic_sync(..., 0o600)`、Unix socket 入口校 `SO_PEERCRED` 同 UID、daemon 启用 `PR_SET_DUMPABLE=0`、`daemon::stop` 强制 `pid > 0 && pid != self`。
5. **远程 RPC 强类型边界（`envoy-core/src/config.rs`, `envoy-service/src/{policy.rs,server.rs,conduit/accept.rs}`）**：保留 `enum Caller { Local, Remote(String) }`，禁止改回 `"local"` 字符串；远程 `Submit` 强制最小 `WorkspaceWrite`；`AgentConfigStore::reload` 缺失配置必须回退禁用。
6. **PermissionEngine 优先级与边界（`envoy-agent/src/permissions/{mod.rs,rules.rs}`）**：优先级固定为 `Managed Deny > Local Deny > Plan Gate > Protected Guard > Ask > Allow > Baseline`；`ENVOY_MANAGED_*` 仅 `#[cfg(test)]` 生效；托管策略/Hook 文件存在但读取或 JSON 解析失败必 **fail closed**；`Effect::Allow` 绝不把未审批 `decision.extra_rw` 注入 `SandboxPolicy`；`Plan` 模式 `exec` 强制 `ReadOnly` 沙箱且无后端直接拒绝，同时禁用 `delegate` 隔离分支。
7. **规则与路径匹配（`permissions/{match_rule.rs,paths.rs}`, `envoy-core/src/search.rs`）**：`rule_matches` 严禁文件名 basename 回退；`is_temp_path` 只检查 `resolved.canonical`；`extract_domain` 用 `reqwest::Url::parse`；`search::resolve_root` 拒绝保护根目录并用 `is_env_filename` 跳过 `.ENV*`。
8. **Shell 分类定位（`permissions/{shell.rs,shell_classify.rs}`）**：只做分类提示；解析失败、`<`/`<>` 重定向、`<(...)`/`>(...)`、复合关键字、`eval`/`env -S`/`LD_*` 必返回 `Err` 或 `Ask`；`find` 的 `-path` 过滤参数（如 `-not -path './.envoy/*'`）保持正确跳过。
9. **Hooks（`hooks/{config.rs,mod.rs,exec.rs}`）**：`HookScope::Project` 禁设 `sandbox: "off"`；`sh -c` + `cmd.env_clear()`；子进程 `pre_exec` 应用 `LandlockPlan`；`fail_closed: true` 遇损坏/截断 JSON 必阻断；`confirm_hook_ask` 默认推荐 `"Deny"`（索引 `1`）。
10. **MCP 服务端（`mcp/{server.rs,server_tools.rs}`）**：相对路径与 `cwd` 在授权前按 `mcp.cwd` 归一化；过滤 `LD_*`/`BASH_ENV`/`PATH` 等危险环境变量；经 `authorize_tool_call` 审计；本地 `job` 与 `resources/*` 按工作区隔离。
11. **MCP 客户端（`mcp/{config.rs,transport.rs,client.rs}`）**：项目级 `.mcp.json` 不得覆盖同名用户级 server；项目级 `safe_stdio_env` 过滤危险键并仅对白名单变量展开 `${VAR}`；项目级 stdio 必进沙箱、HTTP 启用允许 loopback 的 SSRF 校验（`allow_loopback = true`）+ 4 MiB 上限；权限匹配通过 `McpManager::tool_identity` 获取原始 `(server, raw_name)`。
12. **`webfetch` SSRF（`agent/tools/web.rs`）**：通过 `SafeDnsResolver` 原子校验绑定 IP、`.no_proxy()`、封堵 `198.18.0.0/15`/`0.0.0.0/8`/NAT64/尾点主机、3xx 每跳重过 SSRF 与 `authorize_tool_call`。

---

## 3. 全程强制工程规范与防翻车红线

### 3.1 依赖方向与分层红线（Cargo 强制检查）

依赖严格单向：
```
envoy-proto -> envoy-core -> envoy-service -> envoy-agent -> envoy-cli
```
- **禁止**下层依赖上层，**禁止** `envoy-agent` / `envoy-core` / `envoy-service` / `envoy-proto` 引入 `ratatui` 或 UI 类型。
- **禁止** `crates/envoy-cli/src/tui/view/**` 在 Phase 6 完成后存在任何直接文件读写、命令执行、环境变量或 RPC/Provider I/O。

### 3.2 文件行数与函数拆分红线（CI 强制拦截）

`scripts/check-file-sizes.py` 当前设定：**软限制 400 行、硬限制 700 行**（**注意：在 P8-3 修正脚本前，检查器会把同文件内的 `#[cfg(test)]` 测试一并计入行数！**）。

**操作铁律**：
1. 动手改任何 `.rs` 文件前先运行 `wc -l <file>`。
2. 若目标文件已 >= **600 行**，禁止直接大段塞代码或往末尾塞测试！必须先按命名概念拆子模块（或先建独立 `tests_*.rs` 模块）。
3. **当前已接近 700 行红线的高危文件清单（按当前实测行数）**：
   - `crates/envoy-agent/src/mcp/server_tools.rs`：**680 行**
   - `crates/envoy-agent/src/hooks/exec.rs`：**668 行**
   - `crates/envoy-cli/src/tui/tests.rs`：**657 行**
   - `crates/envoy-service/src/client.rs`：**654 行**
   - `crates/envoy-agent/src/auth/routing.rs`：**642 行**
   - `crates/envoy-core/src/job.rs`：**641 行**
   - `crates/envoy-cli/src/tui/view/modals.rs`：**628 行**
   - `crates/envoy-agent/src/agent/context/catalog.rs`：**627 行**
   - `crates/envoy-core/src/search.rs`：**626 行**
   - `crates/envoy-service/src/server.rs`：**619 行**
   - `crates/envoy-agent/src/auth/mod.rs`：**611 行**
   - `crates/envoy-agent/src/agent/context/summary.rs`：**599 行**
   - `crates/envoy-service/src/conduit/mod.rs`：**596 行**
4. 单个函数软限制 **60 行**、硬限制 **100 行**；嵌套深度 **<= 3 层**，多用早返回（early return）；禁止复制粘贴相似分支（抽共享 helper）。

### 3.3 Postcard 线协议红线

- **禁止**在任何 wire 结构体或枚举上使用 `#[serde(skip_serializing_if = ...)]`（Postcard 非自描述，会彻底破坏编解码对齐）。
- 新增或修改任何 wire 类型，必须在对应模块补充完整的 `postcard` round-trip 测试（覆盖全部枚举变体与边界字段）。

### 3.4 并行测试卫生与防孤儿进程

- Cargo 默认多线程并行跑单测：**绝对禁止**在单测里 `std::env::set_var` 修改 `HOME`、`ENVOY_DATA_DIR`、`ANTHROPIC_API_KEY` 等共享全局环境变量。测试环境变量隔离时必须使用**该测试专属唯一变量名**（例如 `ENVOY_TEST_<MOD>_<CASE>`）。
- 临时目录使用 `getrandom` 随机后缀（推荐放 `std::env::temp_dir()`），测试末尾务必 `remove_dir_all`。
- 测试涉及 daemon socket 时优先使用进程内 `UnixListener` + `oneshot` 关闭，**不要**放任测试调 `daemon::ensure` 拉起残留外部后台进程。

### 3.5 已锁定的 5 项架构决策（不得擅自推翻）

1. **`agent.sock` 线协议**：保留 `postcard`（[ADR 002](../decisions/002-postcard-local-wire-protocol.md)），在 Phase 4 补固定版本前置头、解耦 DTO 与持久化结构、并实现流式快照。
2. **非官方 OAuth**：在 Phase 8 修完 CSRF/XSS/刷新并发锁与 env 污染后保留，统一标注 `experimental / unofficial`，产品默认优先引导官方 API Key / Gateway。
3. **`tree-sitter` 依赖**：在 Phase 7 直接从 `envoy-agent` 移除 `tree-sitter` 与 `tree-sitter-rust`。
4. **`envoy-core/src/search.rs`**：按 `AGENTS.md` 窄适配原则，在 Phase 7 引入 `regex` + `globset` + `ignore` 替换当前手写引擎。
5. **`memory.rs` 定位**：在 Phase 8 修复作用域隔离与字符边界截断，保持轻量定位。

---

## 4. 阶段执行依赖图

```
[Phase 1~2 已完成]
       │
       ├──> Phase 3 (执行层 / SSE & Provider / Runtime 状态机 / Worktree / CLI & 脚本)
       │         │
       │         ├──> Phase 4 (线协议版本握手 / 流式快照 / /cd /clear /machine 同步)
       │         │         │
       │         │         └──> Phase 6 (TUI 零渲染 I/O / 事件驱动重绘 / God App 拆解)
       │         │
       │         └──> Phase 5 (Prompt Caching 保鲜 / 无损压缩与 CJK Token / Reasoning 透传)
       │
       └──> Phase 7 (消自环 IPC / 统一 tools / 单模型表 / regex+globset+ignore & 删 tree-sitter) [依赖 Phase 2/4/5 稳定后进行]
                 │
                 └──> Phase 8 (OAuth 隔离修复 / Skills & Memory / 渲染减债 & 日志轮转 & 死代码 & CI)
```

---

## 5. Phase 3 ~ Phase 8 逐任务详细执行规格

---

### Phase 3：执行层、流式 Provider、Runtime 并发与 Worktree 止血

> **目标**：消灭数据静默丢失（日志截断、SSE 中文乱码、半成品合并）、`select! biased;` 饥饿、Runtime 并发双开/越界 panic 与 CLI/安装脚本缺陷。  
> **建议拆成 2~3 个逻辑边界提交（不要提前 commit，但代码结构保持内聚）**。

#### P3-1：Job 执行、`follow_log` 截断与 `envoy-service` 运维修复
- **文件**：`crates/envoy-core/src/{exec.rs,job.rs,log.rs,fs.rs}`、`crates/envoy-service/src/server.rs`、`crates/envoy-agent/src/agent/tools/shell.rs`
- **具体改法**：
  1. **`envoy-service/src/server.rs` (`follow_log` 与 `audit.log` & `whoami`/hostname)**：
     - `follow_log` 在检测到 `finished.is_some()` 后，**不要立刻 `return`**！必须在同一次/循环读取中把剩余未读日志持续推送到真正 EOF（`read_from(cursor)` 返回空切片且 `cursor >= file_len` 或连读无增量）再结束流；当 `jobs.status(id)` 为 `None`（被 prune 清理）且 `follow == true` 时，返回明确的 `Reply::Error` 或结束哨兵而非无限空等死循环。
     - `audit.log` 改为根据实际发送的 `Reply`（是否 `Reply::Error` / `Reply::Denied`）记录 `"ok": true/false`；`reply()` 发送失败不再 `let _ = ...` 静默吞错（至少记录 `tracing::debug!` 并中止对应任务）。
     - hostname 获取优先走跨平台系统调用（如 `libc::gethostname` 或 `std::env::var("HOSTNAME")` 兜底）而非硬读 `/etc/hostname`。
  2. **`crates/envoy-core/src/exec.rs` (`biased;` 饥饿与孙进程管线)**：
     - 把 `cancel.notified()` 与 `tokio::time::sleep_until(deadline)` 放到最优先分支（或去掉 `biased;` 并在 `out_rx.recv()` 分支显式检查 `Instant::now() >= deadline`），确保疯狂刷屏（如 `yes` / `while true; do echo x; done`）绝不饿死取消和超时。
     - 超时/取消后执行 `kill_group(pid)` + `child.wait().await`，并且对管道排空设置严格短暂截止时间（当前 100ms 读可能被继承 stdout FD 的脱管孙进程无限阻塞！需配合 `tokio::time::timeout` 包裹整个 drain 循环，超时即放弃通道而不是每条重置计时器）。
  3. **`crates/envoy-core/src/job.rs` & `log.rs` (`Jobs` 限流与日志分片)**：
     - 为 `Jobs` 增加全组共享运行上限（跨 group 汇总活跃数不超过全局上限），`set_max_jobs` 需遍历更新或同步现有/新建所有 group；`state.json` 解析失败时扫描现有 `logs/*.log` 计算最大 ID 作为 `next_id` 起点（防止从 `1` 开始覆写旧日志！）。
     - `shutdown` 时停止新任务、将未完成终态落盘并短暂等待清理完成。
     - `LogWriter::record(fd, data)` 若 `data.len() > MAX_RECORD`（如 64 KiB 上限），循环切分成多个记录写入，防止写坏记录头长度字段；`JobLogs::prune` 用 `HashSet` 索引并主动删掉已被淘汰的孤立大日志文件。
  4. **`envoy-core/src/fs.rs` (`edit_file`) & `envoy-agent/src/agent/tools/shell.rs` (`exec_tool`)**：
     - `edit_file` 先 `symlink_metadata` / `metadata` 检查为普通文件且大小不超过上限（例如 16 MiB），拒绝编辑管道/设备/超大文件。
     - `exec_tool` 在接收流时用有界头尾环形缓冲截断（如保留前 128 KiB + 后 128 KiB 或复用合理内存上限），避免长时间命令把 16 MiB+ 全量字节堆在内存里后再截断。
- **高危踩坑**：`exec.rs` 排空循环若用 `while let Ok(Some(..)) = timeout(100ms, out_rx.recv())` 遇到每 50ms 吐一字节的孙进程会永远跳不出去！必须把 `deadline = Instant::now() + 200ms` 放循环外部用 `timeout_at`。
- **必写回归测试 & 验收指标**：
  - `server.rs` / 集成测试：单次输出 `> 256 KiB` 后快速 `exit 0` 的命令，通过 `follow_log(follow=true)` 收到的总字节与真实输出 **100% 完整一致**，0 丢失尾部。
  - `exec.rs`：持续高速输出的 `yes` 在 `timeout_s=1` 下 `<= 2.5s` 内稳定返回 `JobResult::TimedOut`。
  - 损坏 `state.json` 后重开 `Jobs`（日志目录下已有 `15.log`），新提交 job ID `>= 16` 不覆写旧日志。
  - `audit.log` 对失败 RPC 记录 `"ok":false`。

#### P3-2：SSE 字节缓冲、Provider 容错与工具并发修复
- **文件**：`crates/envoy-agent/src/agent/providers/{sse.rs,openai_chat.rs,openai_codex.rs}`、`crates/envoy-agent/src/agent/{loop.rs,loop_provider.rs,loop_hooks.rs,prompt.rs}`、`crates/envoy-agent/src/auth/github.rs`
- **具体改法**：
  1. **`providers/sse.rs`**：内部缓冲由 `String` 改为 `Vec<u8>`，按 `\n` 切帧后再把整行转 UTF-8（多字节中文/emoji 跨 chunk 切割绝不产生 ``）；新增 `finish(&mut self) -> Vec<SseEvent>` 在流 EOF 时排空最后未带空行的残留事件；剥离 `"data:"` 后的**单个可选前导空格**（RFC SSE 规范，不要用 `.trim_start()` 误删内容里的合法缩进！）；限制单事件最大字节上限防内存暴涨；用偏移游标替代每行 `drain(..pos)` 消除 O(N²)。
  2. **Provider HTTP 复用、空闲超时与退避重试（`providers/mod.rs` / `loop_provider.rs` / 各 provider）**：
     - 避免每次 turn 重建 `reqwest::Client`，抽取共享 client（配置合理的 connect 超时与流读超时 / chunk idle timeout）。
     - 对幂等请求遭遇 `429` / `502` / `503` / `529` 或瞬时连接错，在尚未向下游发出不可逆输出前进行有限次指数退避重试（如最多 2~3 次，遵守 `Retry-After`）。
  3. **`openai_chat.rs`**：
     - 检查 SSE 事件体中的 `"error"` 对象，非空则显式报错而不是静默忽略 `HTTP 200` 里的错误。
     - 并行 `tool_calls` 累积禁止缺 `index` 时硬编码 `0`（应按出现顺序分配/追踪独立 slot），空 `id` 不覆盖既有 `id`，空 `arguments` 默认规范为 `"{}"`；`stream_options: {"include_usage": true}` 仅对已知支持或按兼容开关发送（或对明确拒绝该字段的兼容网关避免强制注入，注意别破坏现有测试）。
  4. **`openai_codex.rs`**：保持 `tool_calls` 产出顺序（按输出 item/索引而非 `BTreeMap<id>` 字典序打乱）、处理 `response.incomplete` 结束态并传递合理错误/截断信息、正确拼接 `base_url`（避免双斜杠/误截 `/responses`）。
  5. **`auth/github.rs`**：在内存用 `Mutex` / `RwLock` 按 `expires_at`（提前 ~60s 刷新缓冲）缓存 Copilot session token，避免每步请求重复交换 token。
  6. **`agent/loop_hooks.rs` & `prompt.rs`**：
     - `loop_hooks.rs:160` 解析 `tc.arguments` 失败时，不要静默变成 `json!({})` 执行工具！应标记 `prepared.blocked_error = Some(format!("invalid JSON tool arguments for `{}`: {e}", tc.name))` 让错误作为 tool result 回传给模型自我纠正。
     - `execute_tool_batch` 判断 `all_readonly_batch` 时忽略纯展示 `"say"`（或先/单独执行 `say`，使其与并发 `read`/`glob`/`grep` 并存时仍能并行只读工具）；同时给并发只读批加 `Semaphore`（如上限 8~16 并发）防止一次性上百个并行工具打爆文件描述符/连接池。
- **高危踩坑**：SSE 逐字节喂入多字节 UTF-8 时，**不得**按 chunk 调用 `String::from_utf8_lossy`；必须等到 `\n` 边界把完整一行字节转字符串。
- **必写回归测试 & 验收指标**：
  - `sse.rs`：把包含中文/Emoji 的 SSE 帧**逐字节（1 byte/chunk）**喂入，解析内容 100% 原样恢复无乱码；无结尾双 `\n\n` 的尾帧经 `finish()` 正常吐出。
  - `openai_chat.rs` / `openai_codex.rs`：SSE `error` 帧触发 `Err`；多条并行工具调用按产出顺序保留且空 `arguments` 变为 `"{}"`；`response.incomplete` 被正确识别。
  - `loop_hooks.rs`：非法 `tc.arguments` JSON 返回 `blocked_error` 工具错误信息而不变成 `{}` 执行工具。

#### P3-3：Runtime 并发竞态、Checkpoint/Session 持久化与 Conduit 传输修复
- **文件**：`crates/envoy-agent/src/runtime/{server.rs,ops.rs,hooks.rs,monitors.rs}`、`crates/envoy-agent/src/agent/checkpoint.rs`、`crates/envoy-agent/src/session.rs`、`crates/envoy-agent/src/daemon.rs`、`crates/envoy-service/src/conduit/{accept.rs,ws.rs,mod.rs}`、`crates/envoy-cli/src/main.rs`
- **具体改法**：
  1. **`runtime/` 状态机消除竞态（重中之重）**：
     - 在 `Live` 上用原子阶段枚举（如 `enum LiveBusy { Idle, Initializing, RunningTurn, Compacting, Rewinding }`）保护：`send`、`deliver_monitor_fire`、`rewind`、`compact` 都在持锁检查 `Idle` 后立即占位转换状态，**然后释放锁再做异步/磁盘工作**（并用 `scopeguard`/显式收尾保证异常/取消必复位为 `Idle`）。彻底杜绝并发双开 turn、并发 rewind 越界 panic 与 lock poisoning！
     - 登记活跃运行中的 `exec` / job 句柄，`interrupt()` 不仅取消 agent task，还要向下发取消到关联活跃 job。
     - `ensure_with_hooks` 在 `SessionStart` 完成前挂起或明确状态等待，并对齐客户端连接握手超时（保证复杂 hook 不会导致刚起就超时断开）。
     - 为 `Runtime::sessions` 增加空闲驱逐/LRU 上限（保留活跃或最近使用的 N 个会话），给 `Live::subs` 改用**有界 channel**（慢消费者满载丢弃/触发 `SessionReset` 而不是 daemon 内存 OOM）；`MonitorEntry::drop` 时派发取消 owned job。
  2. **`agent/checkpoint.rs` & `session.rs`**：
     - 检查/过滤临时 worktree 路径产生的 `FileChanged` 不污染主会话 checkpoint。
     - `CheckpointStore::save()` 清洗 `session_id`（拒绝 `/`、`..`、控制字符）、使用 `write_atomic_sync(..., 0o600)`、并避免在持锁态做大 JSON 同步磁盘写；`restore` 还原文件前校验目标路径非 symlink 逃逸并用原文件权限写入/原子替换。
     - `session::delete_session` 连带清理对应 checkpoint 文件；严格清洗 `session.id` 防路径穿越；turn 结束时由 **daemon 侧主动安全落盘持久化会话历史**（不再只靠客户端保存）。
  3. **`envoy-service/src/conduit/{accept.rs,ws.rs,mod.rs}`**：
     - `spawn_accept_pool` 收集所有派生 worker `JoinHandle`，在 pool `Drop`/abort 时全部 `abort()` 回收，杜绝任务泄漏。
     - 修复 `WsByteStream` 对 `futures::Sink` 状态机的实现：保留并报告真实 `poll_write` 底层错误，不在错误后继续返回 `Ok(len)`；确保每次 `start_send` 前满足 `poll_ready`，不在 `Close`/`Pong` 处重复重发死循环。
     - `decode_chunked` 用 `checked_add` 防止 chunk 长度整数溢出并设单帧/总读取超时与上限；`watch` 在 `tx.closed()` 时立刻退出。
  4. **`daemon.rs` & `crates/envoy-cli/src/main.rs`**：
     - 修正启动绑定顺序：先准备好 `agent.sock` 内部服务监听再对外就绪/写就绪标识，让 `daemon::ensure` 等待 `agent.sock` 可连接后返回；优雅停机先停接收新连再收尾退出，清理回收子进程防僵尸；`systemd_unit` 中生成的可执行文件/参数路径加双引号防含空格路径炸裂（注意本仓库路径就包含中文字符，生产环境常含空格！）。
- **必写回归测试 & 验收指标**：
  - `runtime` 压测/并发单测：向同一 session 并发发出 2 次 `Send` 或 2 次 `Rewind`，第 2 个明确返回忙拒绝（`AgentResp::Error`）或顺序串行处理，**0 panic、0 孤儿任务、0 锁 poison**。
  - `CloseSession` 后确认其 owned monitor job 被触发取消。
  - `WsByteStream` 注入底层 I/O 错误时返回错误而不是 `Ok(len)`。
  - `decode_chunked` 喂入恶意超大 hex 长度返回显式错误而不溢出。

#### P3-4：Worktree 并行子 Agent 与单树修复
- **文件**：`crates/envoy-agent/src/agent/subagent/runner/{isolate.rs,run.rs,mod.rs}`、`crates/envoy-agent/src/agent/subagent/catalog.rs`、`crates/envoy-agent/src/worktree/{task.rs,mod.rs}`
- **具体改法**：
  1. **`worktree/task.rs` & `isolate.rs`**：
     - `run_tasks` 前置校验主仓库工作区干净且非 detached HEAD；基于当前活跃分支 `HEAD`（`git rev-parse --abbrev-ref HEAD` / `HEAD` SHA，兼容 linked worktree）创建分支。
     - `settle_trees` 必须结合每个任务对应的 `report.ok`：**只有 `report.ok == true` 才提交并合并回主分支**；`report.ok == false`（超时/报错）一律放弃合并（或提示改动保留在哪条隔离枝并移除 worktree，绝不把半成品 `git add -A && git commit && git merge` 进主干！）。
     - 合并成功后删除临时 worktree、临时分支并清除 `trusted.json` 临时条目；遇冲突执行 `git merge --abort` 清理 `MERGE_HEAD`；增加 `Drop` / 取消清理守卫处理父 turn 中途取消时遗留的临时 worktree。
  2. **`catalog.rs` & `run.rs` & `mod.rs`**：
     - 收紧 `SubagentSpec::is_readonly()`：包含 `job`/`todo`/`monitor`/非只读 `mcp__*` 工具时不得判为纯只读。
     - 汇总所有子 Agent 的 `input_tokens`/`output_tokens`（及 cache tokens）回父 turn 事件/总账本；子 Agent 超时退出前优雅关闭其临时 `McpManager`（若支持）。
     - 并行合并 `hook_state` 时采用确定性策略（如按 `index` 有序合入 delta 且不静默用默认值覆盖已有非同名键）。
  3. **`worktree/mod.rs`**：所有 git 根目录/metadata 判定改走 `git rev-parse --show-toplevel` 与 `git rev-parse --git-dir`，支持在仓库**子目录**执行 `/worktree ...` 以及 submodule/linked worktree（`.git` 是文件而非目录）。
- **必写回归测试 & 验收指标**：
  - 模拟两个并行写任务，1 成功 + 1 报错/超时：主分支仅包含成功任务的提交，失败半成品绝对不进主干且临时 worktree 被彻底清理（`trusted.json` 也不残留）。
  - 模拟冲突合并：自动清理 `MERGE_HEAD` 使工作区恢复干净。
  - 在仓库子目录调用 `/worktree` 创建与移除成功。

#### P3-5：CLI、Headless、安装/发布脚本与 Benchmark 修复
- **文件**：`crates/envoy-cli/src/{cli_args.rs,main.rs,headless/mod.rs}`、`scripts/{install.sh,package.sh}`、`.github/workflows/release.yml`、`benchmarks/terminal-bench/envoy_agent.py`
- **具体改法**：
  1. **`cli_args.rs` & `main.rs`**：`TaskArgs::into_request` 在 `self.shell` 下保留完整命令行（`argv.join(" ")`）而非 `argv.remove(0)` 丢弃第 2 个之后全部单词；把全局选项（`model` / `permission_mode` / `sandbox` / `conduit_socket` 等）实际透传至 TUI 启动与 `Runtime` 初始化，无法组合的 flag 在启动前报错提示。
  2. **TUI 模式日志隔离（`main.rs`）**：进入全屏 TUI 前禁止 `tracing` 默认输出到 `stdout/stderr` 毁坏画面——改为写 `<data_dir>/logs/tui.log` 或按开关启用文件 appender。
  3. **`headless/mod.rs`**：出错路径（如 `TurnError`）保留已完成步骤的累计 token/tool 计数并持久化会话，通过显式收尾/守卫确保 `shutdown` hooks 一定执行后再退出。
  4. **`scripts/install.sh` & `package.sh` & `release.yml`**：
     - `install.sh:169` 双引号里未转义的反引号 `` `envoy` `` 会实际执行！改为单引号或转义 `\`envoy\``；替换二进制时先写入 `$DIR/.envoy.new.$$` 再 `mv -f` 原子替换防 `ETXTBSY`；卸载清理逻辑仅精确删除安装的文件和 `~/.envoy`（严禁对 `--dir /usr/local/bin` 误删目录）。
     - `package.sh` 仅清理当前 `$TARGET` 对应产物；`release.yml` 打入 `LICENSE`、启用 `cargo test --workspace --locked`、兼容主流 glibc（或 musl 静态基线）。
  5. **`benchmarks/terminal-bench/envoy_agent.py`**：通过 stdin（`-p -`）传递指令避免超长或以 `-` 开头的 prompt 解析失败；对输出做完整 JSON 解析（`json.loads` 从末段/完整输出提取）而非按单行切割 pretty JSON。
- **必写回归测试 & 验收指标**：
  - `cli_tests.rs`：`envoy run --shell echo hello world` 完整保留 `"echo hello world"`。
  - `headless` 报错用例：报错退出的统计报告保留真实非零用量。
  - `python3 -m py_compile benchmarks/terminal-bench/envoy_agent.py` 通过，且补充解析多行 pretty JSON 的 Python 单元测试通过。
  - 对 `scripts/install.sh` 运行 `bash -n scripts/install.sh` 与 `bash -n scripts/package.sh` 退出码 0（并在报告注明本机无 `shellcheck`）。

---

### Phase 4：线协议演进、版本握手与状态同步

> **目标**：建立版本可诊断的线协议机制，彻底打破 `Snapshot` 8 MiB 帧上限锁死，让 daemon 真正掌握 `/cd`、`/clear`、`/machine` 权威。

#### P4-1：`envoy-proto` 与 `agent.sock` 版本前置握手及解码加固
- **文件**：`crates/envoy-proto/src/{lib.rs,codec.rs}`、`crates/envoy-agent/src/runtime/{protocol.rs,listen.rs,client.rs}`、`crates/envoy-agent/src/daemon.rs`
- **具体改法**：
  1. 在 `crates/envoy-proto/src/lib.rs` 先解析/发送独立固定头（如 `WireHeader { magic: [u8; 4], version: u16 }`），确保版本号位于具体 enum 解码之前：版本不匹配时可明确回传 `Reply::VersionMismatch { server_version, client_version }`（保留清晰可读错误）。
  2. `codec::read_framed` 改 `postcard::take_from_bytes` 校验零尾部残留，并改分块上限校验避免直接按未验帧头一次性预分配 8 MiB `vec!`。
  3. `agent.sock` 引入固定首帧 `AgentHello { protocol_version: u32, incarnation_id: u64, bin_version: String }`：连接时若客户端与运行中 daemon 版本冲突，`daemon::ensure` 返回可读诊断或按安全策略触发平滑重启提示；遇到跨重启的不同 `incarnation_id`，流式客户端必收到 `SessionReset` 清空旧缓存而非错误拼接 `seq`。
  4. 为所有 `AgentReq` / `AgentResp` / `AgentEvent` 变体补齐 round-trip 测试（不得使用 `skip_serializing_if`）。
- **必写回归测试**：版本错配请求返回可读 `VersionMismatch`；跨重启不同 `incarnation_id` 强制发出 `SessionReset`；`AgentReq`/`AgentResp`/`AgentEvent` 全变体 round-trip 通过。

#### P4-2：流式分片 Snapshot 与有界事件回放
- **文件**：`crates/envoy-agent/src/runtime/{server.rs,listen.rs,protocol.rs}`、`crates/envoy-agent/src/agent/{loop.rs,tools/fs.rs}`
- **具体改法**：
  1. 彻底废除单个巨型 `AgentResp::Snapshot` 全量打包！改流式序列：
     `AgentResp::SnapshotStart { session_id, running, seq, total_items } -> AgentResp::SnapshotChunk { ... }* -> AgentResp::SnapshotEnd`（单包严格控制在如 512 KiB~1 MiB 以内）。
  2. 为 `Live::events` 增加 **总字节上限**（例如 8 MiB，不仅限条数）：`AgentEvent::TurnDone` 不再反复克隆内嵌全量历史 `Vec<ChatMessage>`（改传本轮新增/摘要引用），`FileChanged` 仅携带简短摘要或截断上限；当订阅请求的 `from < oldest_available_seq` 时下发 `SessionReset` 重载快照。
  3. 将 `MonitorsSync` 纳入严格单调 `seq` 分配器，消除解锁后复用 `seq_now` 造成的乱序。
- **必写回归测试**：注入超过 16 MiB 的高频 `FileChanged` 与长会话事件流，重连回放全部成功，0 个 `frame too large` 错误。

#### P4-3：打通 `/cd`、`/clear`、`/machine` 同步与严格错误传播
- **文件**：`crates/envoy-agent/src/runtime/{protocol.rs,server.rs,listen.rs,client.rs}`、`crates/envoy-cli/src/tui/{mod.rs,state/commands.rs,state/modals.rs}`
- **具体改法**：
  1. 新增控制请求 `AgentReq::SetCwd { id, cwd }`、`AgentReq::SetTarget { id, target }`、`AgentReq::Clear { id }`（`Ensure` 对已有 session 也同步更新元信息或返回当前 authoritative cwd/target）。
  2. `Send`、`Interrupt`、`Answer`、`SetPermissionMode` 等若找不到对应 `session_id`，**返回明确 `AgentResp::Error`**，严禁继续返回假 `Ack`。
  3. `runtime::tell`（及客户端包装方法）解析服务端 `AgentResp::Error` 转为 `Err`，让 TUI/CLI 显式展示失败原因。
- **必写回归测试**：集成测试在启动会话后发 `SetCwd`/`SetTarget`/`Clear`，验证下一轮 daemon 实际以新 `cwd`、新 `target` 和清空后的上下文执行，且对不存在会话发 `Interrupt` 返回 `Err`。

---

### Phase 5：现代 LLM Harness 对齐（Prompt Caching、无损压缩、Reasoning）

> **目标**：把长对话输入前缀缓存命中率拉回现代标准（>80–90%），消除压缩前的摧毁性截断与 CJK Token 倒挂估算，完整透传多协议 Reasoning / Thinking 块。

#### P5-1：Anthropic 合规化与全链路 KV / Prompt Caching 保鲜
- **文件**：`crates/envoy-agent/src/agent/providers/{anthropic.rs,openai_chat.rs,openai_codex.rs}`、`crates/envoy-agent/src/agent/{prompt.rs,loop.rs,loop_hooks.rs}`、`crates/envoy-agent/src/agent/context/{mod.rs,mask.rs}`
- **具体改法**：
  1. **`anthropic.rs` 请求合规化 & `cache_control`**：
     - 连续相同 `role` 的 `user`（如 `tool_result` 后跟 steering/`hook_context`/步数提示）必须**合并为同一条 `user` 消息的多个 `content` block**，彻底杜绝 Anthropic HTTP 400！同时剔除空 `text` block。
     - 按 Anthropic Prompt Caching 规范对 `system` 最后一段、最后一个 tool schema（若有）以及**稳定对话前缀末尾消息**打上 `"cache_control": {"type": "ephemeral"}`；从 SSE `message_start` / `message_delta` usage 读取并上报 `cache_creation_input_tokens` 与 `cache_read_input_tokens`。
  2. **`openai_chat.rs` & `openai_codex.rs`**：传递稳定的 `prompt_cache_key`（如 session_id 派生哈希），从 `usage.prompt_tokens_details.cached_tokens` 采集缓存读取 token 数上报。
  3. **前缀缓存保鲜（极关键）**：
     - 停止每步原地修改 `system` 与 `messages[0]` 的动态内容（todos、工作区状态变更不要插到 `messages[0]` 或改写 `effective_system`！需要注入时追加在最新末尾位置或仅增量附注 `<environment_update>`）。
     - `PreModelCall` hook 附加内容仅追加到当前请求尾部临时上下文而不改写系统前缀。
     - Stage 1/2 压缩期间**停止**每步在 60%~100% 区间反复原地改写旧工具历史块（只在明确跨越阈值并执行一次不可变清理边界时修改，保证相邻两步之间旧消息字节完全逐字一致）。
     - 在一个 turn 周期内缓存 `skills`/`memory`/`agents` 磁盘快照，避免每个 step 重扫磁盘。
- **必写回归测试**：序列化单测验证 Anthropic 请求体绝不出现连续 `role: "user"` 或空 `text` block 且含合法 `cache_control`；多步模拟 turn 测试检验 step 2 与 step 3（即使 todo 列表变化且处于 65% 上下文）下 `system` 和前 N 条历史消息序列化字节 **100% 一致**。

#### P5-2：上下文压缩保真与 Token 估算重构
- **文件**：`crates/envoy-agent/src/agent/context/{summary.rs,tokens.rs,budget.rs,mod.rs}`、`crates/envoy-agent/src/agent/{loop.rs,loop_provider.rs}`
- **具体改法**：
  1. **`summary.rs`**：彻底移除把用户需求截断为首行 160 字符、工具输出只留前 12 行（丢尾部编译错误）的破坏性 `dossier_from`！改为保留完整切片（超大单条工具输出仅用头尾保留截断 `bound_tool_output`）；摘要请求只发**单条合成指令 + 传空工具列表 `tools: []`**（绝不塞全量 15 个工具和两条连续 `User`）；`merge_summary` 增加硬字数上限（如 16,000 字符）防滚动摘要失控。
  2. **`tokens.rs` & 校准**：修正 CJK/非 ASCII token 系数为合理值（约 `1.7 ~ 2.0` tokens/char，避免低估 2~3 倍）；按当前实际激活工具 schema 序列化长度动态估算 `tools_overhead`（替代硬编码过时的 1400）；`TokenCalibration` 只用**未经过压缩改写的同请求对照样本**更新且持久存回 session，避免压缩前后的对比把校准系数拉垮到 `0.65` 下限。
  3. **Overflow 重试**：收到 provider overflow 后重算 `max_tokens` 并按 `force = true` 执行一次有效裁剪（只要任一阶段释放了空间就允许返回 `Some` 重试）。
- **必写回归测试**：多行用户需求在 `summarize_messages` 传入体中完整保留（含尾部要求）；总结请求中 `tools` 为空；1000 个纯中文汉字文本 token 估算落入 `1500 ~ 2100` 区间；overflow 触发压缩不会污染 `TokenCalibration`。

#### P5-3：Reasoning / Thinking 现代协议对齐与默认策略修复
- **文件**：`crates/envoy-agent/src/agent/providers/{anthropic.rs,openai_codex.rs,openai_chat.rs}`、`crates/envoy-agent/src/agent/effort.rs`、`crates/envoy-agent/src/session.rs`
- **具体改法**：
  1. 在 `ChatMessage` 增加向后/线协议合规的推理载荷结构（或兼容 `Option<Vec<ReasoningItem>>`——**注意配合 Phase 4 版本头且不用 `skip_serializing_if`**），完整保存 Anthropic 的 `thinking` text + `signature` 与 `redacted_thinking` data，以及 OpenAI Responses 的 `encrypted_content` / id。
  2. `anthropic.rs`：收到 `thinking`/`signature`/`redacted_thinking` 块完整保存，并在同轮工具调用续传请求中**原样放在 assistant `content` 数组首位**回传给 Anthropic。
  3. `openai_codex.rs`：`store: false` 请求中携带 `include: ["reasoning.encrypted_content"]`，流式解析捕获加密推理 item 并在后续同轮工具调用回传。
  4. `openai_chat.rs`：不再硬编码 `provider.id == "deepseek"` 才传 `reasoning_content`——改为依据模型/协议目录能力标识决定，并在同轮 tool call 期间回传 `reasoning_content`，而遇到**新一轮用户输入**时按需剥离旧回合历史推理字段。
  5. `effort.rs`：不再把默认 `DEFAULT_LEVEL` 设为 `"low"` 压低推理模型（调整为 `"medium"` 或模型家族推荐默认值）；允许显式 `"off"` 关闭；确保传给兼容协议的字段互斥合法（不在同一请求同时发送互相冲突的 `reasoning_effort` 与 `thinking`）。
- **必写回归测试**：Anthropic tool-use 续传精确回传 `thinking` + `signature` + `redacted_thinking`；OpenAI Responses 请求含 `"reasoning.encrypted_content"`；经任意兼容网关调 DeepSeek-R1 同轮工具调用保留 `reasoning_content` 而新 user turn 剥离。

---

### Phase 6：TUI 薄客户端化、零渲染 I/O 与 God `App` 拆解

> **目标**：严格兑现 [ADR 003](../decisions/003-tui-as-thin-client.md) 与 `AGENTS.md`，使 `tui/view/` 零磁盘/网络/子进程阻塞，消除空转 redraw，修正闪退与终端恢复问题，拆解 59 字段 `App`。
>
> **执行顺序**以 [hardening-and-modernization.md](./hardening-and-modernization.md) 的 `P6-1a`–`P6-3c` 为准。一次只做一项，做完停。下面三段是原范围，子任务是它的切分，不新增目标。
>
> **进度（2026-11-20）**：P6-1a–P6-1e 已落地。`view/` 只读 `inputs` 里的目录与预览快照；`drive::console_loop` 在输入、daemon 事件和动画计时上重绘；工作区索引在 `links` 限深并用 `(dev, ino)` 去重。Phase 6 其余项、Phase 7 与 Phase 8 已在工作区完成，进度以台账勾选框为准。计划到 Phase 8 结束。

#### P6-1：清零 `tui/view/` 同步 I/O，改为事件驱动渲染与后台任务
- **文件**：`crates/envoy-cli/src/tui/{mod.rs,links.rs,diff.rs,mouse.rs}`、`crates/envoy-cli/src/tui/view/**`、`crates/envoy-cli/src/tui/state/{jobs.rs,agent_events.rs}`
- **具体改法**：
  1. **`tui/view/` 全面清除 I/O**：把渲染期间读 `auth.json`、`model.json`、`model_caps.json`、`skills`、`mcp`、`todos`、`bwrap` 存在性、`git show`、文件预览与 `diff` 全数移出 `view::draw`——改为内存只读视图模型快照 `RenderSnapshot` / 状态字段，仅由后台异步任务通过 channel 刷新注入。
  2. 文件预览安全读：区分本地与远程目标（远程通过 `Remote::read` 拉取，本地用 `File::take(FILE_PREVIEW_LIMIT)` 防开百兆大文件 OOM）。
  3. `walk_workspace` 移入后台任务 + 增加最大深度与 `(dev, ino)` 去重防 symlink 循环。
  4. 主循环改事件驱动重绘（`tokio::select!` 等键盘/鼠标事件、daemon 流事件、后台结果、仅在运行加载/解密动画时激活约 30~60fps ticker；完全静止时停止无意义重绘，空闲 CPU 趋近 `0%`）；`snapshot_screen_lines` 仅在激活鼠标文本选区时计算。
  5. `copy_to_clipboard` 丢进异步后台带超时执行（防止子进程挂死卡死 UI）；`agent_events` 节流持久化（或完全交由 Phase 4 的 daemon 负责主落盘）。
- **必写回归测试 & 验收**：用 `grep`/代码审查断言 `crates/envoy-cli/src/tui/view/` 下 **0 次出现** `std::fs`、`std::process::Command`；在测试里对 `10 MB` 假预览文件安全截断不 panic/OOM。

#### P6-2：修复已知 TUI 崩溃、信号泄漏、状态串台与交互 Bug
- **文件**：`crates/envoy-cli/src/tui/{terminal.rs,edit_file.rs,text.rs,mod.rs,mouse.rs}`、`crates/envoy-cli/src/tui/state/{input.rs,edit.rs,commands.rs,agent.rs,modals.rs,ops.rs,jobs.rs,login/**}`、`crates/envoy-cli/src/tui/view/**`
- **具体改法**：
  1. `terminal.rs`：安装 panic hook 与退出清理 `Drop` guard，并注册 `SIGTERM`/`SIGHUP` 处理，保证必恢复终端 raw mode、alternate screen 与键盘增强标志；`edit_file.rs` 挂起前按需对称 pop/re-push `KeyboardEnhancementFlags` 并汇报外部编辑器非零退出。
  2. 滚动偏移统一使用 `usize` 替代 `u16`（防 65535 行回绕）；修复 `push_linked_rows`、`apply_selection_highlight` 与 `area.width == 0` 边界检查（禁止除零与切片越界 panic）；用 `checked_sub` 代替 `Instant::now() - Duration`；恢复 `Focus::Input` 下 `Ctrl+U` 清空行语义。
  3. 抽出单一 `reset_session_view()` 供新建/切换/handoff 共用（必清 `transcript_cache`、`expanded_explore_groups`、`inspector` 选中状态等，且为 `transcript_cache` 加容量上限）；`UiNotice` 绑定 `session_id` 防跨会话串台；`dispatch_agent` 严格排队保序；`/sessions` 弹窗禁止删当前活跃会话；OAuth 回调监听实绑端口回传 `redirect_uri`；`/rewind` 按真正 Turn 边界索引而不再把本地 `!shell` 当作 LLM turn；Picker 空输入不误拦截 `j`/`k` 搜索；对齐 `char_width` 与 `unicode-width` 显示宽度；`mouse.rs` 分发全部鼠标 `Action`；`cancel_active` 只发 `Interrupt` 并等待 daemon 终结事件确认，绝不提前把 `agent_running` 设为 `false`。
- **必写回归测试**：`TestBackend` 渲染测试覆盖 `area.width == 0`、70,000 行滚动、`NewSession` 清缓存、含 Shell 记录的 `/rewind`、`Focus::Input` 的 `Ctrl+U`；跑通 `docs/MANUAL-TEST.md` PTY 检查清单。

#### P6-3：拆解 59 字段 God `App` 与收口 TUI 薄客户端边界
- **文件**：`crates/envoy-cli/src/tui/{mod.rs,types.rs,commands.rs}`、`crates/envoy-cli/src/tui/state/**`、`crates/envoy-cli/Cargo.toml`
- **具体改法**：
  1. 严格按 `AGENTS.md` 把 `App` 拆解为 5 个清晰状态对象组合：
     `AgentState`、`SessionState`、`JobState`、`LoginState`、`UiState`；不再存在平铺 50+ 字段或单个巨型 `impl App` 文件。
  2. `Welcome` 5 个互斥布尔改为单个 `enum WelcomeMode`；统一 `SLASH_COMMANDS` 元数据表与 `run_slash` 分发入口，补齐 `/jobs`、`/logs`、`/output` 等隐藏命令自动补全。
  3. 将 `/mcp probe`、模型目录远端拉取与凭据认证执行迁入 daemon RPC（或 CLI 独立路径），**从 `crates/envoy-cli/Cargo.toml` 移除直接 `reqwest` 依赖**，让 TUI 成为纯粹终端表现层。
- **验收指标**：`cargo check --workspace --all-targets` 通过，`crates/envoy-cli/Cargo.toml` 不含 `reqwest`，每个 TUI 文件保持在 700 行红线以内。

---

### Phase 7：架构瘦身（消除自环 IPC、合并双写工具、单模型表、替换手写轮子）

> **目标**：砍掉高维护成本的胶水与双份实现，显著减少代码体积和漏洞面。

#### P7-1：消除同进程双 Daemon 自环 IPC 与 `EphemeralServer`
- **文件**：`crates/envoy-service/src/remote.rs`、`crates/envoy-agent/src/runtime/{server.rs,ops.rs,monitors.rs}`、`crates/envoy-agent/src/agent/loop.rs`、`crates/envoy-cli/src/{main.rs,headless/mod.rs}`
- **具体改法**：
  - 为 `envoy_service::AgentDialer` / `Remote` 增加进程内直连实现（`LocalDirect` / 内存 `Jobs` + `AgentServer` 直调通道，保留相同 `Caller::Local` 与安全策略语义，但**不再过 `envoy.sock` 自连自己**，也不在 `agent/audit.log` 写多余自环记录）。
  - 彻底移除 `headless/mod.rs` 中的 `EphemeralServer` 与 `/tmp/envoy-eph-*` 临时 socket；整理并统一散落 4 处的 `dialer_for` 样板选择逻辑。
- **必写回归测试**：`headless` 与 `runtime` 测试在无自环临时 socket 下全通过，本机 tool 执行不会向远程服务端 `audit.log` 制造无意义本地自环噪声。

#### P7-2：统一 `agent/tools/` 与 `mcp/server_tools.rs` 重复工具内核
- **文件**：`crates/envoy-agent/src/agent/tools/*` 与 `crates/envoy-agent/src/mcp/server_tools.rs`
- **具体改法**：
  - 抽取一套共享核心执行层（处理 `exec`/`read`/`write`/`edit`/`ls`/`glob`/`grep`/`job` 的路径归一化、工作区隔离、输出上限截断、环境变量过滤）。
  - `agent/tools/`（供 Agent 对话模型调用）与 `mcp/server_tools.rs`（仅保留 MCP `annotations`/`outputSchema`/`structuredContent` 壳适配器）统一调用该核心。
  - **严守 Phase 2 不变量**：确保 `mcp` 服务端相对路径仍按 `mcp.cwd` 归一化、禁止危险 `env`、保留 `"permission requires approval"` 错误文本、`job` 按工作区隔离。
- **必写回归测试**：原有 `tools` 测试与 `mcp::tests_security` 全数通过且行为一致。

#### P7-3：收口 6 份矛盾的模型/供应商注册表并加入内存缓存
- **文件**：`crates/envoy-agent/src/auth/{catalog.rs,routing.rs,mod.rs}`、`crates/envoy-agent/src/agent/context/{catalog.rs,families.rs,budget.rs,reasoning.rs,fetch.rs}`
- **具体改法**：
  - 建立唯一单一来源 `ModelRegistry`（合并厂商、协议、alias、上下文窗口、`pricing_cliff`、推理能力与推荐排序分数），并用进程内 `RwLock` + 文件 mtime/版本号缓存 `model_caps.json`，**彻底消除每次 LLM 请求重复同步读盘解析 JSON**！
  - 修复具体 bug：解禁可用 model ID（如 `"deepseek-chat"`、`"deepseek-reasoner"`）；改词边界/精确前缀匹配（防 `"pro3"` 误匹配 `"o3"`）；`budget.rs` 正确生效 `pricing_cliff`；修复 Codex `/models` `{"models":[{"slug":...}]}` 解析；写入远端发现模型时保留用户 `custom_models`；统一采用 SI `1000` 单位；TUI 与 daemon 统一走一个 `resolve_provider`（正确尊重 env 覆盖）。
- **必写回归测试**：`"deepseek-chat"` / `"deepseek-reasoner"` 可选；`"my-pro3-model"` 不误匹配 `o3`；`pricing_cliff` 触发预期预算阈值；拉取远端 Codex 列表保留既有 `custom_models`。

#### P7-4：替换手写轮子、剔除低效重依赖与重复胶水
- **文件**：`crates/envoy-core/src/search.rs`、`crates/envoy-agent/Cargo.toml`、`crates/envoy-agent/src/agent/context/extract.rs`、`crates/envoy-agent/src/{mcp/mod.rs,auth/cli.rs}`、`crates/envoy-cli/Cargo.toml`、`crates/envoy-agent/src/{skills/loader.rs,agent/subagent/catalog.rs,memory.rs,agent/effort.rs,agent/delegate_pref.rs,skills/prefs.rs}`
- **具体改法**：
  1. 在 `envoy-core/src/search.rs` 引入 `regex` + `globset` + `ignore`（流式受限遍历、遵守 `.gitignore`、保留 Phase 1/2 安全不变量：**绝不扫保护根目录、遵循 `is_env_filename`、绝不跟随 symlink 逃逸**）。
  2. 从 `crates/envoy-agent/Cargo.toml` 移除 `tree-sitter` 与 `tree-sitter-rust`，简化 `extract.rs` 为通用低开销基于缩进/签名的轮廓折叠。
  3. 把仅属 CLI 交互层的 `auth/cli.rs` 与 `mcp/mod.rs` clap 子命令解析迁入 `envoy-cli`，从 `envoy-agent` 移除 `clap`，从 `envoy-cli` 移除未使用的 `futures`。
  4. 抽取共享 YAML frontmatter 解析器（支持行内 `#` 注释）、共享 `now_ms()`、`ScratchGuard` 与安全原子读写 helper（清理无 `#[cfg(unix)]` 保护的 `std::os::unix` 导入）。
- **必写回归测试**：`search.rs` 压测 `{a,b}` 嵌套花括号与 `(foo|bar)baz` 分组正则不 OOM/ReDoS，且继续阻断 `.git`/`.ssh`/`.ENV.local` 检索。

---

### Phase 8：非官方 OAuth 隔离、Skills/Memory 完善、运维轮转与 CI 卫生

> **目标**：加固边缘实验认证模块、补齐 Skills 与 Memory 安全边界、增加日志轮转、清零死代码并对齐 CI 门禁。

#### P8-1：非官方 OAuth 隔离与安全/并发修复
- **文件**：`crates/envoy-agent/src/auth/{oauth_anthropic.rs,oauth_openai.rs,github.rs,refresh.rs,mod.rs}`
- **具体改法**：
  1. 在相关登录界面/CLI 显式标注 `experimental / unofficial`，引导优先使用官方 API Key。
  2. `oauth_anthropic.rs`：强制校验回调 `state == expected_state`（防 CSRF）。
  3. `oauth_openai.rs`：本地 HTTP 回调错误页面彻底 HTML 转义所有查询参数（防反射 XSS）、设置回调读超时；device auth 轮询遇 `403`/`404`/致命错误立即退出并加总超时与取消通道。
  4. `refresh.rs`：用按 provider 分组的进程内异步互斥锁（`tokio::sync::Mutex`）串行化 token 刷新，防止并行子 Agent 互废 rotating refresh token；禁止 `routed` / `ENVOY_BASE_URL` 等临时 env 覆盖回写污染 `auth.json`。
  5. 从 `auth::ModelPrefs` 移除 TUI 外观状态 `theme`、`show_sidebar`，迁入 TUI 本地配置（或专门前端设置文件）。
- **必写回归测试**：错误 `state` 被拒绝；回调包含 `<script>` 返回转义文本 `&lt;script&gt;`；两个并发 refresh 仅向上游发 1 次网络请求；带 `ENVOY_BASE_URL` 刷新后 `auth.json` 中的 `base_url` 未被覆盖。

#### P8-2：Skills 与 Memory 机制收口
- **文件**：`crates/envoy-agent/src/skills/{loader.rs,prefs.rs}`、`crates/envoy-agent/src/memory.rs`、`crates/envoy-agent/src/agent/prompt.rs`
- **具体改法**：
  1. `skills/loader.rs`：修复 `skill_root_kind`（准确区分项目级 vs 用户级 skill）；`walk_rel` 加深度与 `(dev, ino)` 防 symlink 死循环；真正将 `SKILL.md` 的 `allowed_tools` 接入运行时激活过滤/授权（若不启用则移除误导字段）。
  2. `memory.rs`：按当前 workspace 范围过滤注入（不跨不相干项目混用）；按**字符边界**截断并附截断提示（替代 4000 字节硬切中文）；修复 `split_note` 遇到无尾换行符文件 EOF 解析失败。
  3. `prompt.rs`：同时发现 `AGENTS.md` 与 `ENVOY.md` 时合理合并加载而非直接跳过后者。
- **必写回归测试**：symlink 环遍历安全中止不栈溢出；项目 skill 返回 `"project"` 标签；中文长记忆按字符截断不损坏字符；同级 `AGENTS.md` + `ENVOY.md` 同时生效。

#### P8-3：渲染减债、日志轮转、死代码清理与 CI 护栏对齐
- **文件**：`crates/envoy-cli/src/tui/{markdown.rs,markdown_table.rs,highlight.rs,decrypt.rs,text.rs,modals.rs}`、`crates/envoy-agent/src/runtime/{protocol.rs,server.rs,mod.rs}`、`crates/envoy-service/src/{server.rs,remote.rs,conduit/mod.rs}`、`crates/envoy-agent/src/agent/tools/schema.rs`、`crates/envoy-core/src/{audit.rs,fs.rs,log.rs}`、`scripts/{check-file-sizes.py,file-size-baseline.txt}`、`.github/workflows/ci.yml`、`docs/architecture.md`
- **具体改法**：
  1. 评估并使用 `pulldown-cmark` 替换手写 `markdown*.rs`（或确认简化边界）；清理多余特技渲染包袱。
  2. 为 `audit.log`、`permission_audit.jsonl`、`hook_audit.jsonl`、`envoy.out` 增加大小触发单轮转上限（如 10 MiB 轮转为 `.1`）。
  3. 清理已确认的零引用死协议变体、死导出口与死 `#[allow(dead_code)]`（先全局 `grep` 验证确实零引用！）。
  4. 修正 `scripts/check-file-sizes.py` 真正按 `AGENTS.md` 排除测试文件/模块，清理空置 `file-size-baseline.txt`；更新 `docs/architecture.md` 修复错误的审计日志路径描述；升级 `.github/workflows/ci.yml` 增加 `--locked`、`shellcheck scripts/*.sh`、`python3 -m py_compile` 及 MSRV `1.88` 校验（当前 `globset`/`ignore` 无法在 1.85 上解析）。

---

## 6. 全局量化验收指标与阶段测试目标

每个 Phase 必须同时满足 **4 条硬门禁（全部退出码 0）** + **测试数量只增不减**：

| 阶段节点 | 必须通过的自动化命令（全量 0 错 0 警） | 最小工作区测试通过总数 | 人工 / 额外必测项 |
| :--- | :--- | :---: | :--- |
| **起点基线（已完成）** | clippy + check + test + file-sizes | **279 passed, 0 failed** | 无（已验证） |
| **Phase 3 完成后** | 同上 4 条 | **>= 299**（新增 >= 20 回归） | 跑通 [docs/MANUAL-TEST.md](../MANUAL-TEST.md) PTY + `bash -n scripts/*.sh` |
| **Phase 4 完成后** | 同上 4 条 | **>= 308**（新增 >= 9 协议/同步） | 跑通 PTY 断线重连与 `/cd` `/clear` `/machine` |
| **Phase 5 完成后** | 同上 4 条 | **>= 320**（新增 >= 12 缓存/压缩/推理） | 核对多步前缀字节哈希一致断言 |
| **Phase 6 完成后** | 同上 4 条 | **>= 332**（新增 >= 12 TUI/TestBackend） | `grep tui/view/` 零阻塞 I/O + 跑通 PTY 清单 |
| **Phase 7 完成后** | 同上 4 条 | **>= 342**（新增 >= 10 架构整合） | 核对 `envoy-agent` 无 `clap`/`tree-sitter`、`envoy-cli` 无 `futures` |
| **Phase 8 完成后** | 同上 4 条 + `cargo test --workspace --locked` | **>= 354**（新增 >= 12 隔离/运维/CI） | 核对 `ci.yml`、`architecture.md` 与 `check-file-sizes.py` |

### 每阶段标准门禁执行命令

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace --all-targets
python3 scripts/check-file-sizes.py
```

若目标环境已安装 `rustfmt` / `shellcheck`，额外运行：

```bash
cargo fmt --all -- --check
shellcheck scripts/*.sh
```

---

## 7. 阶段完成汇报模板（每次交卷必须严格按此格式停下汇报）

```markdown
## Phase X 完成汇报

### 1. 修改与新增文件清单
- [文件路径](file:///绝对路径) — 一句话改动说明

### 2. 已完成的台账子项
- 已在 docs/plans/hardening-and-modernization.md 勾选：PX-1 ~ PX-N
- 已更新 `## Next` 至：Phase X+1

### 3. 验证命令与真实输出
- `cargo clippy --workspace --all-targets -- -D warnings`：退出码 0，0 warnings
- `cargo test --workspace`：共 XX passed, 0 failed（列出各 crate 明细）
- `cargo check --workspace --all-targets`：退出码 0
- `python3 scripts/check-file-sizes.py`：退出码 0（所有文件 <= 700 行）
- `cargo fmt` / `shellcheck`：本机未安装 / 已通过（严格按真实状态填写）

### 4. 安全与回归检查确认
- 未回退 Phase 1/2 任何不变量；未 run `git commit`。
- 已暂停等待检视，是否进入下一阶段？
```
