# Lint 策略

> English | [简体中文](lint-policy.zh.md)

所有 lint 都声明在 `Cargo.toml` 的 `[lints]` 中;下表是唯一事实来源。修改
lint 级别必须在**同一个 commit** 中更新本页(双语)——
`githooks/check-docs` 负责机械部分的校验。

## 已声明的 lint

| Lint | 级别 | 原因 |
|------|-------|-----|
| `missing_docs` | warn | 公开 API(lib 表面、配置结构体、CLI)必须携带文档注释 |
| `unsafe_code` | warn | unsafe 仅限显式声明加入的、经过审计的模块;每个 `unsafe` 块必须附 `SAFETY` 注释 |
| `linker_messages` | allow | cross 的 musl 链接器包装会向 stderr 输出 trace;真正的链接错误仍会使构建失败 |
| `dbg_macro` | deny | 生产代码中不留调试残留 |
| `expect_used` | deny | 不允许隐藏 panic;显式处理错误 |
| `panic` | deny | 反向代理不能因不可信输入而崩溃 |
| `undocumented_unsafe_blocks` | deny | 每个 `unsafe` 块必须附 `SAFETY` 注释 |
| `unwrap_used` | deny | 不允许隐藏 panic;显式处理错误 |
| `todo` | warn | 桩代码应被移除而不是堆积(AGENTS.md §9) |
| `pedantic` | deny | 整个 pedantic clippy 组(与 rust-agents-template 对齐);确实不适用的成员只在代码内逐项豁免并注明原因,绝不在本处放宽 |

## 豁免纪律

先修代码;豁免是最后手段,且只允许代码层:

- 优先 `#[expect(clippy::lint_name)]`(一旦该 lint 不再触发,编译器会提示
  你清理,避免过期豁免),退而求其次用 `#[allow(clippy::lint_name)]`;
- 最小范围:单条语句或单个函数;禁止函数组、模块级 `#![allow(...)]` 或
  crate 级放宽;
- 豁免点必须有一行原因注释(如有相关 issue 一并附上)。

只有两种正当场景:

1. **确实无法避免** —— 业务需要,且不存在同等合理的替代方案;
2. **上游问题** —— 误报、宏/derive 生成的代码,或依赖自身的审计噪音。

永远不要通过修改 `Cargo.toml` 的 `[lints]`、`githooks/pre-commit` 或任何
检查命令来"让错误消失"。所有额外检查(machete、audit、deny、outdated、
docs-sync、secret scan)遵循同样的纪律。

## 与 rust-agents-template lint 集的差异

无。模板的 `clippy::pedantic`(deny)与 `missing_docs`(warn)已在 lint
迁移中纳入上表。个别不适用的 pedantic 成员只在代码内逐项豁免 —— 优先
`#[expect(..., reason = "...")]` —— 并遵循与其他 lint 相同的豁免纪律。
