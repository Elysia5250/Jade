# Plan: 安全加固、缺陷清零与现代 Harness 架构瘦身

- Status: active
- Owner: Elysia
- Last updated: 2026-11-19
- Baseline: `dda2b42` (`v0.3.0`)

## Goal

基于 `dda2b42` 全仓库四路深度审计结果，分阶段消除当前版本的安全漏洞、数据丢失/协议崩溃 Bug 与高维护成本债，达到“日常可放心使用、长期低成本演进”的工程基线：

1. **安全边界闭环**：OS 沙箱（`bwrap` / `Seatbelt` / `Landlock`）、权限引擎、项目 Hooks、MCP 服务与 `webfetch` SSRF 不再存在已知逃逸与绕过路径，能力模型对不同后端的真实隔离能力诚实声明。
2. **可用性与数据安全**：彻底消除静默丢数据（Job >64 KiB 日志尾部截断、SSE 多字节 UTF-8 乱码、worktree 半成品误合入）、超时/取消饥饿、重连 8 MiB 锁死与运行时并发竞态。
3. **协议可演进与 Daemon 权威**：`envoy.sock` 与 `agent.sock` 具备版本前置握手与分片回放，daemon 真正持有会话持久化与 `/cd`、`/clear`、`/machine` 状态权威。
4. **对齐 2026 现代 LLM Harness**：补齐 Anthropic `cache_control` 与 OpenAI `prompt_cache_key` 并保护前缀缓存命中率；修复压缩前毁上下文与 CJK token 估算反转；完整透传 Reasoning / Thinking 状态并移除默认降档。
5. **兑现 ADR 003 与架构瘦身**：清空 `tui/view/` 同步 I/O、改事件驱动渲染并拆解 59 字段 God `App`；消除同进程自环 IPC、工具双写、6 份互殴的模型表、脆弱手写引擎与死代码。

## 阶段总览（分步拆分与 PR 边界）

| 阶段 | 主题 | 优先级 | 核心涉及 Crate | 建议 PR/Commit 粒度 | 前置依赖 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Phase 1 | OS 沙箱闭环与本地信任边界 | `P0` | `envoy-core`, `envoy-service`, `envoy-agent`, `envoy-cli` | 1–2 PR | 无 |
| Phase 2 | 权限引擎、Hooks、MCP 与 SSRF 安全闭环 | `P0` | `envoy-agent`, `envoy-core` | 2 PR | 依赖 Phase 1 能力模型 |
| Phase 3 | 执行层、流式 Provider、Runtime 并发与 Worktree 止血 | `P0/P1` | 全工作区 + `scripts/` + `benchmarks/` | 2–3 PR | 可与 Phase 2 并行 |
| Phase 4 | 线协议演进、版本握手与状态同步 | `P1` | `envoy-proto`, `envoy-service`, `envoy-agent`, `envoy-cli` | 1–2 PR | 依赖 Phase 3 |
| Phase 5 | 现代 LLM Harness 对齐（Prompt Caching、压缩、Reasoning） | `P1` | `envoy-agent` | 2 PR | 依赖 Phase 3 SSE/Provider |
| Phase 6 | TUI 薄客户端化、零渲染 I/O 与 God `App` 拆解 | `P1` | `envoy-cli` | 2 PR | 依赖 Phase 4 同步协议 |
| Phase 7 | 架构瘦身（消自环 IPC、合并工具层、单模型表、替手写轮子） | `P2` | 全工作区 | 3 PR | 依赖 Phase 2 / 4 / 5 |
| Phase 8 | 非官方 OAuth 隔离、Skills/Memory 完善、运维轮转与 CI 卫生 | `P2` | `envoy-agent`, `envoy-cli`, `scripts/`, `.github/` | 1–2 PR | 可穿插收尾 |

## Progress

### Phase 1: OS 沙箱闭环与本地信任边界

> 目标：堵住 OS 沙箱逃逸与凭证外泄路径，修正 Landlock 错误位掩码与虚假能力标记，收紧本地 Unix Socket 与进程控制边界。
> 目标 commit scope：`fix(core,service,agent,cli): close OS sandbox escapes and harden local trust boundary`

- [x] **P1-1** `[P0/安全]` `bwrap` 隔离闭环与防挂载覆盖
  - **位置**：`crates/envoy-core/src/sandbox.rs:162-178, 198-309`
  - **现状问题**：`build_bwrap` 仅用 `--tmpfs` 遮蔽 `.ssh`/`.aws`，暴露 `$HOME/.envoy`（`envoy.sock`/`agent.sock`/`auth.json`）与 `$HOME/.conduit`（`control.sock`），同 UID 沙箱内可直接连 `envoy.sock` 以 `LOCAL_CALLER` 提交 `SandboxPolicy::Off` 逃逸；未启用 `--unshare-pid`/`--unshare-ipc`/`--unshare-uts`/`--new-session` 导致可读取 `/proc/<daemon-pid>/environ`；`.ssh`/`.aws` 遮蔽在 `--bind` 之前导致 `cwd=$HOME` 时被覆盖解封；`find_env_files` 仅扫一级目录且跳过 `.git` 普通文件（worktree/submodule）；`empty_sentinel_file` 放在 `/tmp/envoy-sandbox-empty` 且失败时 fail-open；`find_bwrap` 先扫 `$PATH` 后查 `/usr/bin/bwrap`。
  - **改造动作**：增加 `--unshare-pid --unshare-ipc --unshare-uts --new-session`；将 `$HOME/.envoy`（及当前 `data_dir`）、`$HOME/.conduit`、`$HOME/.ssh`、`$HOME/.aws`、`$HOME/.gnupg`、`$HOME/.kube`、`$HOME/.codex`、`$HOME/.agents` 的 `--tmpfs` 遮蔽统一后置到所有 `--bind` 之后；限定深度递归扫描 `.env*` 与工作区内 `.git`/`.envoy`（处理普通文件与目录）；将 `empty_sentinel_file` 迁至 `0700` `data_dir` 内并 fail closed；`find_bwrap` 优先检查 `/usr/bin/bwrap` 与 `/bin/bwrap`。
  - **回归验证**：`sandbox.rs` 新增测试断言生成的 `bwrap` 参数顺序中 `--tmpfs $HOME/.envoy`、`--tmpfs $HOME/.conduit`、`--tmpfs $HOME/.ssh` 位于 `--bind` 之后且包含 `--unshare-pid --unshare-ipc --unshare-uts --new-session`，验证嵌套 `.env` 与 `.git` gitfile 均生成对应 `--ro-bind`；在支持 `bwrap` 的 Linux 环境验证沙箱内无法连接 `envoy.sock` 且无法读取 `auth.json`。

- [x] **P1-2** `[P0/安全]` macOS `Seatbelt` 凭证与保护路径补漏
  - **位置**：`crates/envoy-core/src/seatbelt.rs:84-185`
  - **现状问题**：仅隐藏 `.ssh` 与 `.aws`，未拒绝访问 `$HOME/.envoy`（含 `auth.json` 与控制 socket）、`$HOME/.conduit`、`$HOME/.codex`、`$HOME/.agents`；`.env*` 规则仅 `deny file-read*` 未禁写、仅单层且正则区分大小写（macOS 默认 APFS 大小写不敏感可被 `.ENV` 绕过）；工作区内 `.ssh`/`.aws`/`.envoy` 仅禁写不禁读。
  - **改造动作**：在 `build_profile` 中对 `$HOME/.envoy`（及 `data_dir`）、`$HOME/.conduit`、`$HOME/.codex`、`$HOME/.agents` 统一添加 `file-read* file-write*` 拒绝及本地控制 socket `network-outbound` 拒绝；`.env*` 使用大小写不敏感匹配覆盖任意深度并同时禁止 `file-read*` 与 `file-write*`；工作区内 `.ssh`/`.aws`/`.envoy` 同时禁止读写。
  - **回归验证**：`seatbelt.rs` 单元测试断言生成 profile 包含 `.envoy`/`.conduit` 读写拒止、`.ENV`/子目录 `.env` 匹配以及工作区敏感目录读写拒止；CI macOS 矩阵验证真实 `sandbox-exec` 拦截。

- [x] **P1-3** `[P0/安全]` Linux Landlock 位掩码修复与显式能力模型
  - **位置**：`crates/envoy-core/src/sandbox.rs:43-66, 311-467`、`crates/envoy-agent/src/permissions/rules.rs:280-285`
  - **现状问题**：`FS_READ` 误包含 `1 << 4`（`LANDLOCK_ACCESS_FS_REMOVE_DIR`），导致即使 `ReadOnly` 沙箱也对全盘 `/` 开放 `rmdir`；`FS_WRITE` 与 `fs_mask` 漏掉 `1 << 14`（`TRUNCATE`）和 `1 << 15`（`IOCTL_DEV`），使沙箱外文件可被 `truncate(2)`；`add_paths` 静默忽略错误；`ro_paths` 放入 `"/"` 使 Landlock 加法语义下无法收回 `.git`/`.env`/`~/.ssh`；`sandbox_backend_present()` 把差异极大的三个后端折成单个 `bool`，让纯 Landlock 也解锁了 `Auto`/`Default` 自动放行。
  - **改造动作**：修正 `FS_READ = (1 << 2) | (1 << 3)`，将 `1 << 4` 移入 `FS_WRITE` 并补齐 `1 << 14`（`abi >= 3`）与 `1 << 15`（`abi >= 5`）；`add_paths` 对非预期错误 fail closed；`ro_paths` 移除 `"/"` 改为显式系统只读根（如 `/usr`、`/lib`、`/lib64`、`/bin`、`/etc` 等，排除 `$HOME`）；新增带 `OnceLock` 缓存的 `SandboxCapabilities { can_hide_secrets, can_protect_subpaths, can_isolate_net, can_isolate_ipc }`，禁止无法保护子路径/秘钥的纯 Landlock 后端自动解锁 `Auto`/`Default` 免确认。
  - **回归验证**：单元测试核验 `FS_READ`/`FS_WRITE`/`fs_mask` 与系统头文件常量一致；在 Linux 上验证 Landlock `ReadOnly` 下 `rmdir` 与 `truncate` 被内核 `EACCES`/`EPERM` 拒绝；验证无 `bwrap` 仅 Landlock 时 `Auto`/`Default` 不会免问放行敏感命令。

- [x] **P1-4** `[P0/安全]` 本地 Socket/数据文件权限、`daemon::stop` 守卫、远程 `Caller` 类型安全与脱敏修复
  - **位置**：`crates/envoy-cli/src/main.rs:358-381`、`crates/envoy-cli/src/headless/mod.rs:123-132`、`crates/envoy-agent/src/daemon.rs:290-334`、`crates/envoy-agent/src/runtime/listen.rs:17-29`、`crates/envoy-core/src/config.rs:51-118`、`crates/envoy-core/src/fs.rs:122-132`、`crates/envoy-service/src/conduit/accept.rs:40-51`、`crates/envoy-service/src/server.rs:133-168, 397-469`、`crates/envoy-agent/src/auth/mod.rs:256-286`、`crates/envoy-agent/src/permissions/audit.rs:53, 88`、`crates/envoy-agent/src/hooks/mod.rs:158-165`
  - **现状问题**：`<data_dir>` 与 `/tmp/envoy-eph-*` 默认 `0755` 且 socket 先 `bind` 后 `let _ = set_permissions(0o600)`，无 `SO_PEERCRED` 校验与 `PR_SET_DUMPABLE=0`，`sessions/`/`jobs/`/`audit.log` 在默认 umask 下全员可读；`daemon::stop` 未校验 `pid > 0`（`-1` 杀用户全部进程，`0` 杀进程组）；Conduit 入站直接把 `peer_device_id` 传给 `serve_stream` 与魔法串 `LOCAL_CALLER = "local"` 混用；远程 `AgentServer` 允许对端传 `SandboxPolicy::Off`、读敏感文件、无上限/无超时写入并开无限连接；`AgentConfigStore::reload` 在 `agent.json` 被删时保留旧配置；`save_auth`/`save_prefs`/`write_atomic`/`AgentConfigStore::save` 用固定 `.tmp` 名且在 `0600` 前落盘；`redact_secrets` 按空白分词导致 `starts_with("Bearer ")` 成为死代码，hook `trace[i].reason` 未脱敏。
  - **改造动作**：创建 `<data_dir>` 与临时目录强制 `0700`（`DirBuilderExt::mode`），统一原子安全写 helper（随机/PID 唯一 `.tmp` + 创建即 `0600` + `fsync` + `rename`），迁移 `auth.json`、`model.json`、`agent.json`、`sessions`、`checkpoints`、`trusted.json`、`todos`；Unix socket 监听校验 `set_permissions` 错误并在连接入口强制校验 `peer_cred().uid() == geteuid()`；daemon 启动时设 `PR_SET_DUMPABLE=0`；`daemon::stop` 强制 `pid > 0` 且校验进程/锁；用 `enum Caller { Local, Remote(String) }` 替换 `"local"` 字符串；远程 `AgentServer` 强制最小 `WorkspaceWrite` 沙箱底线、拒绝读取 `.ssh`/`.aws`/`.envoy`/`.env`、为 `serve_write` 增加文件上限与帧读超时、加连接并发上限、`reload` 文件缺失回退安全默认值；修复 `Bearer <token>` 分词前匹配与 hook `reason` 脱敏。
  - **回归验证**：单测覆盖 `daemon::stop` 对 `0`/`-1` PID 文件的拒绝、`Caller::Remote("local")` 不绕过 `gate()`、远程 `Submit` 传 `SandboxPolicy::Off` 被拒绝/提升、`reload` 删除配置文件后禁用远程访问、`redact_secrets` 正确打码 `Authorization: Bearer ...`、Linux/macOS 验证 `data_dir` 与 `auth.json` 权限为 `0700`/`0600`。

---

### Phase 2: 权限引擎、Hooks、MCP 与 SSRF 安全闭环

> 目标：消除权限规则、路径解析、Shell 分类、项目 Hooks、MCP 与 `webfetch` 上的系统性绕过。
> 目标 commit scope：`fix(agent,core): close permission, hooks, MCP, and webfetch SSRF bypasses`

- [x] **P2-1** `[P0/安全]` `PermissionEngine` 规则、路径与模式边界绕过修复
  - **位置**：`crates/envoy-agent/src/permissions/{rules,paths,prompt,mod}.rs`、`crates/envoy-core/src/search.rs:97-105`、`crates/envoy-agent/src/agent/tools/{mod.rs:162-163, monitor.rs:78-85, shell.rs:118-194}`、`crates/envoy-agent/src/agent/subagent/runner/isolate.rs:13-83`、`crates/envoy-agent/src/hooks/mod.rs:237`
  - **现状问题**：`Auto` 模式把 `exec` 参数里的非工作区 `cwd` 和写路径自动注入 `extra_rw` 且跳过 `outside_workspace` 检查直接放行；规则匹配复用 `glob_match` 的 basename 回退导致 `Bash(cargo test*)` 匹配 `Bash(curl http://evil/cargo test)`，`suggested_rule` 对 `sh -c` 生成过宽放行规则；`protected_hit` 漏掉 `ls`/`glob`/`grep` 且 `search::walk_files` 不过滤根路径，导致 `grep ~/.ssh` / `grep ~/.envoy` 在 `Auto` 下直接放行；`is_temp_path` 检查 `resolved.requested` 而非 `canonical` 导致 `/tmp/symlink_to_home/.bashrc` 在 `Auto` 下直接写入；`extract_domain` 先按 `:` 再按 `@` 切分，可被 `https://allowed.com:443@evil.com/` 欺骗；`Plan` 模式下 `delegate` 仍可触发 `worktree` 建分支合并且 `Default` 下带沙箱即自动放行变更性 `exec`；`prompt.rs:108` 把 `"expanded path"` 误映射为 `OnceWithNet`、`125` 行 `glob`/`grep` 摘要错读 `args["root"]` 而非 `args["path"]`；`confirm_hook_ask` 默认推荐 `"Allow once"`；`ENVOY_MANAGED_*` 环境变量在 release 构建生效且畸形 JSON fail-open；`ask` 跳过统一授权、`monitor` 不触发 `PreToolUse("exec")`、`job` 缺会话归属。
  - **改造动作**：`Auto` 模式禁止由未审批参数自动填充 `extra_rw` 且 `outside_workspace` 必返回 `Ask`；规则引擎使用无 basename 回退的专用匹配器并禁止为 `sh -c`/`bash -c` 生成通配保存规则；将 `protected_hit` 扩展到 `ls`/`glob`/`grep` 并在 `search::resolve_root` 直接拒绝敏感根；`is_temp_path` 仅校验 `resolved.canonical`；`extract_domain` 改为 `reqwest::Url::parse` 解析 `host_str()`；`isolation_applies` 检查非 `Plan` 模式，且 `Plan` 模式调用 `exec` 时强制配合 OS `ReadOnly` 沙箱（或在无只读沙箱时全拒 `exec`）；修复 `prompt.rs` 选择项映射（分离 `OnceExpanded` 与 `OnceWithNet`）和 `args["path"]` 读取；`confirm_hook_ask` 默认推荐索引设为 `Deny`；`ENVOY_MANAGED_*` 仅 `#[cfg(test)]` 生效，且正式环境托管策略/Hook 文件损坏时 fail closed；补齐 `monitor` 的 `PreToolUse("exec")`、全局 `job` 会话归属检查与 `ask` 审计。
  - **回归验证**：在 `permissions/tests.rs` 为上述每项绕过补齐回归用例（`Auto`+`extra_rw`、`cargo test*` 注入、`grep ~/.envoy`、`/tmp` 符号链接越界、`userinfo@evil.com`、`Plan`+`delegate` 编码任务、损坏 `managed_policy.json` fail-closed）。

- [x] **P2-2** `[P1/安全]` Shell 分类器降级为辅助提示并封堵已知语法绕过
  - **位置**：`crates/envoy-agent/src/permissions/{shell,shell_classify}.rs`
  - **现状问题**：`cat <.env`（未解析 `<`/`<>` 输入重定向）、`cat .en*`（通配符跳过路径检查）、`cat <(rm -rf .)`（忽略进程替换）、`rg --pre <cmd>`、`find -fprintf/-fprint/-fls/-okdir`、`sed 'w ...'`、`git branch -D`、`git diff --output=...`、`env -S "..."`（空段被静默丢弃）、前导 `LD_PRELOAD=...`、以及 `{ ...; }` / `!` / `if` / `eval` / `python3 -c` / `node -e` 均会绕过分类，导致本该拦截或确认的命令被放行。
  - **改造动作**：明确架构定位——不再以 `analyze_shell` 结果决定沙箱 `extra_rw`；补全 `<`/`<>` 重定向提取、通配符令牌路径匹配、进程替换 `<(...)`/`>(...)` 识别与空段/复合结构/未知语法一律降级为非常规写命令（触发 `Ask`，不在 `Plan`/`Default` 自动放行）；将 `Plan`/`Default` 免问只读白名单收窄为严格“命令+允许参数白名单”，任何未列明参数一律返回非只读。
  - **回归验证**：新增针对 `<.env`、`.en*`、`<(cmd)`、`rg --pre`、`find -fprintf`、`sed w`、`env -S`、`LD_PRELOAD=x ls`、`{ rm -rf .; }` 的分类回归测试。

- [x] **P2-3** `[P0/安全]` 项目 Hooks 补回 Landlock、隔离环境变量并禁止自关沙箱
  - **位置**：`crates/envoy-agent/src/hooks/{exec.rs:41-146, 407-423, config.rs:46-103}`
  - **现状问题**：`HookLaunch` 丢弃了 `prepared.landlock`；`run_hook_process` 未调 `cmd.env_clear()` 使项目 Hook 继承 daemon 完整环境变量；`.envoy/hooks.json` 允许通过 `"sandbox": "off"` 自关沙箱；`sh -lc` 启动登录 shell 污染输出与环境；`fail_closed: true` 时畸形/截断 JSON fail-open 放行。
  - **改造动作**：`HookLaunch` 保存并由子进程 `pre_exec` 应用 `landlock`（或复用受控沙箱执行器）；`run_hook_process` 强制 `cmd.env_clear()` 后仅注入安全基础环境变量与 `ENVOY_HOOK_*`；项目级 `HookScope::Project` 禁止自设 `sandbox: off`（仅受管或用户显式授权配置可放开）；改用 `sh -c`；`fail_closed: true` 且输出看似 JSON 但解析失败/被截断时返回阻断决定。
  - **回归验证**：`hooks/` 单测覆盖：项目 Hook `sandbox: "off"` 被忽略、Linux 无 `bwrap` 时携带非空 `landlock` 计划、环境不包含宿主 `ANTHROPIC_API_KEY`、`fail_closed` 遇到截断 JSON 返回阻断。

- [x] **P2-4** `[P0/安全]` MCP 服务端与客户端安全闭环
  - **位置**：`crates/envoy-agent/src/mcp/{server.rs:175-302, server_tools.rs:200-568, config.rs:83-247, client.rs:77-79, 221-244, transport.rs:86-254}`、`crates/envoy-core/src/exec.rs:75-77`
  - **现状问题**：MCP server 用 `mcp.cwd` 检查相对路径但发送未解析路径到实际以 `$HOME` 为基准的 `envoy_core::fs::resolve`，形成完整越狱；MCP `args["env"]` 未过滤直接注入宿主机上的 `bwrap` 启动环境（可传 `LD_PRELOAD`/`BASH_ENV`）；`call_tool` 跳过 `authorize_tool_call` 与审计；`resources/*` 与远程 `machine` 未受限；MCP 客户端允许项目 `.mcp.json` 通过 `${ENV_VAR}` 窃取任意宿主环境变量、允许项目配置覆盖同名用户级 server 继承权限、项目 stdio server 不进沙箱、`qualify_tool_name` 在含 `__` 或超 64 字符截断后破坏 `Mcp(...)` 规则匹配，且 stdio/HTTP 读取无大小上限。
  - **改造动作**：MCP 服务端所有文件与 `cwd` 相对路径先以 `mcp.cwd` 规范化再交权限检查和执行；严格过滤 `args["env"]`（拒绝 `LD_*`/`DYLD_*`/`BASH_ENV`/`ENV`/`IFS`/`PATH` 等危险变量，并仅在沙箱内层应用）；统一通过 `authorize_tool_call` 执行权限与审计；为 `resources/*` 与 `machine` 增加权限检查；MCP 客户端对 `McpScope::Project` 限制 env 展开白名单、禁止覆盖已有用户/托管级 server 名、为项目 stdio server 套用工作区沙箱、直接传递结构化 `(server, raw_tool)` 供规则匹配，并给 stdio 行长/HTTP 响应加硬上限及项目 HTTP URL SSRF 校验。
  - **回归验证**：新增 MCP 集成测试覆盖相对路径 `.bashrc` 拒绝越界读写、`LD_PRELOAD` 被拒绝、项目 `.mcp.json` 无法读出 `ANTHROPIC_API_KEY`、无法同名覆盖用户 MCP、长工具名/`__` 仍命中 `Deny: Mcp(...)`。

- [x] **P2-5** `[P1/安全]` `webfetch` SSRF、DNS Rebinding、TUN Fake-IP 与重定向防护
  - **位置**：`crates/envoy-agent/src/agent/tools/web.rs:17-176`
  - **现状问题**：验证 DNS 解析结果后即丢弃，交给 `reqwest` 二次解析存在 DNS Rebinding TOCTOU；未阻止 `198.18.0.0/15`（Clash/mihomo/sing-box TUN Fake-IP 常用段）、`0.0.0.0/8`（如 `0.0.0.1`）、`192.0.0.0/24` 与 IPv6 `64:ff9b::/96` / `64:ff9b:1::/48` / `::/96` / `2002::/16` / `fec0::/10`；未剥离尾点 `localhost.`；未调用 `.no_proxy()`；最多 5 跳跨站重定向不重过 `PermissionEngine`。
  - **改造动作**：实现自定义 `reqwest::dns::Resolve` 在真正建连前原子校验并固定已解析 `SocketAddr`；构建客户端启用 `.no_proxy()`；主机名归一化去尾点；补全上述 IPv4/IPv6 保留与 Fake-IP 段黑名单；改为手动处理重定向每一跳，对每个 `Location` 重新校验 SSRF 与 `PermissionEngine::authorize_tool_call`。
  - **回归验证**：单测验证 `198.18.0.1`、`0.0.0.1`、`64:ff9b::127.0.0.1`、`localhost.` 被阻断，且模拟本地 HTTP 302 跳转到被 `Deny` 域名或内网 IP 时被立即拦截。

---

### Phase 3: 执行层、流式 Provider、Runtime 并发与 Worktree 止血

> 目标：消灭已知的丢数据、死锁/饥饿、UTF-8 乱码、半成品误合与 CLI/脚本缺陷，稳住日常单机与远程执行链路。
> 目标 commit scope：`fix(core,service,agent,cli): fix log truncation, select starvation, SSE UTF-8, runtime races, and worktree/CLI bugs`

- [x] **P3-1** `[P0/核心]` Job 执行、`follow_log` 截断与 `envoy-service` 运维修复
  - **位置**：`crates/envoy-core/src/{exec.rs:127-175, job.rs:81-109, 135-144, 160-164, 210-340, 418-459, log.rs:26-180, fs.rs:86-131}`、`crates/envoy-service/src/server.rs:155-328, 539-556`、`crates/envoy-agent/src/agent/tools/shell.rs:47-115`
  - **现状问题**：`follow_log` 读单批 64 KiB 后遇到 `finished.is_some()` 直接 `return` 丢弃剩余日志尾部，且被 prune 掉的 job 会让 `follow=true` 死循环；`exec.rs:151` `select! { biased; }` 把 `out_rx.recv()` 放第一分支，刷屏命令饿死超时、取消与退出检测，且继承管道孙进程可泄漏读取任务；`audit.log` 因误差都通过 `reply()` 返回 `Ok(())` 而恒记 `"ok": true`；`Jobs` 未全局限流、`set_max_jobs` 漏非默认组、`state.json` 损坏直接重置 `next_id=1` 覆写旧日志、`shutdown` 不等 `ScratchGuard`；`LogWriter::record` 写 >64 KiB 会毁坏日志头、`JobLogs::prune` O(N*M) 且保留 1 小时已淘汰大日志；`fs::edit_file` 无大小限制和非常规文件检查；`exec_tool` 内存中全量拼接最大 16 MiB 输出。
  - **改造动作**：`follow_log` 在确认 `finished.is_some()` 后循环排干日志到 EOF 再结束，并检测 job 已被清理时返回明确的结束/缺失态；`exec.rs` 将 `cancel.notified()` 与 `sleep_until(deadline)` 放到 `biased;` 之前或去掉 `biased;` 并在输出分支检查截止时间，处理孙进程管道超时回收；`envoy-service` 按真实 `Reply` 结果写 `audit.log`、`reply()` 记录写错误、使用系统 API 替代 `/etc/hostname` 兼容 macOS；为 `Jobs` 增加跨组全局运行上限与 `set_max_jobs` 同步、保留已有日志序号不覆写、`shutdown` 持久化终态并等待清理；`LogWriter::record` 自动按 `MAX_RECORD` 分片、`JobLogs::prune` 使用 `HashSet` 且主动清理已淘汰任务；`edit_file` 加文件类型与大小限制；`exec_tool` 在接收流时即施加双端/尾窗截断上限。
  - **回归验证**：集成测试覆盖：单次输出 >256 KiB 快速退出的命令完整回收全部日志、无限 `yes` 在 `timeout_s=1` 时按时中止、失败 RPC 在 `audit.log` 记录 `"ok": false`、损坏 `state.json` 不重置 `next_id` 覆写既有日志。

- [x] **P3-2** `[P0/Provider]` SSE 字节缓冲、Provider 容错与工具并发修复
  - **位置**：`crates/envoy-agent/src/agent/providers/{sse.rs:16-38, openai_chat.rs:38-196, openai_codex.rs:40-189}`、`crates/envoy-agent/src/agent/{loop.rs:98-100, loop_provider.rs:51-59, loop_hooks.rs:160, 378-405, prompt.rs:22}`、`crates/envoy-agent/src/auth/github.rs:110-138`
  - **现状问题**：`SseParser` 直接对网络切片做 `from_utf8_lossy` 破坏跨包中文/emoji，且无 EOF `flush()` 丢最后一条事件、每行 `drain` 为 O(N²)；每次 turn 重建 `reqwest::Client` 且无流读取空闲超时，无 429/502/503/529 退避重试；`openai_chat` 吞掉 HTTP 200 SSE `error` 帧、缺 `index` 默认 `0` 撞坏并行 tool call、空 `id` 覆盖、空 `arguments` 未兜底 `"{}"`、无条件传 `stream_options`；`openai_codex` 用 `BTreeMap` 按 `id` 重排并行工具、忽略 `response.incomplete`、硬拼不规范 `base_url`；Copilot 每步重请求 session token（未按 `expires_at` 缓存）；`loop_hooks.rs:160` 遇到畸形工具参数 JSON 静默变成 `{}`；`prompt.rs` 要求与只读工具同时发 `say` 反手破坏 `all_readonly_batch` 并发且 `join_all` 无并发上限。
  - **改造动作**：`SseParser` 内部改用 `Vec<u8>` 缓冲直到完整换行/UTF-8 边界，新增 `finish()` 排空末条事件、仅剥离标准规范单空格、设置单帧上限；全局/Session 复用带读空闲超时的 `reqwest::Client` 并增加幂等 429/5xx 指数退避重试；修复 `openai_chat` / `openai_codex` 工具调用累积保序、SSE 错误检查、`response.incomplete` 处理与 `/responses` URL 规范化；在内存按 `expires_at` 缓存 Copilot session token；工具参数 JSON 解析失败时把错误结构化返回给模型重试而非塞 `{}`；调整并发判定忽略纯展示类 `say`（或将其提示语优化），并给并发只读批加合理并发度限制（如 `Semaphore`）。
  - **回归验证**：单测验证多字节中文 UTF-8 字符被拆成单字节喂入 `SseParser` 零损坏、无双换行尾帧在 `finish()` 输出、`openai_chat` 缺 `index`/SSE `error` 正确处理、`openai_codex` 并行工具保序、`loop_hooks` 畸形 JSON 返回工具错误信息。

- [x] **P3-3** `[P1/并发]` Runtime 并发竞态、Checkpoint/Session 持久化与 Conduit 传输修复
  - **位置**：`crates/envoy-agent/src/runtime/{server.rs:47-507, ops.rs:38-146, hooks.rs:26-82, 234-270, monitors.rs:31-38, 244-276}`、`crates/envoy-agent/src/agent/checkpoint.rs:31-299`、`crates/envoy-agent/src/session.rs:367-426`、`crates/envoy-agent/src/daemon.rs:25-96, 240-341`、`crates/envoy-service/src/conduit/{accept.rs:11-35, ws.rs:98-159, mod.rs:346-524}`、`crates/envoy-cli/src/main.rs:356-420`
  - **现状问题**：`Runtime::send`/`deliver_monitor_fire` 放锁后做同步磁盘读再设 `running=true`，并发发送会双开 turn 并孤儿化句柄；并发 `Runtime::rewind` 导致磁盘撕裂及 `checkpoints[idx]` 越界 panic poison 锁；`interrupt()` 不取消底层运行中 job；`ensure_with_hooks` 在 `SessionStart` 完成前暴露 session 且其 30s 上限高于客户端 `attach()` 5s 超时；`CheckpointStore` 记录临时 worktree `FileChanged`、`save()` 用未清洗 `session_id` 临文件且全量 JSON 同步堵锁、只通过写入跟进且 `std::fs::write` 随 symlink 并丢权限、`delete_session` 漏删 checkpoint；`session.id` 未防路径穿越且仅靠客户端落盘；`Runtime::sessions` 无淘汰、`Live::subs` 无界、`MonitorEntry::drop` 漏取消 owned job；`spawn_accept_pool` `abort()` 泄漏内层 worker；`WsByteStream` 丢弃 `poll_write` 错误、`start_send` 未配 `poll_ready` 且重发 `Pong`/`Close`；`decode_chunked` 可整数溢出、`watch` 接收端关闭泄漏任务；daemon 先绑 `envoy.sock` 后开 `agent.sock`、`spawn` 漏收僵尸子进程、`systemd_unit` 路径未加引号。
  - **改造动作**：在 `live.lock()` 内通过原子状态机（如 `SessionBusy::Idle / RunningTurn / Compacting / Rewinding`）占位跃迁后再做异步/磁盘操作，彻底消灭双开 turn 与并发 rewind panic；`interrupt()` 登记并取消当前活跃 job 句柄；`ensure_with_hooks` 延迟暴露或挂起并发 turn 至初始化完成并对齐连接/Hook 超时；`CheckpointStore` 过滤 worktree 临时事件、使用清洗后的唯一临时文件原子保存、安全还原文件避免追随 symlink 并保留文件权限、`delete_session` 连带删除 checkpoint；清洗 `session.id` 并把 turn 完成持久化收归 daemon；为 `Runtime::sessions` 设空闲驱逐策略、`Live::subs` 改有界队列/背压、`MonitorEntry::drop` 取消所控 job；`spawn_accept_pool` 保存所有子任务句柄在 `Drop`/中止时统一清理；按 `futures::Sink` 协议修正 `WsByteStream` 状态机；`decode_chunked` 改 `checked_add` 并加 I/O 超时与 `tx.closed()` 退出；修正 daemon 就绪顺序、子进程回收、优雅停止先关 `agent.sock` 再收尾会话、给 `systemd_unit` 路径加引号。
  - **回归验证**：并发压测验证同时向同 session 连发两次 `Send` 或并行两次 `Rewind` 无 panic 与孤儿任务；`CloseSession` 后取消 owned monitor job；`spawn_accept_pool` drop/abort 后任务计数归零；`WsByteStream` 模拟底层错误不再返回 `Ok(len)`；`daemon::ensure` 等待 `agent.sock` 就绪后才返回。

- [x] **P3-4** `[P1/并发]` Worktree 并行子 Agent 与单树修复
  - **位置**：`crates/envoy-agent/src/agent/subagent/runner/{isolate.rs:13-83, run.rs:122-219, mod.rs:183-198}`、`crates/envoy-agent/src/agent/subagent/catalog.rs:106-115`、`crates/envoy-agent/src/worktree/{task.rs:49-195, mod.rs:114-303}`
  - **现状问题**：`run_tasks` 不校验 `report.ok`，把超时/报错的子 Agent 半成品照样 `git add -A && git commit && git merge` 合进主分支；未检查脏工作区，当用户位于 linked worktree 时会从主仓而非当前分支 `HEAD` 拉分支；合并后从不删除临时 worktree、临时分支及 `trusted.json` 条目；合并冲突遗留未清理 `MERGE_HEAD`；父 turn 中断无 `Drop` 兜底清理；子 Agent `is_readonly()` 未识别 `job`/`todo`/`monitor`/可变 MCP，消耗 token 未计入父 turn 总计，超时强杀跳过 MCP 优雅关闭，并行 `hook_state` 合并采用后写覆盖；`worktree/mod.rs` 在子目录执行 `/worktree remove|merge` 失败且把 `.git` 写死为目录（破坏 submodule/gitfile）。
  - **改造动作**：仅对 `report.ok == true` 的任务执行提交与合并，失败任务默认丢弃临时提交/保留现场提示并清理树；前置校验工作区干净，基于当前活跃 `HEAD` 建分支，合并成功后清理 worktree 目录、临时分支与 `trusted.json` 记录，遇冲突自动 `git merge --abort` 并回滚，为批量任务加 `Drop`/取消清理守卫；收紧 `is_readonly()` 判定；将子 Agent token 用量汇总进父 turn 账本；合并并行 `hook_state` 提示并发覆盖策略；`worktree/mod.rs` 统一走 `git rev-parse --show-toplevel` / `--git-dir` 支持子目录与 gitfile。
  - **回归验证**：`worktree/tests.rs` 与 `subagent` 测试覆盖：失败/超时 subagent 不污染主分支且临时分支与目录被删除、脏工作区前置报错、冲突后 `MERGE_HEAD` 自动清理、在子目录运行 `/worktree` 命令成功。

- [x] **P3-5** `[P1/CLI-运维]` CLI、Headless、安装/发布脚本与 Benchmark 硬伤修复
  - **位置**：`crates/envoy-cli/src/{cli_args.rs:167-170, main.rs:131-173, 234-246, 357-382, headless/mod.rs:248-329}`、`scripts/{install.sh:127, 169, 183, package.sh:49}`、`.github/workflows/release.yml:47-63`、`benchmarks/terminal-bench/envoy_agent.py:133-154`
  - **现状问题**：`envoy run --shell echo hello` 用 `argv.remove(0)` 丢弃后续词；全局 `HeadlessArgs`（`--model`/`--permission-mode`/`--sandbox` 等）在 TUI 模式静默被吞；`--conduit-socket`/`--socket` 未注入 `Runtime`；TUI 模式下 `tracing` 输出直接打到 stdout 破坏全屏界面；`headless` `TurnError` 把 token/tool 计数归零且提前 `?` 跳过 shutdown；`install.sh:169` 双引号未转义反引号 `` `envoy` `` 会实际执行命令替换，127 行 `cp` 覆盖运行中二进制报 `ETXTBSY`，183 行卸载提示在 `--dir /usr/local/bin` 下危险；`package.sh` 每次清空 `$DIST`，`release.yml` 跳过 `cargo test` 且漏 `LICENSE`、使用较新 Ubuntu glibc；`envoy_agent.py` 用 CLI 参数传超长/含 `-` 指令且按行解析 pretty JSON 失败。
  - **改造动作**：`TaskArgs::into_request` 在 `self.shell` 下 `argv.join(" ")` 拼接整句命令；将全局选项透传到 TUI 启动初始化（或明确界定适用范围并报错拒绝不支持的组合）；`main.rs` 向 `Runtime` 传入 `conduit_socket`；TUI 模式将 `tracing` 重定向至日志文件（如 `<data_dir>/tui.log`）或禁用 stdout 输出；`headless` 保留出错前的累计用量与会话持久化并用 scopeguard/显式收尾保证 `shutdown`；`install.sh` 转义反引号（改单引号或 `\'`）、先同目录临时文件后 `mv` 原子替换防 `ETXTBSY`、卸载仅删 `$DIR/envoy` 和 `$HOME/.envoy`；`package.sh` 仅清理当前 target 产物、`release.yml` 跑测试并打入 `LICENSE`（评估 Linux musl 静态或兼容基线）；`envoy_agent.py` 改用 `-p -` stdin 传递与完整 JSON 解析。
  - **回归验证**：`cli_tests.rs` 验证 `envoy run --shell echo hello world` 保留完整命令、`headless` 失败保留非零用量；`shellcheck scripts/*.sh` 零警告；验证 `envoy_agent.py` 单元测试解析 pretty JSON 报告。

---

### Phase 4: 线协议演进、版本握手与状态同步

> 目标：建立向后可诊断的线协议版本机制，打破 8 MiB 重连锁死上限，让 TUI 与 daemon 完全同步。
> 目标 commit scope：`feat(proto,service,agent,cli): version agent.sock handshake, stream snapshots, and sync session state`

- [x] **P4-1** `[P1/协议]` `envoy-proto` 与 `agent.sock` 版本前置握手及解码加固
  - **位置**：`crates/envoy-proto/src/{lib.rs:347-360, codec.rs:15-40}`、`crates/envoy-agent/src/runtime/{protocol,listen,client}.rs`、`crates/envoy-agent/src/daemon.rs:25-29`
  - **现状问题**：`envoy-proto` 先整包反序列化再检查 `call.version != AGENT_VERSION`，版本不匹配直接解包失败无法回错误；`read_framed` 预分配最高 8 MiB `vec!` 且 `from_bytes` 忽略尾部多余字节；`agent.sock` 全无版本握手与 daemon `incarnation_id`，升级二进制遇旧 daemon 静默错位或无限 750ms 重连旋涡；`protocol.rs` 与 `events.rs` round-trip 测试覆盖不足半数变体。
  - **改造动作**：`envoy-proto` 与 `agent.sock` 首帧均实现独立版本的固定头/Hello 握手（版本号前置独立字段，解码具体枚举前先校验版本并返回结构化版本冲突提示）；`codec::read_framed` 使用 `take_from_bytes` 校验零残留并做增量/流式读取限流；引入 daemon `incarnation_id` 与二进制版本标识，`daemon::ensure` 检测版本不一致时提示或安全平滑重启，消除 `SeqEvent` 跨重启拼接错位；补齐所有 `AgentReq` / `AgentResp` / `AgentEvent` 变体与边界字段（`#[serde(default)]` 已移除）的 postcard round-trip 测试。
  - **回归验证**：单测验证版本错配请求能收到可读的 `VersionMismatch` 报错、跨重启不同 `incarnation_id` 必触发 `SessionReset`、全量 wire 变体 round-trip 测试通过。

- [x] **P4-2** `[P1/协议]` 流式分片 Snapshot 与有界事件回放
  - **位置**：`crates/envoy-agent/src/runtime/{server.rs:170-179, listen.rs:130-163, protocol.rs}`、`crates/envoy-agent/src/agent/{loop.rs:80-85, tools/fs.rs:63, 180-188}`
  - **现状问题**：`Live::events` 按条数（20,000）不按字节限制，而每条 `TurnDone` 内嵌全量历史 `Vec<ChatMessage>`、每条 `FileChanged` 内嵌 512 KiB 文本，导致 `AgentResp::Snapshot` 单帧突破 8 MiB 永久锁死重连；事件环裁剪后无缺口标记；`MonitorsSync` 在解锁后复用 `seq_now` 破坏单调性。
  - **改造动作**：`Snapshot` 改为流式 `SnapshotStart { running, seq } -> SnapshotChunk(...) -> SnapshotEnd`（或基于独立轻量快照+尾部增量事件），彻底取消单帧全量打包；`Live::events` 增加总字节上限（如 4–8 MiB），`TurnDone` 仅携带新增/摘要而不反复拷贝全量历史、`FileChanged` 仅发 diff 摘要或按需从文件/checkpoint 拉取；如果客户端 `from` 早于已被裁剪的最旧序号，显式发送 `SessionReset` 重载快照；将 `MonitorsSync` 纳入严格单调递增序号生成。
  - **回归验证**：集成测试回放 32 MiB 历史事件与高频 `FileChanged` 仍能正常分片重连恢复，无 `frame too large` 错误。

- [x] **P4-3** `[P1/状态同步]` 打通 `/cd`、`/clear`、`/machine` 同步与严格错误传播
  - **位置**：`crates/envoy-agent/src/runtime/{protocol,server,listen,client}.rs`、`crates/envoy-cli/src/tui/{mod.rs:218-229, state/commands.rs:351-376, 450-460, state/modals.rs:477-486}`
  - **现状问题**：`/cd`、`/clear`、`/machine` 仅改 TUI 内存未同步 daemon，且 `Ensure` 对既有 session 不更新；`Send`/`Interrupt`/`Answer`/`SetPermissionMode` 找不到 session 仍返回 `Ack`；`runtime::tell` 把 `AgentResp::Error` 吞成 `Ok(())`。
  - **改造动作**：在 `AgentReq` 中新增 `SetCwd { id, cwd }`、`SetTarget { id, target }`、`Clear { id }`（或由 daemon 发布对应确认事件）；对不存在 session 的控制请求统一返回明确 `AgentResp::Error` 而非假 `Ack`；`runtime::tell` 正确返回服务端错误并在 TUI 展示提示。
  - **回归验证**：TUI+Daemon 集成测试验证执行 `/cd`、`/machine`、`/clear` 后下一轮对话在 daemon 侧确实使用新目录、新机器与清空后的上下文。

---

### Phase 5: 现代 LLM Harness 对齐（Prompt Caching、压缩、Reasoning）

> 目标：把每百步长会话的输入缓存命中率拉回现代标准（>80–90%），修复压缩失真与 CJK Token 误算，完善多厂商推理块保存与透传。
> 目标 commit scope：`feat(agent): align prompt caching, lossless compaction, and reasoning continuity`

- [x] **P5-1** `[P1/性能与协议]` Anthropic 合规化与全链路 KV / Prompt Caching 保鲜
  - **位置**：`crates/envoy-agent/src/agent/providers/{anthropic.rs:43-95, 147-151, openai_chat.rs:142-145, openai_codex.rs:171-175}`、`crates/envoy-agent/src/agent/{prompt.rs:131-147, loop.rs:168-174, 217-220}`、`crates/envoy-agent/src/agent/context/{mod.rs:185-206, mask.rs:17-41}`
  - **现状问题**：Anthropic `stream_anthropic` 未合并连续 `user`（导致工具后 steering/hook/压缩/步数上限 100% HTTP 400）且发送空 text block；完全没加 Anthropic `cache_control: {"type": "ephemeral"}` 也没读 `cache_creation_input_tokens`/`cache_read_input_tokens`；OpenAI 未传 `prompt_cache_key` 也未读 `cached_tokens`；`sync_environment` 每步改写 `messages[0]`、`PreModelCall` 改写 `effective_system`、超过 60% 预算后 `mask_old_observations` 每步原地篡改滑动窗口外历史工具输出——三者共同造成每一轮都在击穿整条前缀缓存；此外 `environment_note_for_depth` 每步重复扫盘。
  - **改造动作**：在 `anthropic.rs` 合并相邻 `user` 消息（含 `tool_result` + 后续 `user` 文本块）并剔除空文本块；为 Anthropic `system`、末尾 `tools` 与稳定前缀末尾消息添加 `cache_control: {"type": "ephemeral"}` 并完整统计/展示缓存读写 token；为 OpenAI 传递稳定 `prompt_cache_key`（如 session ID 哈希）并采集 `cached_tokens`；保持 `system` 与 `messages[0..k]` 严格 append-only——动态 `<environment>`（todos/状态变更）与 `<hook_context>` 只在需要时作为末尾附注注入或仅变更时追加系统增量，在工具边界阶段仅做不可变/按 Stage 2 整体切换的阈值压缩而停止 60%–100% 区间每步原地擦除历史块；在 turn 生命周期内缓存 skills/memory/subagents 磁盘快照消除每步 N+1 I/O。
  - **回归验证**：单测检验 Anthropic 序列化体永不出现连续 `role: "user"` 或空 `text` 块、包含预期 `cache_control` 断点；多步模拟 turn 测试检验在第 2–5 步之间 `system` 与历史前缀字节完全一致（不因 todo 变更或 65% 占用而变）。

- [x] **P5-2** `[P1/核心质量]` 上下文压缩保真与 Token 估算重构
  - **位置**：`crates/envoy-agent/src/agent/context/{summary.rs:83-457, tokens.rs:25-105, budget.rs:81-350, mod.rs:189, 209}`、`crates/envoy-agent/src/agent/{loop.rs:121, 206, 238, loop_provider.rs:36, 115-135}`
  - **现状问题**：`summarize_messages` 先经过 `dossier_from` 把用户消息砍成首行 160 字符、助手砍成末行 160 字符、工具只留前 12 行（丢尾部报错），还发送两条连续 `User` 并在禁止调工具的请求中塞入全量 15 个工具；降级 `merge_summary` 无长度上限越滚越大；CJK token 估算 `(non_ascii_chars * 2).div_ceil(3)` 方向相反（低估 2–3 倍且校准上限 1.65 卡死）；内置工具估算停在旧版 1,400 token（实为 15 工具 ~2,800）；`TokenCalibration` 用压缩前估算对照压缩后实测值导致因数塌陷到 0.65，且未跨 turn 持久化；overflow 后未重算 `max_tokens`；`force=true` 仅靠 masking 释放空间但 `head` 为空时 `prepare_context` 返回 `None` 导致提前中止。
  - **改造动作**：移除压缩前破坏性的 `dossier_from` 截断，按真实切片（必要时做结构化/保留首尾的关键分块）并在可复用前缀情况下发起总结，传空工具列表 `ToolFilter::none()` 与单条合成指令，为 `merge_summary` 设硬上限；修正 `estimate_text_tokens` 非 ASCII 系数至约 `1.7–2.0` tokens/char、按实际激活工具列表动态计算 schema 开销、计入实际会回传的 reasoning 内容；`TokenCalibration` 仅对同批未压缩请求采样并跨 turn 持久化到 session，将压缩消耗计入账本，overflow 后重算 `max_tokens`，在 `force=true` 下只要任一策略释放了空间即返回允许重试。
  - **回归验证**：单测验证多行用户需求在 `summarize_messages` 传入体中完整保留、总结请求携带 `tools: []`、纯中文 1000 字文本估算落入 1,500–2,000 token 合理区间、overflow 重试不污染 `TokenCalibration::factor`。

- [x] **P5-3** `[P1/核心能力]` Reasoning / Thinking 现代协议对齐与默认策略修复
  - **位置**：`crates/envoy-agent/src/agent/providers/{anthropic.rs:55-74, 152-186, openai_codex.rs:40-85, openai_chat.rs:76-81}`、`crates/envoy-agent/src/agent/effort.rs:18, 88-159`、`crates/envoy-agent/src/session.rs:32-46`
  - **现状问题**：Anthropic 带 `interleaved-thinking-2025-05-14` 头但 `ChatMessage` 既未存储 `signature`/`redacted_thinking`，工具续传也不回传 `thinking` 块，`effort.rs` 未设 `thinking` 参数且禁止 `off`；`openai_codex.rs` 用 `store: false` 却未请求/回传 `reasoning.encrypted_content`；`openai_chat.rs` 仅针对 `provider.id == "deepseek"` 回传 `reasoning_content`（中转站 DeepSeek 及 Qwen/Kimi/GLM 全失效），且跨用户新轮次不清理历史 `reasoning_content`；`DEFAULT_LEVEL = "low"` 默认压低所有推理模型能力，`apply_chat` 同时传 `reasoning_effort` 与 `thinking` 易致 HTTP 400。
  - **改造动作**：在 `ChatMessage` 中以结构化方式保留供应商推理项（`thinking` + `signature` / `redacted_thinking` / `encrypted_reasoning`），并保证 Anthropic 工具调用轮次原样回传（同时严守不破坏线协议与历史兼容，或在 Phase 4 同步升级协议版本）；`openai_codex` 请求 `include: ["reasoning.encrypted_content"]` 并跨步回传 `reasoning` item；`openai_chat` 依据模型/能力配置而非硬编码 `provider.id == "deepseek"` 决定是否按轮回传 `reasoning_content`（并在新用户输入开始时剥离上一轮历史推理字段）；将默认 effort 调整为与模型家族匹配的合理默认值（如 `medium` / `auto` 不强制压成 `low`），修 `effort.rs` 为各协议仅输出唯一合规字段组合。
  - **回归验证**：单测验证 Anthropic tool-use 续传请求精确回传原 `thinking`+`signature` 与 `redacted_thinking`、OpenAI Responses 请求包含 `include: ["reasoning.encrypted_content"]`、经 OpenRouter 调用 DeepSeek R1 在同轮工具调用回传 `reasoning_content` 而跨新 user turn 剥离。

---

### Phase 6: TUI 薄客户端化、零渲染 I/O 与 God `App` 拆解

> 目标：严格落实 ADR 003 与 `AGENTS.md`，使 TUI 达到低 CPU 空载、高响应帧率、无阻塞/崩溃且清晰可维护的薄客户端架构。
> 目标 commit scope：`refactor(cli): remove view I/O, move to event-driven loop, fix TUI crashes, and decompose App`

- [ ] **P6-1** `[P1/性能与架构]` 清零 `tui/view/` 同步 I/O，改为事件驱动渲染与后台任务
  - **位置**：`crates/envoy-cli/src/tui/{mod.rs:280-295, links.rs:27-48, 76, 161-214, diff.rs:330-357, mouse.rs:297-323}`、`crates/envoy-cli/src/tui/view/**`、`crates/envoy-cli/src/tui/state/{jobs.rs:131-144, agent_events.rs}`
  - **现状问题**：TUI 主循环以 25–33ms 无条件空转重绘；`view::draw` 内部每帧同步执行 `git show` 子进程、无大小检查直接 `std::fs::read`（开大文件 OOM，且远程模式读错本机磁盘）、每帧重复多次读盘解析 `auth.json`/`model.json`/`model_caps.json`/`skills`/`mcp`/`todos`/`bwrap` 存在性；每 5s 同步遍历工作区且 `walk_workspace` 无防 symlink 死循环；每帧克隆 `TranscriptItem` 并分配上万屏幕字符 `String`；`poll_shell_cards` 在渲染循环以 40 RPC/s 直接 `.await`；`copy_to_clipboard` 同步 `child.wait()` 可能永远死锁；`agent_events` 每步写 pretty JSON 全量刷盘。
  - **改造动作**：把所有状态快照（provider/budget/sandbox_label/MCP/skills/todos/工作区索引/补全列表/文件预览与 git diff 内容）移出 `view/` 到后台异步任务，仅通过 channel 更新到内存渲染视图模型；打开文件预览前先做远程/本地分流并用 `File::take(FILE_PREVIEW_LIMIT)` 限制读取字节；`walk_workspace` 加深度与 inode 去重防环并移入后台；`snapshot_screen_lines` 仅在鼠标拖拽选中期间生成，消除 `render_single_item` 全量 `clone()`；主循环改用 `tokio::select!` 事件驱动重绘（无动画/无输入时不空转，仅动画运行期按帧刷新）；将 `refresh`、`poll_shell_cards`（对齐刷新节拍/推送）和 `copy_to_clipboard`（带超时）全部放后台任务；节流或交由 daemon 异步写会话。
  - **回归验证**：静态审查/测试确认 `tui/view/` 下零 `std::fs`/`std::process::Command`/`auth`/`mcp`/`skills` 调用；打开 100 MB 大文件预览不 OOM；idle 状态下 CPU 接近 0%。

- [ ] **P6-2** `[P1/稳定与交互]` TUI 终端生命周期、崩溃与交互 Bug 清零
  - **位置**：`crates/envoy-cli/src/tui/{terminal.rs:42-88, edit_file.rs:13-52, text.rs:127-133, mod.rs:389-405, 490, mouse.rs:219-224}`、`crates/envoy-cli/src/tui/state/{input.rs:97-222, edit.rs:294-298, commands.rs:27-36, agent.rs:60-163, modals.rs:379-490, ops.rs:190-201, jobs.rs:228-259, login/methods.rs:48-108}`、`crates/envoy-cli/src/tui/view/{transcript.rs:233, inspector/mod.rs:129-133, mod.rs:204-220, status.rs:81-209, modals.rs:355-587}`
  - **现状问题**：`Tui::enter` 中途失败或 panic/`SIGTERM`/`SIGHUP` 会泄漏 raw mode 与 CSI-u 标志；`edit_file` 挂起未 pop/re-push 键盘增强标志；滚动偏移用 `u16` 在 65,535 行后回绕；`push_linked_rows` 与 `apply_selection_highlight` 在极端宽高下越界 panic；`Instant::now() - Duration` 在启动 <1s 容器里 panic；`Ctrl+U` 清空输入被全局向上滚动遮蔽成死代码；`NewSession` 未清 `transcript_cache`/`expanded_explore_groups`/`inspector` 致跨会话显示串台且缓存无限涨；`UiNotice` 无 `session_id` 串台、并发 `dispatch_agent` 乱序、可删活跃会话导致复活、OAuth 备用端口 `redirect_uri` 失配、`/rewind` 错把 Shell 算作 Turn、Picker 禁输 `j`/`k`、`char_width` vs `display_width` 不一致导致组合字符光标偏移、鼠标只处理 `AttachJob`、登录弹窗高未 clamp；`cancel_active` 提前写死 `agent_running = false` 抢占 daemon 权威。
  - **改造动作**：在 `terminal.rs` 安装全局 panic hook、清理 guard 与 `SIGTERM`/`SIGHUP` 信号退出清理；`edit_file` 挂起前按需对称 pop/re-push `KeyboardEnhancementFlags` 并提示非零退出码；所有滚动字段与计算改用 `usize`；补全切片边界 `min` 保护、`area.width == 0` 早退、`checked_sub` 替代 `Instant` 直接减法；恢复 `Focus::Input` 下 `Ctrl+U` 语义；抽取单一 `reset_session_view()` 供新建/切换/handoff 共用并为 `transcript_cache` 加 LRU/容量上限；`UiNotice` 绑定 `session_id`、`dispatch_agent` 用串行队列保序、禁止在 `/sessions` 删除当前活跃会话、通过 `LoginEvent` 回传实际回调端口、`/rewind` 正确过滤非代理回合、Picker 空输入时不拦截字母检索、对齐 `char_width` 与 `UnicodeWidth`、分发完整鼠标 `Action`、弹窗/状态栏按屏幕尺寸安全裁剪；`cancel_active` 仅发 `Interrupt` 并等待 daemon 终结事件确认。
  - **回归验证**：`tui/tests*.rs` 新增针对 70,000 行滚动、`NewSession` 缓存隔离、`Focus::Input` `Ctrl+U`、`area.width == 0` 渲染、含 Shell 记录的 `/rewind` 与跨 session `UiNotice` 过滤的测试；按 `docs/MANUAL-TEST.md` 跑通 PTY 冒烟测试。

- [ ] **P6-3** `[P2/结构]` 拆解 59 字段 God `App` 并收口 TUI 薄客户端边界
  - **位置**：`crates/envoy-cli/src/tui/mod.rs:324-491`、`crates/envoy-cli/src/tui/types.rs:46-179`、`crates/envoy-cli/src/tui/commands.rs`、`crates/envoy-cli/src/tui/state/{commands.rs:208-474, command_ops.rs:96-98, modals.rs:31-38, login/**}`、`crates/envoy-cli/Cargo.toml:24`
  - **现状问题**：`App` 堆了 59 个扁平字段与 18 个 `impl App` 文件；`Welcome` 用 5 个布尔表示互斥页面；`SLASH_COMMANDS` 与 `run_slash` 漂移（`/jobs`、`/logs`、`/output` 隐身）；TUI 直接依赖 `reqwest` 拉 `/models`/`models.dev`、跑 OAuth 与 `/mcp probe` 子进程。
  - **改造动作**：按照 `AGENTS.md` 把 `App` 拆分为 `AgentState`、`SessionState`、`JobState`、`LoginState`、`UiState` 组合结构；`Welcome` 5 布尔重构为 `WelcomeMode` enum；合并命令表与分发入口，让斜杠命令定义与实现单一来源；将 `/mcp probe`、模型目录拉取与凭据操作迁入 daemon RPC（或 CLI 独立子命令调用路径），从 `envoy-cli` 移除直接 `reqwest` 依赖。
  - **回归验证**：`cargo check --workspace --all-targets` 与全量 TUI 测试通过，确认 `envoy-cli/Cargo.toml` 不再需要 `reqwest`。

---

### Phase 7: 架构瘦身（消除自环 IPC、合并双写工具、单模型表、替换手写轮子）

> 目标：砍掉高维护成本胶水代码、消灭分叉实现带来的漏洞隐患，显著缩小代码体积并提升编译与迭代效率。
> 目标 commit scope：`refactor(workspace): eliminate self-loopback IPC, unify tools/models, and drop redundant custom engines`

- [ ] **P7-1** `[P2/瘦身]` 消除同进程双 Daemon 自环 IPC 与 `EphemeralServer`
  - **位置**：`crates/envoy-service/src/remote.rs`、`crates/envoy-agent/src/runtime/{server.rs, ops.rs, monitors.rs}`、`crates/envoy-agent/src/agent/loop.rs:111-127`、`crates/envoy-cli/src/{main.rs:347-421, headless/mod.rs:84-162}`
  - **现状问题**：`envoy serve` 进程内 `Runtime` 不持有 `AgentServer` / `Jobs`，导致本地每次工具调用、200ms monitor 轮询和 rewind 都要走 `envoy.sock` 自连自己并重读 `agent.json` 写无用审计；`headless` 还为此在 `/tmp` 起 `EphemeralServer` 自连。
  - **改造动作**：为 `Remote` / `AgentDialer` 增加进程内 `LocalDirect` 实现（当 target 为本地且同进程持有 `Arc<Jobs>` / `AgentServer` 时直接内存派发，远程仍走 Conduit Service 16）；彻底移除 `headless` 中的 `EphemeralServer` 及临时 socket；同时修正全仓 4 处重复 `dialer_for` 样板代码。
  - **回归验证**：本机工具调用与 monitor 不再在 `agent/audit.log` 制造无效自环记录；`headless` 全量与压力测试在无临时 socket 环境下通过。

- [ ] **P7-2** `[P2/瘦身]` 统一 `agent/tools/` 与 `mcp/server_tools.rs` 重复工具内核
  - **位置**：`crates/envoy-agent/src/agent/tools/` 与 `crates/envoy-agent/src/mcp/server_tools.rs`
  - **现状问题**：8 个核心工具（`exec`、`read`、`write`、`edit`、`ls`、`glob`、`grep`、`job`）两套各自实现，直接导致路径基准、输出截断、环境过滤与审计严重漂移。
  - **改造动作**：在 `envoy-agent` 抽出一套核心工具执行器与统一参数规范化/截断层，共用同一 `authorize_tool_call` + `HookEngine` 入口；`agent/tools` 与 `mcp/server_tools` 仅保留各自入参解析与返回格式薄适配。
  - **回归验证**：原有 agent tools 与 MCP tool 测试全部通过，新增测试确认双方共享一致的路径校验、截断上限与审计记录。

- [ ] **P7-3** `[P2/瘦身]` 收口 6 份矛盾的模型/供应商注册表并加入内存缓存
  - **位置**：`crates/envoy-agent/src/auth/{catalog.rs, routing.rs, mod.rs:150-156, 443}`、`crates/envoy-agent/src/agent/context/{catalog.rs, families.rs, budget.rs, reasoning.rs, fetch.rs}`
  - **现状问题**：6 份硬编码元数据互打架（`is_retired_model` 封死 `"deepseek-chat"` 和 `"deepseek-reasoner"`、`families.rs` `id.contains("o3")` 误伤 `pro3`/`video1`、`budget.rs` 忽略 `pricing_cliff`、Codex `/models` 返回格式不被 `remember_caps_from_models` 识别、`store_listed_models` 覆盖 `custom_models`、`k` 按 `1024` vs `1000` 混用、TUI 与 daemon 调不同的 provider 解析函数），且 `effort::apply` 与 `budget.rs` 在**每次 LLM 请求**里同步重读反序列化几 MB 的 `model_caps.json`。
  - **改造动作**：建立单一 `ModelRegistry`（合并厂商、协议、alias 映射、上下文窗、pricing cliff、推理能力开关与排序分数），用 `OnceLock`/`RwLock` + 文件 mtime/版本号做进程内缓存消除热路径磁盘读；解禁 `"deepseek-chat"`/`"deepseek-reasoner"`；改用整词/前缀匹配替代宽松子串；修复 Codex `{"models":[{"slug":...}]}` 解析、保护用户 `custom_models`、统一 SI 单位计算；收口 TUI 与 daemon 使用同一 `resolve_provider`（正确保留环境变量覆盖）。
  - **回归验证**：单测验证 `"deepseek-chat"` / `"deepseek-reasoner"` 可选、`"my-pro3-model"` 不误匹配 `o3`、`pricing_cliff` 触发预设阈值、Codex 模型列表写入 caps 缓存且保留 `custom_models`。

- [ ] **P7-4** `[P2/瘦身]` 替换手写轮子、剔除低效重依赖与重复胶水
  - **位置**：`crates/envoy-core/src/search.rs`、`crates/envoy-agent/Cargo.toml`、`crates/envoy-agent/src/agent/context/extract.rs`、`crates/envoy-agent/src/{mcp/mod.rs:11, auth/cli.rs}`、`crates/envoy-cli/Cargo.toml:21`、`crates/envoy-agent/src/{skills/loader.rs, agent/subagent/catalog.rs, memory.rs, agent/effort.rs, agent/delegate_pref.rs, skills/prefs.rs}`
  - **现状问题**：`search.rs` ~400 行手写 `RegexLite` + `expand_braces` + `walk_files` 带来花括号爆炸 OOM、括号分组失效、ReDoS 与无界遍历；`envoy-agent` 仅为了 `extract.rs` 折叠 `.rs` 文件而引入 `tree-sitter` + `tree-sitter-rust` C 编译依赖；`envoy-agent` 里混入了仅 CLI 用的 `clap` 和 267 行交互式 stdin `auth/cli.rs`；`envoy-cli` 声明了未使用的 `futures`；3 份手写 frontmatter 解析器（不支持行内 `#` 注释）、7 份重复 `now_ms()`、4 份重复非原子文件写入（含无保护 `std::os::unix` 导入）、重复 `ScratchGuard` 以及 CLI/TUI 重复的登录/会话加载/日志 demux 代码。
  - **改造动作**：在 `envoy-core::search` 内部用 `regex` + `globset` + `ignore`（流式带上限遍历 + `.gitignore` 支持）替换手写引擎；移除 `tree-sitter` + `tree-sitter-rust`（改为统一稳健的缩进/签名折叠，或视决策引入 `tree-sitter-bash`）；把 `auth/cli.rs` 与 `mcp/mod.rs` 的 CLI 解析移回 `envoy-cli`，移除 `envoy-agent` 的 `clap` 与 `envoy-cli` 的死 `futures`；抽出统一的 frontmatter 解析器、时间戳 helper、安全原子写入与 `ScratchGuard`、共享登录/会话加载/日志流辅助函数。
  - **回归验证**：`cargo test --workspace` 全通过；`search.rs` 压测 `{a,b}`×30 不 OOM、`(foo|bar)baz` 精确匹配、`walk_files` 遇百万文件树提前截断无内存暴涨；非 Unix 条件检查不因无保护 unix import 报错。

---

### Phase 8: 非官方 OAuth 隔离、Skills/Memory 完善、运维轮转与 CI 卫生

> 目标：清理边缘灰产风险与半成品特性，建立长期运维上限与可信 CI 护栏。
> 目标 commit scope：`chore(auth,skills,memory,ci): isolate experimental OAuth, add log rotation, prune dead code, and tighten CI`

- [ ] **P8-1** `[P2/认证隔离]` 非官方一厂 OAuth 标记隔离与并发/安全修复
  - **位置**：`crates/envoy-agent/src/auth/{oauth_anthropic.rs:50-67, oauth_openai.rs:45-107, 210-248, github.rs, refresh.rs:11-120, mod.rs:33-45, 177-189}`
  - **现状问题**：借用 Claude Code / Codex CLI / VS Code Copilot 一厂 Client ID 存在随时封禁风险；Anthropic OAuth 未验证 `state == expected_state`（CSRF）；OpenAI 回调 HTML 直接拼装错误参数（反射 XSS）且读头无超时、device auth 遇到 403/404 永久死等；`refresh.rs` 缺并发互斥导致并行 subagent 互相刷废 rotating refresh token，且会把 `ENVOY_BASE_URL` 临时覆盖永久写进 `auth.json`；`ModelPrefs` 夹带 TUI `theme`/`show_sidebar` 状态。
  - **改造动作**：若保留该功能，显式标记为 `experimental / unofficial` 并收拢提示与文档；修复 `exchange_anthropic_code` 的 `state` 校验；对 OpenAI 本地回调 HTML 转义、加读取超时、给 device 轮询加总超时与取消通道；用进程/异步互斥锁序列化同一 provider 的 token 刷新，且禁止 `routed` / env override 回写持久化 `auth.json`；将 `theme`/`show_sidebar` 移出 `envoy-agent` 放到 TUI 配置。
  - **回归验证**：单测验证错误 `state` 被拒绝、回调错误页 HTML 转义 `<script>`、并发两个 refresh 任务只发一次上游刷新请求、`ENVOY_BASE_URL` 刷新后不污染 `auth.json`。

- [ ] **P8-2** `[P2/能力补齐]` Skills 与 Memory 机制收口
  - **位置**：`crates/envoy-agent/src/skills/{loader.rs:24, 101-142, 360-434, prefs.rs}`、`crates/envoy-agent/src/memory.rs`、`crates/envoy-agent/src/agent/prompt.rs:232-238`
  - **现状问题**：`SKILL.md` 的 `allowed_tools` 仅作文本展示不执行；`skill_root_kind` 把项目级 skill 误标为 `"user"`；`walk_rel` 递归无 symlink 防环；`memory.rs` 没有供 Agent 读写的工具、按 4000 **字节**而非字符截断（中文仅 ~1300 字）且跨全工作区混杂注入、`split_note` 无尾换行符解析失败；`prompt.rs` 加载到 `AGENTS.md` 后直接跳过同级 `ENVOY.md`。
  - **改造动作**：将 `allowed_tools` 接入 `ToolFilter`/激活期授权（或从 schema 删除避免虚假安全感）；修复 `skill_root_kind` 与 `walk_rel` symlink inode/深度防环；`memory.rs` 支持按当前 workspace scope 隔离过滤与字符上限+截断标记，修复 `split_note` EOF 解析，并决定是否开放 Agent 记忆工具；`prompt.rs` 同时兼容合并 `AGENTS.md` 与 `ENVOY.md`。
  - **回归验证**：单测覆盖 symlink 环不死循环、项目 skill 标签正确、中文长 memory 按字符边界截断并带提示、同时存在 `AGENTS.md` 与 `ENVOY.md` 均被注入。

- [ ] **P8-3** `[P2/卫生]` 渲染减债、日志轮转、死代码清理与 CI 护栏对齐
  - **位置**：`crates/envoy-cli/src/tui/{markdown.rs, markdown_table.rs, highlight.rs, decrypt.rs, text.rs:37-58, modals.rs:89}`、`crates/envoy-agent/src/runtime/{protocol.rs, server.rs, mod.rs}`、`crates/envoy-service/src/{server.rs, remote.rs, conduit/mod.rs}`、`crates/envoy-agent/src/agent/tools/schema.rs:328-397`、`crates/envoy-core/src/{audit.rs, fs.rs, log.rs}`、`scripts/{check-file-sizes.py, file-size-baseline.txt}`、`.github/workflows/ci.yml`、`docs/architecture.md:86`
  - **现状问题**：手写 Markdown/高亮/解密渲染约 1,800 行持续吃维护成本；`audit.log`、`permission_audit.jsonl`、`envoy.out` 永不轮转；存在整批零调用死协议与死代码（`AgentReq::{Ensure, Attach, List}`、`AgentResp::Sessions`、`SessionInfo`、`Op::Probe`、`AgentServer::with_device`、`schema.rs` 5 个 `#[allow(dead_code)]`、`expanded_prefix`、`ModelsModal::fetching` 等）；`check-file-sizes.py` 未按 `AGENTS.md` 排除测试文件，`file-size-baseline.txt` 空置；CI 缺 `--locked`、`shellcheck`、Python/benchmark 脚本检查与 MSRV 检查；`docs/architecture.md:86` 审计日志路径与实际不符。
  - **改造动作**：评估用 `pulldown-cmark` 替换手写 Markdown 块/行内/表格解析并简化或可选化 `decrypt.rs`；为 `audit.log`、`permission_audit.jsonl`、`envoy.out` 增加大小轮转上限；清理上述已确认死协议变体、死导出与死测试函数；修正 `scripts/check-file-sizes.py` 真正排除 `tests/` 与 `tests*.rs` 并清理无用 baseline；CI 增加 `--locked`、`shellcheck scripts/*.sh`、Python 语法检查及 MSRV 1.85 验证；同步修正 `docs/architecture.md:86` 路径说明。
  - **回归验证**：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace --locked`、`python3 scripts/check-file-sizes.py`、`shellcheck scripts/*.sh` 全部零错误零警告通过。

## Decisions

已按工程性价比与可维护性锁定以下 5 项设计决策（按顺序在后续阶段落实）：

1. **`agent.sock` 线协议**：保持 `postcard`（延续 [ADR 002](../decisions/002-postcard-local-wire-protocol.md)），但在 Phase 4 补齐固定前置版本头、将在线 DTO 与磁盘模型解耦，并将快照改造成流式分片传输。
2. **非官方一厂订阅 OAuth**：默认主推官方 API Key 与兼容 Gateway；一厂借用 Client ID 的 OAuth 在 Phase 8 修完 CSRF/XSS/刷新互斥锁后保留，并显式标为 `experimental / unofficial`。
3. **`tree-sitter` 依赖**：在 Phase 7 从 `envoy-agent` 直接移除 `tree-sitter` 与 `tree-sitter-rust`，降低构建耗时与跨平台打包复杂度。
4. **`envoy-core/src/search.rs`**：按 `AGENTS.md` “behind a narrow trait/adapter” 原则，在 Phase 7 引入 `regex` + `globset` + `ignore` 替换当前手写搜索/正则引擎。
5. **`memory.rs` 定位**：在 Phase 8 先修工作区范围隔离与字符截断，保持轻量级记忆/指令存储定位，不急于引入复杂 Agent 自主写记忆闭环。

## Next

**Phase 1 (`P1-1` ~ `P1-4`)、Phase 2 (`P2-1` ~ `P2-5`)、Phase 3 (`P3-1` ~ `P3-5`)、Phase 4 (`P4-1` ~ `P4-3`) 与 Phase 5 (`P5-1` ~ `P5-3`) 已完成。** Phase 5 本地门禁：`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`（347 passed, 0 failed）、`cargo check --workspace --all-targets`、`python3 scripts/check-file-sizes.py` 均为退出码 0。本机无 `rustfmt` / `shellcheck`，且当前会话无 TTY，未跑 `docs/MANUAL-TEST.md` 的 PTY 清单。多步前缀字节一致性由 `prefix_bytes_stay_identical_from_step_2_through_5_at_65_percent` 覆盖。接手后续阶段的 Agent 请阅读 [handoff-phase3-to-phase8.md](./handoff-phase3-to-phase8.md)，从 **Phase 6（`P6-1` ~ `P6-3`：TUI 薄客户端、零渲染 I/O 与 God `App` 拆解）** 开始。不要回退 Phase 1/2 的安全不变量，也不要回退 Phase 3 的并发/worktree 修复、Phase 4 的线协议版本与快照分片，以及 Phase 5 的前缀冻结、压缩保真与 reasoning 回传。

## Verification

每个阶段合并前均需通过以下验证基线：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace --all-targets
python3 scripts/check-file-sizes.py
shellcheck scripts/*.sh
```

涉及 TUI、Daemon 断线重连、流式打断与排队交互的阶段（Phase 3、4、6），需同步按 `docs/MANUAL-TEST.md` 跑通 PTY 端到端检查清单。
