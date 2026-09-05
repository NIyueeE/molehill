# 检查门

> English | [简体中文](checks.zh.md)

快速门在每次 commit 前运行,重量级门在每次 push 前运行,CI 则在每次 push /
pull request 时通过 `just check` 运行完整的链。

## 工具

这些门依赖四个外部工具;`just setup` 会安装缺失的工具(并激活 git hooks):

```bash
cargo install cargo-machete cargo-audit cargo-outdated cargo-deny --locked
```

`cargo fmt` 和 `cargo clippy` 由 `rust-toolchain.toml` 声明的工具链
(`channel = "stable"` + clippy/rustfmt 组件)自带。

## 每次 commit — `githooks/pre-commit`

| # | 门 | 命令 | 目的 |
|---|------|---------|---------|
| 1 | fmt | `cargo fmt --all -- --check` | 代码风格 |
| 2 | secrets | `githooks/check-secrets` | 对暂存区做密钥扫描 |
| 3 | machete | `cargo machete` | 未使用的依赖 |
| 4 | docs | `githooks/check-docs` | 文档 ↔ 代码对齐 |
| 5 | clippy | `cargo clippy --all-targets -- -D warnings` | 严格 lint,默认特性 |
| 6 | clippy (gates) | `cargo clippy --all-targets --no-default-features --features server,client -- -D warnings` | 特性门控的代码路径 |

与模板的差异:molehill 的两种 TLS 后端(`native-tls` 与 `rustls`)及其
websocket 变体**互斥**,因此 clippy 运行两遍(默认特性,再仅
`server,client`),而不是用 `--all-features` 跑一遍。`just check` 与之完全
一致。

必须携带密钥形状字符串的行(例如密钥格式文档)应加上
`security-scan:allow` 标记和原因;`check-secrets` 会跳过这些行。

## 每次 push — `githooks/pre-push`

| # | 门 | 命令 | 目的 |
|---|------|---------|---------|
| 7 | audit | `cargo audit` | RustSec 安全通告 |
| 8 | deny | `cargo deny check` | 许可证 / 禁用 / 通告策略(deny.toml) |
| 9 | outdated | `cargo outdated --root-deps-only` | 过期的直接依赖 |
| 10 | test | `cargo test --quiet -- --test-threads=1` | 测试套件(按设计串行) |

测试**串行**运行(`--test-threads=1`):集成测试会在固定端口上启动真实的
server/client 对,并行执行会在端口上竞争。

## 一次跑完

```bash
just check   # 与 hooks + CI 完全一致
```

其它配方:`just fmt`(自动修复)、`just test`、`just powerset`(cargo-hack
特性幂集,即 CI 的 `features` 任务)、`just container`(scratch 镜像)。

## 当门拦住你时

先修代码。豁免是最后手段:仅限代码层
(优先 `#[expect(...)]` 而非 `#[allow]`),最小范围,并附原因注释。永远不要
削弱 `[lints]`、hooks 或 CI。参见
[Lint 策略](lint-policy.zh.md)与 [AGENTS.md](../AGENTS.md)。
