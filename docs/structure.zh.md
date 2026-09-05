# 仓库结构

> English | [简体中文](structure.zh.md)

本仓库每个文件和目录的用途。更深入的行为文档:
[配置](configuration.md)、
[transport](transport.md)、[internals](internals.md)、
[构建指南](build-guide.md)。

## 根目录

| 路径 | 用途 |
|------|---------|
| `Cargo.toml` | crate 清单;`[lints]` 是策略,文档见 [lint-policy](lint-policy.zh.md) |
| `Cargo.lock` | 锁定的依赖图(已提交;CI 构建用 `--locked` 校验) |
| `build.rs` | 通过 vergen 注入构建元数据(git SHA、时间戳、特性、目标三元组) |
| `justfile` | 任务入口:`just setup` / `fmt` / `test` / `check` / `powerset` / `container` |
| `rust-toolchain.toml` | `channel = "stable"` + clippy/rustfmt 组件;不要硬编码版本号 |
| `deny.toml` | cargo-deny 策略:许可证、禁用、通告(pre-push + CI) |
| `.editorconfig` | 编辑器默认值(Rust 4 空格,YAML/TOML 2 空格,md 保留行尾空格) |
| `AGENTS.md` | 面向 AI 代理与人类的仓库规则;每次会话的入口 |
| `HANDOFF.md` | 当前工作状态、决策、未决事项 —— 读完 AGENTS.md 后阅读 |
| `CHANGELOG.md` | 发布说明的唯一来源(Keep a Changelog);发布的前置门 |
| `CONTRIBUTING.md` / `SECURITY.md` | 贡献者上手;私有漏洞报告渠道 |
| `README.md` / `README.zh.md` | 双语着陆页;必须记录 edition、channel、`just setup`、`just check`、hooks 激活方式 |
| `Containerfile` | 用 musl 发布产物组装的 scratch 容器镜像 |
| `LICENSE` | Apache-2.0 |
| `assets/` | logo 与基准测试图表 |

## 自动化

| 路径 | 用途 |
|------|---------|
| `githooks/pre-commit` | 快速门:fmt、密钥扫描、machete、文档对齐、clippy ×2 |
| `githooks/pre-push` | 重量级门:audit、deny、outdated、测试 |
| `githooks/check-secrets` | 暂存区密钥扫描(用 `security-scan:allow` 标记豁免某行) |
| `githooks/check-docs` | 文档 ↔ 代码对齐(hook 命令、lint、edition、channel、README 索引、CI 入口) |
| `.github/workflows/ci.yml` | `just check` 链 + 特性幂集 + 分特性测试矩阵 + 最小体积检查 + 4 平台构建 |
| `.github/workflows/release.yml` | tag 驱动发布:版本/changelog 门、9 目标矩阵、草稿 Release、GHCR、crates.io |
| `.github/workflows/test-build.yml` | 手动触发的按 commit CD 测试构建(绝不发布) |
| `.github/dependabot.yml` | 每周 cargo + GitHub Actions 更新 |
| `.github/ISSUE_TEMPLATE/`、`PULL_REQUEST_TEMPLATE.md` | issue 表单(禁用空白 issue)、PR 检查清单 |

## 源码

| 路径 | 用途 |
|------|---------|
| `src/main.rs` | 二进制入口:CLI 解析、信号处理、日志初始化 |
| `src/lib.rs` | 库根:运行模式判定、主事件循环、配置监视器生命周期 |
| `src/cli.rs` | clap derive 的 CLI 定义 |
| `src/protocol.rs` | 线协议(Hello/Auth/Ack/命令)、postcard 序列化、协议版本 |
| `src/common.rs` + `src/common/` | 常量、DNS/keepalive/重试助手、`MultiMap` |
| `src/config.rs` + `src/config/` | TOML 解析与校验(`Config`、`ClientConfig` 等、`MaskedString`)、热重载监视器 |
| `src/core/client.rs` | 客户端模式:控制通道、鉴权、注册、数据通道请求 |
| `src/core/server.rs` | 服务端模式:注册策略、急切绑定、连接池 |
| `src/logging.rs` | 带颜色、感知 span 的日志格式化器 |
| `src/transport.rs` + `src/transport/` | `Transport` trait + tcp / native-tls / rustls / noise / websocket / multiplex 实现 |

## 测试、基准、示例、文档

| 路径 | 用途 |
|------|---------|
| `tests/integration_test.rs` | 启动真实 server+client 对;跨传输类型的 TCP/UDP 转发 |
| `tests/common/mod.rs` | echo/pingpong 打流器与运行助手 |
| `tests/for_tcp/`、`tests/for_udp/`、`tests/config_test/` | 传输夹具与有效/无效配置 |
| `benches/` | HTTP 延迟(vegeta)与内存采样脚本 |
| `examples/` | 可运行的配置:tls、noise_nk、udp、use_proxy、minimal、iperf3、unified、systemd、container、full |
| `docs/` | 用户文档(configuration、transport、build-guide、internals)+ 治理文档(checks、lint-policy、release、structure),全部双语(`*.md` + `*.zh.md`) |
