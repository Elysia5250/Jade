# 竞品 Harness 设计调研

> 调研时间：2026-11-16  
> 范围：Claude Code、OpenAI Codex CLI、OpenCode、Gemini CLI、Cline、Cursor、Aider、Amp  
> 重点：上下文压缩、权限系统、代码规范与工程约束  
> 结论只采用官方文档、官方工程博客和公开源码；产品细节会随版本变化，文末附来源。

---

## 0. 先给结论

1. **上下文不是“越大越好”，而是预算管理。** 口碑好的 harness 都会把上下文拆成稳定前缀（系统提示、工具定义、项目规则）、对话历史、按需检索三层，并尽量让稳定前缀可缓存。
2. **压缩不是截断，而是一次结构化交接。** 主流做法是“摘要旧历史 + 保留最近一段原文 + 重新注入关键指令/文件/计划”，而不是简单丢掉最早的消息。
3. **压缩阈值、保留量、触发时机都应该显式可配。** OpenCode 的 15k 保留、15% buffer，Codex 的 20k 用户消息预算，Gemini 的 0.5 阈值，都是可解释的参数，不是魔法。
4. **压缩要围绕 prompt cache 设计。** 摘要请求复用原请求前缀；摘要作为最后一条消息追加；尽量在任务自然断点压缩，避免中途改系统提示/工具集。
5. **权限不是一句“允许/拒绝”，而是多层防线。** 模式（Manual/Plan/Auto/Bypass）× 规则（allow/ask/deny）× 沙箱（OS 级）× 分类器/钩子 × 工作区信任 × 组织策略。
6. **规则引擎必须明确优先级。** Claude 是 deny > ask > allow；OpenCode 是最后匹配者胜；Gemini 是优先级分层；Cursor 是 deny 优先。最忌讳“看起来更具体就赢”的隐式规则。
7. **Shell 规则不是安全边界。** 复合命令、`sh -c`、wrapper、重定向、符号链接都能绕过字符串匹配；真正可靠的边界是 OS 沙箱/容器/网络代理。
8. **审批 UX 决定权限系统是否可用。** 要能看清“到底要执行什么”，支持 once / session / project / always，拒绝时能带反馈，危险操作默认拒绝。
9. **代码规范必须机械化。** OpenAI 的 harness engineering 明确把架构分层、文件大小、命名、结构化日志做成自定义 linter，并让 lint 错误直接给 agent 修复指令。
10. **模块化不是审美，是吞吐量。** Codex 是上百个 Rust crate；OpenCode 是严格的 Schema → Core/Protocol → Server 分层；Cline 是四层 SDK；Gemini CLI 把 UI 和 core 拆开。单文件几千行会直接拖慢 review、测试、并行开发和 agent 自己改代码。
11. **长任务不只靠压缩。** Anthropic 的实践表明：压缩解决“同一 agent 继续跑”，上下文重置 + 结构化交接解决“换一个干净 agent 继续跑”；两者要配合 feature list、progress 文件、git 提交和端到端验证。
12. **对 envoy 最重要的三件事：** 先把 `tui/state.rs` 这类 god module 拆掉；再把 context compaction 做成独立模块；最后把权限从“提示词里让模型小心”升级为 harness 强制执行的规则 + 沙箱。

---

## 1. 上下文压缩

### 1.1 各家做法对照

| Harness | 触发 | 摘要形态 | 保留最近原文 | 压缩后重新注入 | 溢出/失败恢复 | 缓存/原生能力 |
|---|---|---|---|---|---|---|
| **Claude Code** | 接近上下文上限自动压缩；`/compact [focus]` 手动；可配 auto-compact window；支持 idle compaction | 结构化摘要，保留架构决策、未解决 bug、实现细节；丢弃冗余工具输出/消息 | 最近访问/修改的最多 5 个文件；技能正文有 5k/25k token 上限 | 系统提示、output style、项目根 `CLAUDE.md`、auto memory、计划、git status、匹配的 SessionStart hook、技能描述 | 自动压缩；必要时清理旧图片；`/rewind` 可回到更早的缓存前缀 | 深度利用 prompt cache；摘要请求复用原前缀；明确 compaction 会使对话层缓存失效 |
| **OpenAI Codex** | 自动/手动；pre-turn、post-turn、standalone 三种 phase | 短而明确的 handoff：progress/decisions/context/constraints/next steps/critical data（源码 `SUMMARIZATION_PROMPT`） | 最近用户消息最多 20k token；保留一部分 client items/tool 输出 | base instructions、world state、初始上下文，插在最后一条真实用户消息/摘要之前 | 压缩过程中遇到 context overflow 时，从最旧历史开始删除并重试 | 摘要作为 user-role item 追加；支持 pre/post compact hooks；有 compaction analytics |
| **OpenCode** | 到达 `window - buffer`；buffer 默认 15%，最低 16k；`/compact` API 手动 | 结构化摘要：Objective、Next Move、decisions、completed/active、blockers、relevant files | 默认保留最近 15k token；工具输出会缩短，附件变成描述 | 当前版本的项目指令作为新基线；最近对话保留原文 | provider 报 too long 时压缩并重试一次；第二次再失败就报错 | 摘要请求复用 prompt cache；支持 background compaction；支持 provider native compaction（不透明加密 checkpoint） |
| **Gemini CLI** | `model.compressionThreshold` 默认 0.5；`/compress` 手动 | “用摘要替换整个 chat context”，保留高层摘要 | 文档未给出固定 tail 预算 | 通过 `GEMINI.md` 层级和 memory 重新提供项目上下文 | 主要靠压缩 + 截断；工具输出默认 40k 字符上限 | token caching；experimental context management |
| **Cline** | 接近模型上下文上限自动压缩 | 综合摘要，强调技术细节、代码变更、决策；替换历史后继续 | 文档未给出固定 token 数 | Memory Bank（projectbrief/productContext/activeContext/systemPatterns/techContext/progress） | 部分模型回退到规则式 truncation；checkpoints 可回滚到压缩前 | 摘要复用 prompt cache，成本主要是输出 token |
| **Aider** | 聊天历史超过 `max_chat_history_tokens` | 递归摘要：先保留约一半预算的 tail，摘要 head；深度最多 3 层 | tail 约半预算 | repo map 每轮随请求发送；系统提示/约定文件重新提供 | 递归摘要；弱模型失败后换主模型 | repo map 使用 tree-sitter + PageRank；prompt cache |
| **Amp** | 主要靠用户显式 handoff/新 thread | Handoff：另一个模型从旧 thread 提取目标相关信息，生成新 thread 的首条消息 | 新 thread 自带最近上下文 | AGENTS.md、环境信息、@mention 文件（默认 500 行/2KB 每行截断） | 引用其他 thread 时由第二个模型按需提取 | 强调“短而聚焦的 thread”，不是无限长会话 |
| **Cursor** | 公开文档未给出压缩算法细节 | 以 rules/context、checkpoints、side chats 为主 | — | rules（Project/User/Team/AGENTS.md）按 glob/描述/手动注入 | 不明确 | 规则分层、Auto-review、checkpoint；压缩细节未公开 |

### 1.2 共同算法模式

大多数 harness 的压缩都可以抽象成：

```text
budget = model_window - reserve_for_output - safety_buffer

if estimate(history) <= budget:
    send(history)
    return

# 1. 先做便宜的清理
history = prune_old_tool_outputs(history)
history = drop_old_images_or_attachments(history)

# 2. 保留最近的高保真 tail
tail = take_recent_tokens(history, keep_tokens)

# 3. 对 head 做结构化摘要
head = history - tail
summary = model.summarize(
    system = stable_prefix,
    input = head,
    prompt = COMPACTION_PROMPT,
    focus = user_focus_or_none,
)

# 4. 摘要作为“交接消息”放在 tail 前面，而不是伪装成普通用户输入
new_history = [stable_prefix, summary_as_handoff, tail, pending_input]
```

关键差异不在伪代码，而在四个参数：

- **何时压**：窗口百分比 + buffer，还是固定 token 数。
- **保留多少**：最近 15k、20k，还是半预算。
- **摘要什么**：只摘要对话，还是把工具输出、文件、计划、技能一起处理。
- **压缩后补什么**：系统提示、项目规则、计划、最近文件、活动任务。

### 1.3 应该保留和丢弃什么

**必须保留（高信号）**

- 目标、验收标准、明确约束、用户偏好。
- 已做决策及原因；被否决的方案及原因。
- 改动过的文件、关键路径、命令、退出码、未解决的错误。
- 当前计划/todo、下一步、阻塞点。
- 活动后台任务/job id、正在运行的进程、需要等待的外部状态。
- 权限拒绝记录、用户明确说“不要做”的事情。
- 最近几轮用户原文（防止摘要扭曲意图）。

**优先丢弃（低信号/可重取）**

- 旧的、已消费的工具 stdout/stderr；保留头尾各若干行和退出码即可。
- 重复日志、重复文件内容、已被后续编辑覆盖的旧版本。
- 大图片、PDF、二进制附件；保留路径/描述。
- 已经被证伪的假设、走不通的尝试细节（保留“为什么放弃”）。
- 冗长的 stack trace（保留异常类型、首尾帧、关键路径）。

**压缩后重新注入**

- 系统提示与工具定义（保持稳定前缀）。
- `AGENTS.md` / `CLAUDE.md` / `GEMINI.md` / `.cursor/rules` 等项目规则。
- 当前 plan / feature list / progress 文件。
- git 状态、当前分支、最近提交。
- 最近读写过的文件（Claude 最多 5 个；可配置）。
- 技能正文（有 token 上限，按需加载）。
- 活动 job/后台任务列表。

### 1.4 与 prompt cache 的关系

Claude Code 的文档把这一点讲得最透：

- 请求前缀按“系统提示 → 项目上下文 → 对话”排序，稳定内容在前。
- 摘要请求复用对话的 system prompt、tools、history，因此热缓存下 `/compact` 的成本主要是生成摘要，而不是重新读全部历史。
- 压缩本身会替换对话层，所以下一次请求必须重建这段缓存；这是设计使然，不是 bug。
- 中途切模型、改 effort、增删 MCP 工具、改系统提示都会让缓存失效。因此：
  - 在任务自然断点压缩，不要在长任务中途频繁改配置。
  - 摘要应追加在对话末尾，避免改变已有前缀。
  - 增量摘要优于每次从头摘要。
  - 后台压缩可以提前生成摘要，到阈值时直接换入（OpenCode experimental）。

### 1.5 溢出与失败恢复

- **Codex**：压缩请求本身遇到 context overflow 时，从最旧历史开始删一条再重试；保留最近消息和前缀缓存。
- **OpenCode**：provider 报 too long 时自动压缩并重试一次；第二次失败才报错。
- **Cline**：部分模型不支持摘要时回退到规则式 truncation。
- **Claude Code**：自动压缩；图片按批次移除；`/rewind` 截断回已缓存前缀，而不是重新摘要。
- **通用原则**：压缩不是无限可靠。必须有“第二次失败就明确报错”的兜底，不能悄悄丢用户输入。

### 1.6 长任务：压缩之外还需要“重置 + 交接”

Anthropic 的长任务实践给出了一个重要区分：

- **Compaction**：同一个 agent 在缩短的历史上继续跑，保留连续性；但可能仍有“context anxiety”（接近上限时提前收尾）。
- **Context reset**：清空上下文，启动一个干净 agent，用结构化 handoff 文件交接；适合跨多个 context window 的长任务。

配套工件：

- 初始 feature list（JSON，逐项 `passes: false`，只允许改状态，不允许删测试）。
- progress 文件（每个 session 结束时写清做了什么、下一步、坑）。
- `init.sh`（如何启动、如何跑端到端验证）。
- git 提交（可回滚、可审计）。
- 独立 evaluator/QA agent（避免 generator 自评过于宽容）。
- sprint contract（先约定“done 是什么”，再实现）。

这对 envoy 的启示：上下文压缩模块应该同时支持“摘要继续”和“重置 + handoff”两种策略，而不是只做一种。

### 1.7 如何评测压缩质量

不要只看“压缩后还能不能跑”。建议至少测：

- **任务成功率**：压缩前后同一组长任务的成功率、返工率。
- **关键事实召回**：目标、约束、文件路径、未解决 bug、下一步是否还在。
- **指令保持**：压缩后是否仍遵守项目规则/权限边界。
- **成本**：摘要 token、缓存命中率、压缩次数、总 token。
- **延迟**：压缩阻塞时间；后台压缩是否真的省时。
- **退化模式**：是否丢失最近用户意图、是否把“已否决方案”又拿出来、是否虚构进度。

Anthropic 的建议是：先在复杂 agent trace 上最大化 recall，再迭代提高 precision；不要一上来就追求短摘要。

---

## 2. 权限系统

### 2.1 推荐的分层模型

```text
┌──────────────────────────────────────────────┐
│ 组织/托管策略（最高优先级，不可被项目覆盖）        │
├──────────────────────────────────────────────┤
│ 工作区信任（是否加载项目配置/hooks/MCP/规则）      │
├──────────────────────────────────────────────┤
│ 会话模式（Manual / Plan / Auto / Bypass）       │
├──────────────────────────────────────────────┤
│ 规则引擎（allow / ask / deny，按明确优先级匹配）   │
├──────────────────────────────────────────────┤
│ 运行时审查（classifier / hooks / 人工审批）       │
├──────────────────────────────────────────────┤
│ OS 沙箱 / 容器 / 网络代理（真正的强制边界）        │
└──────────────────────────────────────────────┘
```

关键点：**规则引擎负责“要不要问”，沙箱负责“问了也做不到”。** 两者不能互相替代。

### 2.2 各家权限设计对照

| Harness | 模式 | 规则语法/优先级 | 粒度 | 沙箱/隔离 | 值得借鉴 |
|---|---|---|---|---|---|
| **Claude Code** | Manual(`default`)、`acceptEdits`、`plan`、`auto`、`dontAsk`、`bypassPermissions` | `allow` / `ask` / `deny`；**deny > ask > allow**；裸工具名 deny 会把工具从模型上下文移除；支持 `Bash(git log *)`、`Read(./.env)`、`WebFetch(domain:...)`、参数匹配 | 文件路径、命令前缀/通配、URL 域、MCP、子 agent、参数 | Bash sandbox（macOS Seatbelt / Linux bwrap 等）、受保护路径、容器/VM 建议；auto 模式有分类器 | 规则优先级清晰；deny 移除工具省 context；审批可带 comment；受保护路径；`dontAsk` 用于 CI |
| **OpenAI Codex** | 沙箱模式 `read-only` / `workspace-write` / `danger-full-access` × 审批策略 `on-request` / `never` / `granular` × reviewer `user` / `auto_review` | 权限 profiles；execpolicy 规则；网络 allow/deny；managed requirements | 文件根、网络域/Unix socket、MCP/app 工具、命令规则 | macOS Seatbelt；Linux bwrap + seccomp；Windows MXC；网络默认关闭；`.git`/`.agents`/`.codex`/`.aws` 受保护；网络代理 allowlist-first | **沙箱与审批解耦**；workspace-write 默认；保护 `.git` 等路径；自动审查 fail-closed；`/permissions` 可查看当前边界 |
| **OpenCode** | 由 agent/permissions 决定；无独立“模式”概念，plan agent 通过 deny edit 实现 | `permissions: [{action, resource, effect}]`；**最后匹配者胜**；没有匹配默认 ask；支持 `*`/`?` 通配；`external_directory` 单独检查 | read/edit/glob/grep/shell/subagent/skill/webfetch/websearch/MCP/图片生成 | 文档没有提供内置 OS 沙箱；shell 用内置 scanner（Bash/Zsh/POSIX/PowerShell），解析失败返回 scanner error；并明确警告 shell 拥有宿主全部权限 | 规则模型简单可扩展；agent 级规则追加；once/always/reject；always 保存为项目级 allow；policies 是硬 deny，永不提示 |
| **Gemini CLI** | `default`、`autoEdit`、`plan`、`yolo` | TOML policy：`toolName`、`commandPrefix`、`commandRegex`、`argsPattern`、`mcpName`、`subagent`、`decision`、`priority`；**最高优先级胜**；tier：default/extension/workspace/user/admin | 工具、参数正则、命令前缀/正则、MCP、子 agent、交互/非交互 | macOS Seatbelt、Docker/Podman、gVisor/runsc、LXC；sandbox expansion 请求；`security.folderTrust` | **策略分层 + 优先级**；deny 工具从模型记忆移除；folder trust 会先扫描命令/MCP/hooks/skills/settings 并警告；sandbox expansion 很实用 |
| **Cursor** | Auto-review、Allowlist、Run Everything | `permissions.allow/deny`：`Shell(...)`、`Read(...)`、`Write(...)`、`WebFetch(...)`、`Mcp(...)`；**deny 优先** | 命令、路径 glob、域名、MCP | macOS Seatbelt；Linux Landlock + seccomp；网络默认阻断，`sandbox.json` 配 allowlist；受保护路径；read boundary | Auto-review 分类器 + 沙箱组合；拒绝后 agent 可换方案；团队策略可覆盖；`sandbox.json` 与权限文件分离 |
| **Cline** | Auto Approve 分类开关；YOLO；SDK tool policies | 按工具类别 autoApprove；模型给命令标 `requires_approval`；SDK `toolPolicies` 可 `enabled:false` | 读/写/命令/browser/MCP | 依赖宿主/编辑器；YOLO 明确警告 | 审批粒度贴合用户心智（读项目/读所有/编辑项目/执行安全命令/执行所有）；“safe vs approval-required”由模型标注；SDK 可写条件审批 |
| **Aider** | `--yes`、`/allow`、git 自动提交 | 相对简单，主要靠用户确认和 git | 文件/命令 | 无强沙箱 | 轻量；git 作为回滚边界 |

> 注：Gemini CLI 在 2026-06-18 后主要面向 Gemini Code Assist 企业用户，免费/Google One 用户迁移到 Antigravity CLI；其权限/沙箱设计仍值得参考。

### 2.3 规则引擎的设计要点

1. **优先级必须写死并测试。** 推荐 deny > ask > allow，托管 deny 永远最高；不要让“更具体的规则”隐式获胜。
2. **工具名与资源分开。** `Bash` 是工具，`git push *` 是资源；`Read` 是工具，`./.env` 是资源。不要用一个字符串同时表达两者。
3. **路径先规范化再匹配。** 展开 `~`/`$HOME`，`canonicalize`，处理 symlink/junction；allow 要同时匹配请求路径和真实路径，deny 任一匹配即拒绝。
4. **Shell 要解析复合命令。** `&&`、`||`、`;`、`|`、子 shell、命令替换、循环体都要拆开；每个子命令都满足 allow 才放行；deny/ask 任一匹配即生效。
5. **处理 wrapper 和重定向。** `timeout`、`nice`、`nohup`、`env`、`xargs`、`sh -c`、`> file`、`tee` 都可能绕过简单前缀匹配；解析失败就 ask。
6. **裸工具 deny 可以移除工具定义。** 这既省 context，又减少模型尝试；但要注意不要误删 `ask`/`end` 之类必需工具。
7. **保存的审批要有作用域。** once / session / project / always；项目级规则写入项目本地配置，不覆盖 deny；组织策略不可被项目 allow 翻盘。
8. **Plan 模式是“只读 + 可写计划文件”。** 读、搜索、查看 git 允许；edit/write 默认拒绝；计划文件写到专用目录；批准计划后再切换到执行模式。
9. **工作区信任是加载项目配置的前置条件。** 不可信仓库不加载 `.env`、hooks、MCP、自定义命令、项目设置；首次进入要展示发现项和风险（Gemini 的 discovery dialog 是很好的参考）。
10. **分类器/自动审查不是安全边界。** 它可以减少提示，但可能误判；失败要 fail-closed；高风险动作仍应走沙箱或人工。
11. **审批 UI 要显示“实际执行内容”。** 完整命令、规范化路径、diff、目标机器、为什么触发规则；拒绝时可以带一句反馈给模型。
12. **审计。** 记录时间、session、工具、资源、决策、命中规则、是否自动批准；敏感内容脱敏。

### 2.4 沙箱

- **文件系统**：只允许工作区 + 明确挂载的额外目录；`.git`、`.ssh`、`.aws`、`.env`、`.envoy`、`.codex`、`.agents` 等默认只读/拒绝。
- **网络**：默认关闭；需要时用域名 allowlist；`deny` 优先；防止 DNS rebinding；本地/私网地址默认阻断。
- **进程**：限制子进程、ptrace、Unix socket；避免 agent 通过本地 daemon 绕过。
- **平台**：macOS Seatbelt；Linux bwrap + seccomp 或 Landlock；Windows MXC/Job Object；容器作为更强边界。
- **沙箱扩展**：当命令确实需要额外权限时，弹出“本次命令需要 X 目录/网络”的请求，批准只对本次生效（Gemini 的 sandbox expansion）。
- **无沙箱重试**：沙箱失败后允许“在沙箱外重试”，但必须重新审批并记录（Claude/Cursor 的做法）。

### 2.5 常见坑

- 把 shell 规则当安全边界：`Bash(curl *)` 挡不住 `/usr/bin/curl`、`sh -c 'curl ...'`、变量拼接、重定向。
- 只匹配命令前缀，不解析复合命令：`safe-cmd && rm -rf /`。
- 路径不做 canonicalize：`./project/../../.ssh/id_rsa`、symlink 指向外部。
- 保护路径只保护文件工具，不保护 shell：`echo x > .git/config`。
- 允许规则覆盖拒绝规则：一次“always allow”就把组织策略架空。
- 分类器超时/失败时默认放行：必须 fail-closed。
- 项目配置在未信任仓库里自动加载：hooks/MCP 可以在用户批准前执行代码。
- 审批提示只显示工具名，不显示参数：用户无法判断风险。

---

## 3. 代码规范与架构

### 3.1 OpenAI 的 harness engineering：把规范变成可执行约束

OpenAI 在 2026-02 的 *Harness engineering* 中总结了他们的做法，核心不是“写更多文档”，而是：

- **AGENTS.md 是目录，不是百科全书。** 他们试过“一个大 AGENTS.md”，失败得很可预测：挤占任务上下文、什么都重要等于什么都不重要、很快腐烂、无法机械校验。最终把 AGENTS.md 控制在约 100 行，只做 map，指向 `docs/` 里的深层资料。
- **仓库知识是 system of record。** `docs/` 下有 design-docs、exec-plans、generated、product-specs、references、ARCHITECTURE.md、DESIGN.md、QUALITY_SCORE.md、RELIABILITY.md、SECURITY.md 等；计划是 first-class artifact，活跃/完成/技术债都版本化。
- **渐进披露。** agent 先读小而稳定的入口，再被指引到下一步，而不是一上来灌 1000 页。
- **机械化校验。** 自定义 linter + CI 检查知识库是否最新、交叉链接、结构正确；有“doc-gardening” agent 定期清理过期文档。
- **架构分层。** 每个业务域固定层次：Types → Config → Repo → Service → Runtime → UI；跨切面（auth/connectors/telemetry/feature flags）只通过 Providers 接口进入；依赖方向用 linter/结构测试强制。
- **文件大小、命名、结构化日志、平台可靠性**都用自定义 lint 强制；lint 错误消息直接包含修复指令，方便 agent 自己改。
- **Golden principles + 持续垃圾回收。** 例如共享工具优先、边界必须解析/验证、不猜数据结构；后台 agent 定期扫描偏差、更新质量评分、开小 PR。
- **合并哲学随吞吐量变化。** PR 短命、减少阻塞门禁、flake 用后续运行解决；因为修正成本低、等待成本高。

这套东西对 envoy 的直接启发：**“禁止几千行单文件”不能只靠口头约定，要变成 CI 里的硬检查 + lint 错误里的修复提示。**

### 3.2 各家代码组织

| 项目 | 组织方式 | 可借鉴点 |
|---|---|---|
| **OpenAI Codex** | `codex-rs/` 是 Rust workspace，成员超过 150 个 crate：`core`、`protocol`、`prompts`、`tools`、`sandboxing`、`linux-sandbox`、`windows-sandbox`、`tui`、`exec-server`、`app-server`、`model-provider`、`history`、`context-fragments`、`utils/*` 等 | 按能力和边界拆 crate；`protocol`/`prompts` 独立；平台沙箱独立；工具、provider、历史、TUI 解耦；workspace 统一 lint/依赖版本 |
| **OpenCode** | `packages/schema`（wire/storage）、`core`（domain/persistence）、`protocol`（public API）、`server`、`client`、`cli`、`tui`、`app`、`desktop`、`plugin` 等 | 严格依赖方向；生成 client 不手改；插件/协议/SDK 分层；TUI 与 server 分离；大量小包 |
| **Cline** | SDK 四层：`@cline/core`（orchestration）→ `@cline/agents`（browser-compatible loop）→ `@cline/llms`（provider）→ `@cline/shared`（types/tools/hooks/contracts） | 依赖只能向下；底层可独立嵌入；core 负责 session/storage/tools/hub；agents 不依赖 Node 特定能力 |
| **Gemini CLI** | `@google/gemini-cli`（UI、命令解析、用户功能）与 `@google/gemini-cli-core`（API、认证、缓存）两个包；core 不打包，可独立使用 | UI 与 core 分离；core 可复用；npm workspace |
| **Aider** | `aider/coders/*`、`repomap.py`、`history.py`、`commands.py`、`repo.py` 等按职责拆 | repo map 与聊天摘要独立；但 `base_coder.py` 约 2.5k 行，说明成熟项目也会积累 god file，需要硬约束 |
| **envoy（当前）** | `tui/mod.rs` 848、`tui/state.rs` 2832、`tui/view.rs` 1310、`tui/text.rs` 623、`tui/theme.rs` 76；`llm.rs` 1669、`auth.rs` 1666、`main.rs` 999 | 已经从单文件 `tui.rs` 迈出第一步；但 `state.rs` 仍是一个 2.8k 行的 `impl App`，`view.rs` 仍 1.3k 行，需要继续拆 |

### 3.3 建议写进 envoy AGENTS.md 的硬规则

```text
## Source layout
- One public concept per module; no "god" modules or god objects.
- Source file soft limit: 400 lines. Hard fail: 700 lines (tests/generated excluded).
- Function soft limit: 60 lines. Hard fail: 100 lines.
- Max nesting depth: 3.
- TUI render functions are pure; no I/O, no provider calls, no job control.
- Domain/agent code must not depend on TUI types.
- Provider SDKs live only in agent/providers/*; no provider types in TUI/session.
- Wire types live in a protocol/schema module; no skip_serializing_if on postcard types.
- Prefer small traits at boundaries: Provider, Tool, PermissionEvaluator, CompactionStrategy, EventSink.
- No new dependency without a boundary rationale.

## Verification
- cargo fmt --check
- cargo clippy --workspace --all-targets -- -D warnings
- cargo test --workspace
- protocol round-trip tests for every event
- TUI snapshot/golden tests for layout changes
- PTY smoke test: two turns in a row; steer; queue; recall; interrupt; restart daemon
- file-size check in CI

## Docs
- AGENTS.md is a map, not an encyclopedia. Keep it under 200 lines.
- Deep docs live in docs/: architecture, decisions (ADR), plans, protocol, security.
- Nested AGENTS.md per crate/package when local rules differ.
- Generated files are never hand-edited; codegen command documented.
```

### 3.4 针对 envoy 的具体拆分建议

**TUI**

```text
crates/envoy-cli/src/tui/
  mod.rs                 // run loop, terminal setup, re-exports
  theme.rs               // colors only
  text.rs                // wrapping, display width, markdown helpers
  state/
    mod.rs               // App struct + composition of sub-states
    session.rs           // Session, transcript, selection
    agent.rs             // agent_runtime, outbox, pending ask, event apply
    jobs.rs              // jobs, shell watches, refresh
    login.rs             // login flow, OAuth/device code
    modals.rs            // models/sessions/picker/commands
    input.rs             // editor buffer, history, key handling
    commands.rs          // slash commands, palette, actions
  view/
    mod.rs               // draw orchestration
    transcript.rs
    editor.rs
    status.rs
    sidebar.rs
    queued.rs
    ask.rs
    menus.rs
    modals.rs
```

目标：`state.rs` 的 2.8k 行按“agent / jobs / login / commands / input”拆成 5-7 个 200-500 行模块；`App` 不再是一个巨型 `impl`，而是组合 `AgentState`、`SessionState`、`JobState`、`LoginState`、`UiState`。

**Agent / LLM**

```text
crates/envoy-cli/src/agent/
  mod.rs
  loop.rs                // single loop, turn boundaries, steering
  prompt.rs              // system prompt, AGENTS.md, env
  events.rs              // AgentEvent, serialization tests
  tools/
    mod.rs               // Tool trait + registry
    fs.rs                // read/write/edit/ls
    shell.rs             // exec/job
    say_ask.rs           // say/ask
  providers/
    mod.rs               // Provider trait
    openai_chat.rs
    openai_codex.rs
    anthropic.rs
    sse.rs               // shared SSE parser
  context/
    budget.rs
    tokens.rs
    prune.rs
    summary.rs
    compactor.rs
    prompt.rs            // structured handoff prompt
```

**权限**

```text
crates/envoy-cli/src/permissions/
  mod.rs                 // Decision, Mode, Evaluator trait
  rules.rs               // rule parsing/matching/precedence
  shell.rs               // command parser/scanner
  paths.rs               // canonicalize, protected paths, symlinks
  approval.rs            // once/session/project, feedback
  policy.rs              // managed/project/user precedence
  audit.rs               // append-only JSONL
  sandbox.rs             // bwrap/landlock/seatbelt adapters (later)
```

**运行时 / 协议**

- `runtime.rs`（623 行）可以继续拆成 `runtime/server.rs`、`runtime/session.rs`、`runtime/protocol.rs`、`runtime/attach.rs`。
- `session.rs`（493）保持 store/transcript；把 token/摘要元数据放进 `context`。
- `main.rs`（999）只保留 CLI 解析和命令分发；各命令具体实现进 `commands/`。

---

## 4. 对 envoy 的落地优先级

### P0：先把地基做对（下一两个迭代）

1. **拆 `tui/state.rs` 和 `tui/view.rs`。**  
   这是当前最大的可维护性风险，也直接违反“禁止几千行单文件”的新规则。先做纯移动（不改行为），再加 CI 文件大小检查。
2. **加 `agent/context/` 模块。**  
   实现：预算 + buffer、最近 tail、旧工具输出清理、结构化摘要、压缩后重新注入项目规则/计划/最近文件、overflow 重试一次、`/compact [focus]`、压缩事件与 UI 指示。
3. **加 `permissions/` 模块。**  
   先做 Manual/Plan 两种模式 + allow/ask/deny + deny 优先 + 路径规范化 + `.env`/`.git`/`.ssh` 保护 + shell 复合命令解析；再接审批 UI 和审计日志。
4. **拆 `llm.rs`。**  
   按 provider / SSE / tool loop / prompt / events 拆；provider trait 化，方便 DeepSeek/OpenAI/Anthropic/Codex 统一。
5. **CI 硬约束。**  
   `cargo fmt --check`、`cargo clippy -D warnings`、`cargo test --workspace`、协议 postcard 往返、文件大小脚本、TUI golden/PTY smoke。
6. **AGENTS.md 改成 map。**  
   根目录 < 200 行；深层架构/决策/计划进 `docs/`；每个 crate 可加嵌套 AGENTS.md。

### P1：把体验和安全性补齐

- 工具输出默认截断（头尾 + 退出码 + “如何获取更多”提示），旧输出在压缩前优先清理。
- 子 agent / side chat 独立 context，只回传摘要；避免主上下文被长输出污染。
- 符号索引 / repo map（tree-sitter + PageRank）作为“按需检索”的导航层。
- Plan 模式 + 工作区信任 + 项目配置发现（AGENTS.md、hooks、MCP、settings）。
- OS 沙箱（Linux bwrap/landlock/seccomp；macOS seatbelt）+ 网络 allowlist + 受保护路径。
- 权限审计 JSONL + 结构化日志 + token/compaction/permission 指标。

### P2：更进一步

- 后台压缩（提前生成摘要，到阈值换入）。
- Provider native compaction（不透明 checkpoint）。
- 自动审查分类器（fail-closed），用于减少审批噪音。
- 组织级托管策略 / 远程配置。
- 长任务 handoff：feature list、progress 文件、`init.sh`、独立 evaluator。

---

## 5. 参考来源

### 上下文压缩 / 长任务

- Claude Code, *Explore the context window*: https://code.claude.com/docs/en/context-window
- Claude Code, *How Claude Code uses prompt caching*: https://code.claude.com/docs/en/prompt-caching
- Claude Code, *Manage costs effectively*: https://code.claude.com/docs/en/costs
- OpenAI Codex, compaction source: https://github.com/openai/codex/blob/main/codex-rs/core/src/compact.rs
- OpenAI Codex, compaction prompt: https://github.com/openai/codex/blob/main/codex-rs/prompts/templates/compact/prompt.md
- OpenCode, *Compaction*: https://opencode.ai/docs/compaction/
- Gemini CLI, *CLI commands* (`/compress`): https://www.geminicli.com/docs/reference/commands
- Gemini CLI, *Settings* (`compressionThreshold`, `truncateToolOutputThreshold`): https://www.geminicli.com/docs/cli/settings/
- Cline, *Auto Compact*: https://docs.cline.bot/features/auto-compact
- Cline, *Memory Bank*: https://docs.cline.bot/best-practices/memory-bank
- Aider, *Repository map*: https://aider.chat/docs/repomap.html
- Aider, `history.py`: https://github.com/Aider-AI/aider/blob/main/aider/history.py
- Amp, *Context Management in Amp*: https://ampcode.com/guides/context-management
- Anthropic, *Effective context engineering for AI agents*: https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- Anthropic, *Effective harnesses for long-running agents*: https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents
- Anthropic, *Harness design for long-running application development*: https://www.anthropic.com/engineering/harness-design-long-running-apps
- Anthropic, *Writing effective tools for agents*: https://www.anthropic.com/engineering/writing-tools-for-agents

### 权限 / 沙箱

- Claude Code, *Configure permissions*: https://code.claude.com/docs/en/permissions
- Claude Code, *Choose a permission mode*: https://code.claude.com/docs/en/permission-modes
- Claude Code, *Settings files and precedence*: https://code.claude.com/docs/en/settings
- OpenAI Codex, *Permissions*: https://developers.openai.com/codex/permission-modes
- OpenAI Codex, *Agent approvals & security*: https://developers.openai.com/codex/agent-approvals-security
- OpenCode, *Permissions*: https://opencode.ai/docs/permissions/
- OpenCode, *Policies*: https://opencode.ai/docs/policies/
- Gemini CLI, *Policy engine*: https://www.geminicli.com/docs/reference/policy-engine/
- Gemini CLI, *Trusted folders*: https://www.geminicli.com/docs/cli/trusted-folders
- Gemini CLI, *Sandboxing*: https://www.geminicli.com/docs/cli/sandbox/
- Cursor, *Permissions*: https://cursor.com/docs/cli/reference/permissions
- Cursor, *Run Modes*: https://cursor.com/docs/agent/security/run-modes
- Cline, *Auto Approve & YOLO*: https://docs.cline.bot/features/auto-approve
- Cline, *Permission Handling*: https://docs.cline.bot/sdk/guides/permission-handling

### 代码规范 / 架构

- OpenAI, *Harness engineering: leveraging Codex in an agent-first world*: https://openai.com/index/harness-engineering/
- OpenAI Codex workspace/lints: https://github.com/openai/codex/blob/main/codex-rs/Cargo.toml
- OpenCode, `AGENTS.md`: https://github.com/anomalyco/opencode/blob/dev/AGENTS.md
- OpenCode, `CONTRIBUTING.md`: https://github.com/anomalyco/opencode/blob/dev/CONTRIBUTING.md
- Cline, *Packages*: https://docs.cline.bot/sdk/architecture/overview
- Gemini CLI, *Package overview*: https://www.geminicli.com/docs/npm/
- AGENTS.md: https://agents.md/
- Anthropic, *Building effective agents*: https://www.anthropic.com/engineering/building-effective-agents

---

## 6. 一句话总结

**最好的 harness 不是“提示词写得最花”的那个，而是把上下文预算、权限边界和代码结构都变成可配置、可观测、可机械校验的系统。** envoy 已经有 daemon 单 loop、事件回放、say/ask 这些正确的地基；下一步应该先把 TUI 拆干净，再把 compaction 和 permissions 做成独立模块，而不是继续往 `App` 和 `llm.rs` 里堆逻辑。
