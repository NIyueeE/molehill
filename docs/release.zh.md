# 发布流程

> English | [简体中文](release.zh.md)

发布完全**由 tag 驱动**。发布唯一的触发方式是推送 `v*` tag;整个流程由
`.github/workflows/release.yml` 负责,没有其它发布路径。

## 版本编号

molehill 是 [rathole](https://github.com/rapiz1/rathole) 的 fork,版本号
**沿上游序列续计**:上游最后一个版本是 v0.5.0,fork 从 v0.6.0 继续。该序列
内部遵循 [Semantic Versioning](https://semver.org/spec/v2.0.0.html)。

## 发布说明:CHANGELOG.md 是唯一来源

`CHANGELOG.md` 按
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) 格式维护。

- 开发期间,把值得记录的变更写到 `## [Unreleased]` 下。
- 打 tag 前,把内容移入带日期的小节:`## [x.y.z] - YYYY-MM-DD`
  (git tag 是同一版本加 `v` 前缀,例如 `v0.7.1`)。
- changelog 小节缺失或为空会**直接导致发布失败** —— 工作流在构建任何东西
  之前就报错退出。修复方式:补上小节,删除 tag,重新推送。永远不要在
  GitHub 上手改发布说明。

## Tag 推送策略:不随手发版

commit 随时可以推 —— 快速门守护它们,且不会触发任何公开动作。推送 `v*`
tag 是一次郑重的发布行为;四项前提条件(明确的人类请求、`Cargo.toml` 版本
一致、带日期的 CHANGELOG 小节、`just check` 全绿)是 [AGENTS.md §5](../AGENTS.md)
中的仓库规则 —— 发布工作流对其中版本与 CHANGELOG 两项做机械强制校验。

重新打 tag 仅用于修复失败的发布(删除 tag、修复、重新推送)。不发布地验证
某个 commit,请使用 CD 测试构建。

## 发布工作流做什么

1. **发布前检查**:Cargo.toml 版本 ↔ tag 版本、CHANGELOG 小节存在、
   `cargo audit`。
2. **构建矩阵**(9 个目标):linux gnu + musl、aarch64 musl(经 cross)、
   arm/armv7 musl(`embedded` 特性)、macOS x86_64 + aarch64、Windows
   msvc。musl 产物以完整 rustls 特性集构建;linux 产物经 UPX 压缩。原生与
   cross 目标都会在矩阵内跑测试。
3. **GitHub Release**:以**草稿**创建,说明提取自 `CHANGELOG.md`,附全部
   压缩包与 `SHA256SUMS`。快速过目后发布 —— 不要手改说明。
4. **GHCR**:用 musl 产物发布多架构 scratch 镜像
   (`ghcr.io/niyueee/molehill:<tag>` 与 `:latest`),并做冒烟测试。
5. **crates.io**:使用 `CRATES_IO_API_TOKEN` secret 发布
   `molehill-rathole`。

## CD 测试构建:按 commit、按平台的产物

`.github/workflows/test-build.yml` 从任意 commit 构建**测试产物**而不创建
发布:在 Actions 页手动触发,选择 `ref`(commit SHA、分支或 tag)与
`targets`(`linux`、`macos`、`windows`)。

- 产物是临时的(保留 7 天),绝不是 Release —— 不要把它的链接当作发布版
  分发,也不要写进 changelog。
- 典型用途:打 tag 前验证某个 commit 能在所有平台编译;在精确的 commit 上
  复现平台相关问题。
