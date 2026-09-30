<p align="center">
  <img src="assets/molehill.svg" width="257" height="257">
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

<p align="center">面向 NAT 穿透的安全、稳定、高性能反向代理。</p>

[English](README.md) | [简体中文](README.zh.md)

molehill，类似于 [frp](https://github.com/fatedier/frp) 和 [ngrok](https://github.com/inconshreveable/ngrok)，可以将 NAT 后的设备上的服务通过具有公网 IP 的服务器暴露到互联网。

<!-- TOC -->

- [molehill](#molehill)
  - [特性](#特性)
  - [基准测试](#基准测试)
    - [如何选配置](#如何选配置)
    - [molehill vs 明文 TCP 对端](#molehill-vs-明文-tcp-对端)
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
- **多路复用** 每个数据通道都作为 yamux 流跑在一个弹性隧道池（上限 `[client.data.tcp|kcp].max_tunnels`，默认 4）的某条隧道上——省去每条连接的握手、显著减少文件描述符，吞吐超越单条 TCP 流并隔离队头阻塞（丢段只停滞自己的隧道）。池冷启动、按需增长，空闲的客户端不持有任何隧道；可选的 `default_carrier = "kcp"`（feature `kcp`）把数据面换成 KCP-over-UDP 会话。`[client.data]` 选项与 `mode = "direct"` 回退路径见[配置文档](./docs/configuration.zh.md)。
- **安全性** 共享 token 强制鉴权，`allow_ports` 白名单限制客户端可暴露的端口。可选的 Noise Protocol 只需一对预共享 X25519 密钥即可加密传输——没有 PKI、没有 CA；设置 `resume = true` 后，重连用一次 MAC 证明持有上次会话的握手摘要即可，不必重跑密钥交换(建连从每对 442.7 us 降到 38.5 us)。`plain` 为明文转发。
- **热重载** 支持配置文件热重载，动态添加或移除端口转发服务。

## 基准测试

单机对比(`访客 → 服务端 → 客户端 → 后端`,四跳都在同一台机器上)。一切
**穿过隧道**测量:探针拨的是每个工具的暴露端口,绝不直接连它转发的后端。
对端工具为最新 GitHub release 构建(frp、rathole 上游、nps,版本随每次运行
一起记录)。每个工具都被驱以**完全相同的工作负载**,同时网络条件按脚本化的
阶段表原地切换,因此工具会话从不重建——它如何适应劣化再恢复的路径,本身就是
测量的一部分。

### 如何选配置

默认值——`mode = "multiplex"`、`max_tunnels = 4`、`carrier = "tcp"`、
明文传输——对绝大多数人是正确的起点;只有树上出现明确分支时才偏离。其余由
三个问题决定,每个答案就是 `[client.data]` 或 `[client.services.<name>]` 里的
一行:**是否加密**(设 `[client.transport] type = "noise"` 并放置密钥,见
[传输](docs/transport.zh.md));**并发多少**(提高 `max_tunnels`——每条隧道约
承载 64 条并发连接,`8` ≈ 512——或用 `[server.data] stripe_count` 把一条连接
摊到多条并行通道);**路径什么状况**(TCP 数据隧道被限速、或需要延迟优先的
UDP 时值得 A/B `carrier = "kcp"`;有损路径保持 `max_tunnels >= 4`,让池聚合
并隔离队头阻塞)。

在这两个数之间做取舍,最好在**你自己的路径上**测,而不是从表里读:
**可持续负载**(交互流仍满足 50 ms SLO 时,工具能扛多少条 bulk 流)与
**工作点成本**(每承载 1 Gbit/s 的 CPU 秒)。如何跑这套对比见
[基准测试](docs/benchmarks.zh.md);各设置本身(含分步决策树)见
[配置文档](docs/configuration.zh.md#选择配置决策树)。

### molehill vs 明文 TCP 对端

每个工具跑同一份工作负载——1 条交互流(SLO 仪器)、N = 20 条 bulk TCP 流、
每秒 16 次短连接、1 条 UDP 会话——同时路径按阶段表推进(netem 塑造整个
`lo`,控制面保持不整形)。下图是本主机上的 v0.10.0 一次运行(发布二进制的
默认值:`multiplex`、每服务最多四条隧道、按需弹性增长、明文):橙色线是 bulk
吞吐,蓝色点是交互流 RTT,阴影带是路径档,虚线是 SLO(p99 ≤ 50 ms,
错误率 ≤ 0.5%)。

![Soak: molehill 与对端在阶段日程上的形态](assets/soak-v0.10.0.png)

同一轮数据的小倍数图——每个阶段一个面板,每个工具一根棒棒糖(圆点 = p50,
横杠 = p99,竖线 = 最差一秒),"哪个工具在哪个条件下更好"不用查表就能看出来:

![逐阶段、逐工具的交互流 RTT](assets/soak-v0.10.0-stages.png)

**交互流 RTT p99,逐阶段**(ms)。饱和运行里被整形的阶段只有几十个样本,
样本不足一百的阶段报的是*最差的那次观测*而不是 p99——样本数就在结果文件里,
与这些数字并排。`~` 表示**整形**档:数值是本轮的读数,但主导它的是 harness 自己
装上的队列,而单轮并不重复——同一套未改动方法在本机跑三轮,这些格子移动 5-24%——
所以 `~` 列是背景,**不在其中标出赢家**。`‡` 表示该阶段还记录到了 wedge(一段静默,
在图上画成扁平段);恢复过来的阶段会同时带着标记和它的数字。

| 工具 | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean(重复) |
|---|---|---|---|---|---|---|---|---|
| molehill (mux) | 10.0 | ~6242‡ | ~1135 | ~3629 | ~1490 | ~8010‡ | ~8184‡ | 9.6 |
| frp | 3.0 | ~8231‡ | ~1066 | ~4498‡ | ~1521 | ~7604‡ | ~7479‡ | 3.1 |
| rathole | 96.9 | ~6701‡ | ~1163 | ~4102 | ~1555 | ~7554‡ | ~6993‡ | 102 |
| nps | 64.6 | ~477 | ~1091 | ~2453 | ~1536 | ~7488‡ | ~6276‡ | 57.7 |

**Bulk 吞吐逐阶段**(Gbit/s,**整个测量窗口**上的负载,而不是它最好的那一秒:
netem 会把一次突发送进它选中的任意区间,所以峰值是整形器的日程,不是路径)。
决定哪一侧说话的是**测量本身**:发送侧,除非它的 `end` 事件、或该阶段有一半以上
区间为零字节,说明它的写入没有跟上路径——那时读数取**接收侧自己的窗口**,用 `*`
标出。`— †` 是完全没有读数的阶段,并写明原因(四个臂的 `jitter` 都是它:那里的零
字节是拥塞崩塌,不是被缓冲的发送侧)。限速档读的是整形器自己的数字——每个臂都是
0.100 与 0.020 Gbit/s——因为限速档上客户端的窗口是有界的:没有这个界,同一格在一
条 20 Mbit 路径上会读 0.033 Gbit/s,**高于**标称,因为传输越过了它被测量的那个
阶段。限速档按构造没有对比度,这张表如实说明,而不是拿它给工具排名。

| 工具 | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean(重复) |
|---|---|---|---|---|---|---|---|---|
| molehill (mux) | 16.331 | 5.224 | 9.702 | 5.270 | 0.100 | 0.020 | — † | 16.438 |
| frp | 6.050 | 5.579 | 5.727 | 5.241 | 0.100 | 0.019 | — † | 6.059 |
| rathole | 12.877 | 5.189 | 9.680 | 5.295 | 0.100 | 0.019 | — † | 12.889 |
| nps | 0.132 | 0.152 | 0.142 | 0.165 | 0.100 | 0.020 | — † | 0.134 |

**这些数字必须超过的噪声。** 日程在每条时间轴的首尾各测一次 `clean`,所以每个工具的
两次 clean 读数就是同一条件相隔约一小时的两个样本——**这一轮运行自己的重复实验**,
也是其余每一格都要拿来对照的尺度。`just soak-check` 会把它报出来:

| 工具 | clean bulk 读数 | clean 交互 p99 |
|---|---|---|
| molehill (mux) | 16.331 – 16.438 Gbit/s (**0.7 %** apart) | 9.6 – 10.0 ms |
| frp | 6.050 – 6.059 Gbit/s (**0.1 %** apart) | 3.0 – 3.1 ms |
| rathole | 12.877 – 12.889 Gbit/s (**0.1 %** apart) | 96.9 – 102 ms |
| nps | 0.132 – 0.134 Gbit/s (**1.3 %** apart) | 57.7 – 64.6 ms |

**它能扛多少。** 同一份产物还带着负载斜坡:一条全新的交互连接第一次打破 SLO
(p99 50 ms、错误率 0.5%)时的 bulk 负载档。它与阶段日程是**两把不同的尺子**——
日程问"路径变化时会发生什么",斜坡问"上限在哪里"——两者互不印证。

| 工具 | 可持续流数 | 上限 | 余量 | 打破时的原因 |
|---|---|---|---|---|
| molehill (mux) | 8 | 8 | 0.0 | never broke |
| frp | 8 | 8 | 0.0 | never broke |
| rathole | 8 | 8 | 0.0 | never broke |
| nps | 0 | 8 | 1.0 | interactive p99 201.156 > 50.0 |

三个臂跑满了斜坡自己的 8 流上限,所以这读作**下界**("至少 8"),而不是测到的最大值;
nps 在提供给它的第一条流上就打破了 SLO。

**这些形状说明什么。** 每个工具在劣化档上都退化、在回归 clean 档上都恢复——
最后一段测的就是这个恢复;一个保持 wedge 的工具就是一个发现。在干净路径上,
molehill 承载 16.3-16.4 Gbit/s,rathole 12.9(相差 27%,两侧的重复实验都在 1% 以内),
frp 6.1,nps 0.13;延迟的顺序在顶部反过来——frp 3.0 ms、molehill 9.6-10.0、nps 58-65、
rathole 97-102——所以 molehill 与 frp 是两个轴都待在 SLO 内的臂。`loss1`
(10 ms 延迟、1% 丢包)把吞吐那一对与 frp 分开:9.70 与 9.68 Gbit/s 对 5.73,nps 0.14。
整形档的交互数字是*背景*:主导它们的是 harness 自己装上的队列,未改动代码重跑时它们
的摆动超过其中任何一对工具的差距,而且每个臂在 `rate20` 与 `jitter` 上都出现 wedge——
那是路径,不是某一个工具。诚实的劣势原样记在表里:clean 档交互代价 frp 3.0 ms 对
molehill 9.0;以及 `nps` 在**每一个**档位(包括 clean)都有一部分区间读数为零字节,
这是其他三个工具都没有的。

对端由同一份工作负载驱动,画在同一批面板里;漂移轴(全程的打开 fd、RSS 与
CPU 斜率)见 `soak-v0.10.0-drift.png`,UDP 会话的 RTT/丢包见
`soak-v0.10.0-udp.png`(画的是滑动丢包**率**,不是丢包事件的计数),负载斜坡见
`soak-v0.10.0-capacity.png`。

以上是本主机上的 v0.10.0 数字,用的是本页描述的方法。**主机的*实例*不是方法**:
能达到 loopback 上限的那两个臂,在本次工作用到的不同容器实例之间,clean 吞吐掉了
四分之一到三分之一(molehill 21.8 -> 16.7,rathole 21.2 -> 12.8 Gbit/s),而 frp 与
nps 纹丝不动——所以顶部那一对的先后是那一轮运行的事实,不作为长期主张。每份结果
文件都记录主机、方法与两个不含工具的标定(CPU 状态与 loopback 路径),`just soak-check` 拒绝比较在这些上不一致的运行;
只有同模型、同方法、同主机的运行之间才可直接比较,而每一轮都由它自身的完整性、
端点与 SLO 检查来判定。
每个阶段的数字在结果文件里都带着自己的样本数(`rtt_n`):交互样本不足一百的阶
段,其 p99 报的是*最差的那次观测*——饱和运行里被整形的阶段(几十个样本)正是如
此。
怎么读图、如何复现一轮,以及发布门的判定见
[基准测试方法](docs/benchmarks.zh.md)。
怎么细读一张图(对数轴、阶梯线、wedge 红条、每个色带代表什么)、阶段日程、
测试类型,以及如何在自己的硬件上复现一轮:
[基准测试方法](docs/benchmarks.zh.md)。

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
default_token = "use_a_secret_that_only_you_know" # 与客户端共享的密钥 # security-scan:allow documentation placeholder

# 动态注册的总开关：只有这里覆盖的端口才能被客户端占用
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333" # molehill 监听客户端连接的端口
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
default_token = "use_a_secret_that_only_you_know" # 必须和服务端的 `default_token` 一致 # security-scan:allow documentation placeholder

[client.control]
default_remote_addr = "myserver.com:2333" # 服务器的地址，端口必须和 `server.control.bind_addr` 中的端口一致

[client.services.my_nas_ssh]
local_addr = "127.0.0.1:22" # 需要被转发的服务地址
remote_bind_addr = "0.0.0.0:5202" # 在服务端暴露的公网地址
```

然后运行:

```bash
./molehill client.toml
```

启动时客户端会在服务端注册 `my_nas_ssh`，服务端按 `allow_ports` 校验端口并开始转发。在 `client.toml` 中增删服务会通过热重载即时生效——无需改动服务端配置。

3. 现在客户端会尝试连接服务器的 `myserver.com:2333`，任何访问 `myserver.com:5202` 的流量都会被转发到客户端的 `22` 端口。

这样你就可以通过 `ssh -p 5202 myserver.com` 来 ssh 到你的 NAS。

如果想在 Linux 上把 `molehill` 作为后台服务运行，可以参考
[systemd 单元](./docs/configuration.zh.md#systemd) 或
[容器部署](./docs/configuration.zh.md#容器)。

## 配置

`molehill` 会根据配置文件自动判断运行模式（server/client），也可以通过 `--server` / `--client` 强制指定。完整的配置规范、日志和调优选项见
[配置文档](./docs/configuration.zh.md),其中也包含覆盖各种常见场景的
[完整示例](./docs/configuration.zh.md#完整示例)。

## 部署

从 [release 页面](https://github.com/NIyueeE/molehill/releases) 下载对应平台的预编译二进制，或者
[从源码编译](./docs/build-guide.md) 获取其他平台和最小化的二进制。

```bash
./molehill server.toml   # 在公网服务器上
./molehill client.toml   # 在 NAT 后的设备上
```

如何作为服务运行由[配置文档](./docs/configuration.zh.md)负责:它拥有
[systemd 单元](./docs/configuration.zh.md#systemd)(root 与 rootless,含多实例)和
[容器部署](./docs/configuration.zh.md#容器)——发布的 `ghcr.io/niyueee/molehill`
镜像(linux/amd64、linux/arm64;`scratch` 上的静态 musl 二进制)、它以哪个非
root UID 运行,以及唯一一条容器相关注意点:使用 `carrier = "kcp"` 的服务还需
要把数据面端口按 **UDP** 发布出去。

## 文档

给部署和使用 molehill 的人：

- [配置文档](./docs/configuration.zh.md) — 完整的配置规范、日志和调优
- [传输层](./docs/transport.zh.md) — Noise Protocol 配置
- [基准测试](./docs/benchmarks.zh.md) — 已发布数字是怎么测出来的、怎么读、怎么复现
- [构建指南](./docs/build-guide.md) — 构建定制、最小化二进制
- [内部原理](./docs/internals.md) — 控制通道和数据通道的工作原理
- [配置示例](./docs/configuration.zh.md#完整示例) — 常见场景的配置(含 systemd 与容器部署)

给改动这个仓库的人(贡献与治理类文档按决定只保留英文,见
[AGENTS.md](./AGENTS.md) §3):

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
git hooks 守护每次 commit、push 与发布 tag，CI 对**涉及代码**的改动运行同一条
链；只改文档的改动则只跑文档一致性检查（`docs.yml`）：

```bash
just setup   # 激活 git hooks（core.hooksPath githooks）并安装检查工具
just check   # fmt / secrets / machete / docs / ruff(check + format) / clippy + audit / deny / outdated / test
just tag     # 发布审查（githooks/pre-tag）+ 创建本地 v* tag
```

molehill 起源于 [rathole](https://github.com/rapiz1/rathole) 的 fork
（Apache-2.0），此后独立发展；上游历史完整保留在 fork 点之下，版本号也从
该点续计（上游最后一个版本是 v0.5.0）。发布机制见
[docs/release.md](./docs/release.md)，仓库规则见
[AGENTS.md](./AGENTS.md)。
