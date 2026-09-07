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

单机对比(全部在回环上,拓扑 `访客 -> 服务端 -> 客户端 -> 后端`);所有
指标都**穿透隧道**测量——iperf3 与探针拨号各工具的暴露端口,绝不直连
后端。对比对象为 GitHub 最新 release 二进制(frp 0.71.0、上游 rathole
0.5.0、bore 0.6.0)。网络档(netem 施加于 `lo`,路径每一段都受影响)与
指标集见[方法论](#方法论)。

### 如何选择配置

下面的测量数据支撑默认值,也告诉你何时该偏离:

| 配置 | 适用场景 | 测得的代价 |
|---|---|---|
| **mux 开(默认)** | 一个客户端暴露**多个服务**;连接频繁建立/断开(HTTP/WebSocket/游戏会话);连接资源受限(FD、端口、**NAT 映射**——NAT 后每条物理隧道占一个映射);单条长连接比频繁建连更容易穿透 NAT/防火墙 | 单流上限约 10 Gbit/s(关掉为 20.1);丢包下所有流共享一个重传域(1% 丢包时 8 流 4.7 vs 18.5 Gbit/s) |
| **mux 关** | 单服务或少数长连接(如 SSH);**吞吐优先**(大文件传输):loopback 单流 20.1、8 流 28.2;弱网 + 大量并发流,每流独立重传更稳 | 每条流一条物理隧道:FD/端口/NAT 映射随流数增长;每条 visitor 连接多一次建连 RTT(本测试被连接池掩盖) |
| **noise** | 要加密且**内存与简洁优先**:22.3 MiB(vs TLS 33.8),预共享公钥,无证书/PKI | 吞吐比 TLS 低约 5-8% |
| **tls** | 要加密且**吞吐最优**(硬件 AES 加速);已有 PKI/证书体系;需要标准 CA 生态兼容 | 内存比 noise 多 50% |

两种加密都会让吞吐减半(3.8–4.1 vs 明文 10.2 Gbit/s),RTT 代价可忽略;
传输层选择不改变丢包行为。

具体写法:`mux` 选项与 `transport` 块见[配置文档](./docs/configuration.zh.md);
noise 密钥对与 TLS 证书见[传输层文档](./docs/transport.md);可直接运行的
配置见[快速开始](#快速开始)与[示例](./examples)。

### molehill vs 明文 TCP 同类工具

只对比明文 TCP 轴线(mux 开、不加密):加密竞品(如 chisel 的 SSH 隧道)
在这里不可比——molehill 自己的加密变体在下文单独隔离。

![Benchmark: molehill 0.7.2 vs plain-TCP peers](assets/benchmark-v0.7.2.png)

| 工具 | 单流 | 8 流 | echo RTT p50 | 内存 |
|---|---|---|---|---|
| **molehill(mux)** | 10.2 | 9.5 | 0.262 ms | 22.6 MiB |
| rathole 0.5.0 | 12.4 | 26.8 | 0.234 ms | 20.0 MiB |
| bore 0.6.0 | 14.2 | 27.0 | 0.495 ms | **8.4 MiB** |
| frp 0.71.0 | 4.8 | 6.3 | 0.375 ms | 72.1 MiB |

多路复用单隧道单流上限约 10 Gbit/s;按连接建通道的架构可达 12–14。
bore 最轻、是很强的纯 TCP 中继(不支持 UDP);frp 内存最高、吞吐最低。
时延档下所有工具 echo RTT 都约 101/1001 ms(bore 每连接多付往返);
1% 丢包下 molehill、rathole、bore 保持 4.3–4.6 Gbit/s,frp 崩到 0.8。

### molehill:多路复用代价(mux vs mux-off)

只变一个变量(复用开关),覆盖 loopback 与全部弱网档:

![Multiplexing cost](assets/benchmark-mux-v0.7.2.png)

| 档位 | mux 单流 | mux-off 单流 | mux 8 流 | mux-off 8 流 |
|---|---|---|---|---|
| loopback | 10.2 | 20.1 | 9.5 | 28.2 |
| rtt10 | 6.3 | 8.3 | 5.9 | 10.1 |
| rtt100 | 0.75 | 0.74 | 1.0 | 1.3 |
| 丢包 1% | 4.4 | 4.3 | 4.7 | 18.5 |
| 丢包 5% | 0.19 | 0.32 | 0.31 | 1.49 |
| 丢包 2% 突发 | 4.0 | 4.3 | 4.3 | 18.7 |

两条结论:纯延迟下差距收窄到持平(一旦 RTT 占主导,按连接建立的代价被
摊薄);**丢包下并发流的差距反转**——单隧道共享一个丢包/重传域,所有流
一起停滞,而 mux-off 每流独立重传。

### molehill:传输代价(mux vs noise vs tls)

只变一个变量(加密传输层),三者均开 mux:

![Transport cost](assets/benchmark-transport-v0.7.2.png)

| 配置 | 单流 | 8 流 | echo RTT p50 | 内存 |
|---|---|---|---|---|
| **mux(明文)** | 10.2 | 9.5 | 0.262 ms | 22.6 MiB |
| noise | 3.8 | 4.3 | 0.318 ms | 22.3 MiB |
| tls | 4.1 | 4.5 | 0.327 ms | 33.8 MiB |

加密让吞吐减半(硬件 AES 使 TLS 比软件 ChaCha20 的 noise 快约 5-8%);
RTT 代价在亚毫秒级;TLS 内存多 50%。弱网档下加密行跟随明文行。

### 方法论

- **环境**:单机回环;四跳(访客、服务端、客户端、后端)都是同一台
  机器上的进程,所以绝对数值与主机相关——只做同机同方法学的对比。
- **网络档**:loopback、rtt10、rtt100、丢包 1%、丢包 5%、丢包 2% 突发;
  netem 塑造整个 `lo`,每一跳都被延迟/丢包;"10 ms"档的 echo RTT 约
  100 ms 是因为路径多段。无 `CAP_NET_ADMIN` 时,rtt 档退化为用户态延迟
  代理、丢包档自动跳过。
- **指标**:TCP 吞吐(1/8 流;reps 中位数及其重传数)、连接路径 RTT(每
  arm 300 次新连接)、稳态数据路径 RTT(单连接 200 次 ping)、UDP 会话
  质量(单会话上的 RTT/丢包/抖动/最大包间隔)、队头阻塞探针(同服务上
  饱和打流 + 游戏式 pinger)、RSS(0.5 s 采样 server+client)。
- **严谨性**:每次对比只变**一个变量**(明文轴线、复用开关、传输层),
  共享对照组;arm 在隔离端口带上**串行**执行、每次全新进程(并行会
  争抢 CPU,使数字失效);molehill 3 次重复、peers 1 次。
- **已知限制**:回环不是真实网络(丢包/时延理想化,无真实拥塞);共享
  qdisc 会稀释丢包——UDP 丢包列是轻会话看到的残余份额,不是配置值;
  `pool_size=8` 连接池掩盖了建连握手成本(mux 的建连优势在此不可见);
  连接资源指标(FD、NAT 映射、TIME_WAIT)——mux 的主要收益——未测量。
- **复现**:`just bench-peers` → `just bench` → `just bench-plot` →
  `just bench-check`(原始数据在
  `benches/scripts/bench/results-v0.7.2.json`;仪式与回归门禁见
  docs/release.md)。

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
[配置文档](./docs/configuration.zh.md)。[示例配置](./examples) 覆盖了各种常见场景。

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

- [配置文档](./docs/configuration.zh.md) — 完整的配置规范、日志和调优
- [传输层](./docs/transport.md) — TLS 和 Noise Protocol 配置
- [构建指南](./docs/build-guide.md) — 构建定制、rustls 支持、最小化二进制
- [内部原理](./docs/internals.md) — 控制通道和数据通道的工作原理
- [示例](./examples) — 常见场景的配置

贡献与工程：

- [检查门](./docs/checks.md) — 每个门运行什么、被拦住时怎么办
- [Lint 策略](./docs/lint-policy.md) — lint 级别与豁免规则
- [发布流程](./docs/release.md) — 发布机制、版本编号、测试构建
- [仓库结构](./docs/structure.md) — 仓库里每个文件的用途
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
[docs/release.md](./docs/release.md)，仓库规则见
[AGENTS.md](./AGENTS.md)。
