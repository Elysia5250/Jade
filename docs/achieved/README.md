# 归档（achieved）

这里存放**已完成、已冻结**的文档，仅供查阅，**不再维护**。

## 规则

- 归档文件的内容不再更新。发现归档文档与代码不一致时，不要改归档文档，
  而是以现行文档（`docs/architecture.md`、`docs/decisions/`、`docs/MANUAL-TEST.md`）为准。
- 归档文件只做「打错字」级别的修正；任何语义变更都不应发生在这一层。
- 新归档项必须：有明确的完成判据、在下面的索引里有登记、且不再被现行文档当作
  权威来源引用（若仍被引用，先更新引用指向现行文档）。
- 归档文件保留原始文件名，便于旧链接和 `grep` 命中。

## 索引

| 文件 | 归档日期 | 内容 | 归档依据 |
| --- | --- | --- | --- |
| `FULL-REVIEW.md` | 2026-11-21 | 十轮完整的项目审查与修复复核记录（安全边界、正确性、并发、文档漂移、遗留清单） | 审查轮次已收敛，结论已并入代码与任务清单 |
| `hardening-and-modernization.md` | 2026-11-21 | 安全加固与现代 Harness 架构瘦身计划，Phase 1–8 | 40 项全部勾选，`## Next` 明示「Phase 8 是最后一项，到此停止」 |
| `handoff-phase3-to-phase8.md` | 2026-11-21 | Phase 3–8 的接手执行手册（阶段拆解、踩坑清单、量化验收） | 对应阶段已随 `35b24bb` 完成 |
| `HARNESS-RESEARCH.md` | 2026-11-21 | 竞品 Harness 设计调研（Claude Code、Codex、OpenCode、Gemini CLI 等） | 时点性调研报告（2026-11-16），内容随上游版本变化而冻结 |

## 仍属现行、不要归档的文档

| 文件 | 理由 |
| --- | --- |
| `docs/architecture.md` | 现行模块地图与不变量，`AGENTS.md` 指向它 |
| `docs/decisions/` | 现行 ADR，决策仍然生效 |
| `docs/MANUAL-TEST.md` | 现行 PTY 检查清单，`AGENTS.md` 与 `README.md` 都引用 |
| `docs/INSTALL.md` | 现行安装矩阵，`README.md` 引用 |
| `docs/CONDUIT-INTEGRATION.md` | 现行运行时契约，`crates/envoy-service/tests/conduit_control.rs` 与根 `ARCHITECTURE.md` 引用 |
| `docs/plans/` | 现行执行计划目录，见其中的 `README.md` |
