<p align="center">
  <img src="https://raw.githubusercontent.com/NIyueeE/molehill/main/assets/molehill.svg" width="81" height="81">
</p>

<h1 align="center">molehill</h1>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/NIyueeE/molehill.svg"></a>
  <img src="https://img.shields.io/github/v/release/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/badge/rust-stable-93450a.svg">
  <img src="https://github.com/NIyueeE/molehill/actions/workflows/ci.yml/badge.svg">
</p>
<p align="center">
  <img src="https://img.shields.io/github/stars/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/github/forks/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/github/last-commit/NIyueeE/molehill.svg">
</p>

<p align="center">[rathole](https://github.com/rapiz1/rathole) 的社区维护 fork 版本。</p>

[English](README.md) | [简体中文](README.zh.md)

molehill，类似于 [frp](https://github.com/fatedier/frp) 和 [ngrok](https://github.com/inconshreveable/ngrok)，可以将 NAT 后的设备上的服务通过具有公网 IP 的服务器暴露到互联网。

<!-- TOC -->

- [molehill](#molehill)
  - [特性](#特性)
  - [快速开始](#快速开始)
  - [部署](#部署)
    - [二进制](#二进制)
    - [systemd](#systemd)
    - [容器](#容器)
  - [配置](#配置)
  - [文档](#文档)
  - [开发](#开发)

<!-- /TOC -->

## 特性

- **高性能** 具有更高的吞吐量，高并发下更稳定。
- **低资源消耗** 内存占用远低于同类工具。[二进制文件最小](docs/build-guide.md)可以到 **~500KiB**，可以部署在嵌入式设备如路由器上。
- **客户端声明服务** 从 v0.7 起，服务端不再需要逐服务配置：客户端声明要暴露的内容（包括公网端口），服务端只通过 `allow_ports` 白名单和共享 `default_token` 执行策略。
- **多路复用** 默认情况下，每个数据通道都作为 yamux 流跑在一条隧道连接上，省去每条连接的握手并显著减少文件描述符。`mux` 相关选项与 `mux = false` 回退路径见[配置文档](./docs/configuration.md)。
- **安全性** 共享 token 强制鉴权，`allow_ports` 白名单限制客户端可暴露的端口。使用 Noise Protocol 可以简单地配置传输加密，而不需要自签证书。同时也支持 TLS。
- **热重载** 支持配置文件热重载，动态添加或移除端口转发服务。

## 基准测试

单机同类对比(明文 TCP,单机拓扑 `访客 -> 服务端 -> 客户端 -> 后端`;所有
指标都**穿透隧道**测量——iperf3 拨号到各工具的暴露端口)。对比对象均为
GitHub 最新 release 二进制:frp 0.71.0、rathole 0.5.0(上游)、bore 0.6.0。
弱网档把 netem 施加在回环接口上,路径的每一段都被延迟/丢包
——对所有工具一视同仁("10 ms"档穿透多段路径后 echo RTT 约为 100 ms;
放大系数与段数相关、与工具无关)。

### molehill vs 明文 TCP 同类工具

对比图**刻意限定在明文 TCP 轴线上**:molehill 默认配置(mux 开、不加密)
对同样传输特性的同类工具。加密竞品(如 chisel 内置的 SSH 隧道)被排除
——它们的数字在这里不可比;molehill 自己的加密变体在下图单独隔离。

![Benchmark: molehill 0.7.2 vs plain-TCP peers](assets/benchmark-v0.7.2.png)

穿透隧道的吞吐把工具真正区分开:`mux` 档共享一条 yamux 通道(单流上限),
对比工具则按连接建通道:

| 工具 | 单流 (Gbit/s) | 8 流 (Gbit/s) | echo RTT p50 | echo RTT p99 | 内存(平均 RSS) |
|---|---|---|---|---|---|
| **molehill 0.7.2**(mux,默认) | 10.2 | 9.5 | 0.262 ms | 0.333 ms | 22.6 MiB |
| rathole 0.5.0(上游) | 12.4 | 26.8 | 0.234 ms | 0.312 ms | 20.0 MiB |
| bore 0.6.0 | 14.2 | 27.0 | 0.495 ms | 0.629 ms | **8.4 MiB** |
| frp 0.71.0 | 4.8 | 6.3 | 0.375 ms | 0.673 ms | 72.1 MiB |

- 多路复用单隧道路径(mux)的单流上限约 10 Gbit/s;按连接建通道的架构
  (rathole、bore)可达 12–14。
- bore 最轻(8.4 MiB)且是很强的纯 TCP 中继——但完全不支持 UDP 转发。
- frp 内存最高(72 MiB),此处吞吐也最低。

弱网档(netem 施加于回环每一跳):附加时延下的连接路径 RTT;bore 付两个
以上往返(其 local 转发每次都经控制端口现拨),预建通道池只需付基线值:

| 工具 | rtt10: echo RTT p50 | rtt100: echo RTT p50 | rtt10: 单流 (Gbit/s) |
|---|---|---|---|
| **molehill 0.7.2**(mux,默认) | **101.3 ms** | **1001.4 ms** | 6.3 |
| rathole 0.5.0(上游) | 101.1 ms | 1001.3 ms | 6.3 |
| bore 0.6.0 | 141.8 ms | 1402.0 ms | 10.2 |
| frp 0.71.0 | 101.5 ms | 1001.6 ms | 1.8 |

| 工具 | 丢包 1%: 单流 (Gbit/s) | UDP 会话丢包 | UDP 最大间隔 |
|---|---|---|---|
| **molehill 0.7.2**(mux,默认) | **4.4** | 5.0% | 61 ms |
| rathole 0.5.0(上游) | 4.3 | 6.0% | 60 ms |
| bore 0.6.0 | 4.6 | -(无 UDP) | - |
| frp 0.71.0 | 0.8 | 3.5% | 60 ms |

- 1% 丢包下,多路复用/通道池化的数据路径(molehill、rathole)与纯中继
  (bore)保持 4.3–4.6 Gbit/s,而 frp(0.8)崩塌——丢包
  容忍度比回环裸速更能区分转发架构。
- 所有支持 UDP 转发的工具,会话质量都退化平缓:残余丢包 ≤6%(共享
  qdisc 使配置的 1% 落点不均),最大包间隔 ≤61 ms——游戏类会话可以存活。

### molehill 配置对比:多路复用与加密

配置图展示的是**同一个二进制的四种配置**——不是四个工具:`mux = false`
隔离多路复用的代价;`noise` 与 `tls` 行把传输层切换为加密的
Noise / TLS(复用保持不变),隔离加密的代价。

![Benchmark: molehill configurations](assets/benchmark-molehill-v0.7.2.png)

| 配置 | 单流 (Gbit/s) | 8 流 (Gbit/s) | echo RTT p50 | echo RTT p99 | 内存(平均 RSS) |
|---|---|---|---|---|---|
| **mux(默认)** | 10.2 | 9.5 | 0.262 ms | 0.333 ms | 22.6 MiB |
| `mux = false` | 20.1 | 28.2 | 0.217 ms | 0.268 ms | 18.7 MiB |
| noise | 3.8 | 4.3 | 0.318 ms | 0.383 ms | 22.3 MiB |
| tls | 4.1 | 4.5 | 0.327 ms | 0.419 ms | 33.8 MiB |

- 多路复用用单流吞吐换取连接效率:同一个二进制 `mux = false` 时为
  20.1 / 28.2 Gbit/s。
- 加密让吞吐减半:noise(3.8)与 tls(4.1)都约为明文 mux 行的一半,而
  连接路径开销仍在亚毫秒级;TLS 额外多占内存(33.8 MiB)。
- 弱网档下加密行跟随明文行:1% 丢包时三个变体都保持 3.6–3.8 Gbit/s
  ——传输层选择不改变丢包行为。

- 绝对数值与主机相关;对比为同机同方法学。复现方式:`just bench-peers` →
  `just bench` → `just bench-plot` → `just bench-check`
  (原始数据 `benches/scripts/bench/results-v0.7.2.json`;仪式与回归门禁
  见 docs/release.md)。

## 快速开始

一个全功能的 `molehill` 可以从 [release](https://github.com/NIyueeE/molehill/releases) 页面下载。或者 [从源码编译](docs/build-guide.md) **获取其他平台和最小化的二进制文件**。

`molehill` 的使用和 frp 非常类似：转发服务由客户端声明，服务端只配置共享 token 和端口策略。

使用 molehill 需要一个有公网 IP 的服务器，和一个在 NAT 或防火墙后的设备，其中有些服务需要暴露在互联网上。

假设你在家里的 NAT 后面有一个 NAS，并且想把它的 ssh 服务暴露在公网上：

1. 在有一个公网 IP 的服务器上

创建 `server.toml`，内容如下，并根据你的需要调整。

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333" # `2333` 配置了服务端监听客户端连接的端口
default_token = "change-me" # 共享 token，客户端必须一致
allow_ports = ["5202"] # 允许客户端注册的公网端口
```

然后运行:

```bash
./molehill server.toml
```

2. 在 NAT 后面的主机上（你的 NAS）

创建 `client.toml`，内容如下，并根据你的需要调整。

```toml
# client.toml
[client]
remote_addr = "myserver.com:2333" # 服务器的地址，端口必须和 `server.bind_addr` 中的端口一致
default_token = "change-me" # 必须和服务端一致才能通过验证

[client.services.my_nas_ssh]
local_addr = "127.0.0.1:22" # 需要被转发的服务地址
remote_bind_addr = "0.0.0.0:5202" # 在服务端暴露的公网地址
```

然后运行:

```bash
./molehill client.toml
```

3. 现在客户端会尝试连接服务器的 `myserver.com:2333`，任何访问 `myserver.com:5202` 的流量都会被转发到客户端的 `22` 端口。

这样你就可以通过 `ssh myserver.com:5202` 来 ssh 到你的 NAS。

如果想在 Linux 上把 `molehill` 作为后台服务运行，可以参考
[systemd 示例](./examples/systemd) 或
[容器示例](./examples/container)。

## 配置

`molehill` 会根据配置文件自动判断运行模式（server/client），也可以通过 `--server` / `--client` 强制指定。完整的配置规范、日志和调优选项见
[配置文档](./docs/configuration.md)。[示例配置](./examples) 覆盖了各种常见场景。

## 部署

### 二进制

从 [release 页面](https://github.com/NIyueeE/molehill/releases) 下载对应平台的预编译二进制，或者
[从源码编译](./docs/build-guide.md) 获取其他平台和最小化的二进制。

```bash
./molehill server.toml   # 在公网服务器上
./molehill client.toml   # 在 NAT 后的设备上
```

### systemd

[systemd 示例](./examples/systemd) 演示了如何把 molehill 作为 systemd 服务运行，包含 root 和 rootless 两种方式，以及多实例管理。

### 容器

官方多架构镜像（linux/amd64、linux/arm64）发布在
`ghcr.io/niyueee/molehill`。镜像是构建在 `scratch` 上的单个静态 musl 二进制（约 8 MiB），以非 root UID 1000 运行，内置了 CA 证书用于 TLS 验证，并且与常规发布构建使用相同的默认特性集（包含多路复用）。

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

镜像内不包含任何配置——挂载你的配置文件，并把文件名作为参数传入。更多部署方式见
[容器示例](./examples/container)，包括 Docker Compose（`compose.yaml` / `compose.bridge.yaml`）和 Podman
Quadlet（`molehill-server.container` / `molehill-client.container`）。

## 文档

使用 molehill：

- [配置文档](./docs/configuration.md) — 完整的配置规范、日志和调优
- [传输层](./docs/transport.md) — TLS 和 Noise Protocol 配置
- [构建指南](./docs/build-guide.md) — 构建定制、rustls 支持、最小化二进制
- [内部原理](./docs/internals.md) — 控制通道和数据通道的工作原理
- [示例](./examples) — 常见场景的配置

贡献与工程：

- [检查门](./docs/checks.zh.md) — 每个门运行什么、被拦住时怎么办
- [Lint 策略](./docs/lint-policy.zh.md) — lint 级别与豁免规则
- [发布流程](./docs/release.zh.md) — 发布机制、版本编号、测试构建
- [仓库结构](./docs/structure.zh.md) — 仓库里每个文件的用途
- [贡献指南](./CONTRIBUTING.md) — 环境搭建与工作流
- [安全策略](./SECURITY.md) — 漏洞报告
- [`HANDOFF.md`](./HANDOFF.md) — 当前工作状态；计划中的工作和将来的设计文档

## 开发

molehill 使用 Rust 编写（2024 edition）；`rust-toolchain.toml` 声明
`channel = "stable"` 并附带 clippy 与 rustfmt 组件 —— 不要硬编码版本号。分层
git hooks 守护每次 commit 与 push，CI 运行同一条链：

```bash
just setup   # 激活 git hooks（core.hooksPath githooks）并安装检查工具
just check   # fmt / secrets / machete / docs / clippy + audit / deny / outdated / test
```

molehill 是 [rathole](https://github.com/rapiz1/rathole) 的社区 fork；版本号
沿上游序列续计（上游最后一个版本是 v0.5.0）。发布机制见
[docs/release.zh.md](./docs/release.zh.md)，仓库规则见
[AGENTS.md](./AGENTS.md)。
