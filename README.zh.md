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

<p align="center">面向 NAT 穿透的安全、稳定、高性能反向代理 —— 现在支持透明（L3）客户端：NAT 后的设备拥有自己的公网 IP。</p>

[English](README.md) | [简体中文](README.zh.md)

molehill，类似于 [frp](https://github.com/fatedier/frp) 和 [ngrok](https://github.com/inconshreveable/ngrok)，可以将 NAT 后的设备上的服务通过具有公网 IP 的服务器暴露到互联网。

**两种暴露服务的方式**，由客户端选择：

- **转发（L4）** 服务端绑定公网端口，把连接中继进隧道。这是最常见的形态，也是这个领域所有工具的做法。
- **透明（L3）** 客户端**自己拥有那个公网 `ip:port`**。服务端把整个 IP 包路由进隧道，由**客户端自己的内核**应答访客，因此后端看到的是访客的真实源地址，服务端完全不持有该连接的连接状态。当真实源地址重要时——邮件服务器、游戏服务器、任何按对端记录日志或限流的服务——这就是你要的模式，也是"前面加了一层反向代理"不再是一种妥协的原因。仅支持 Linux。

<!-- TOC -->

- [molehill](#molehill)
  - [特性](#特性)
  - [基准测试](#基准测试)
    - [v0.11.0 一次运行](#v0110-一次运行)
  - [快速开始](#快速开始)
  - [配置](#配置)
  - [部署](#部署)
  - [文档](#文档)
  - [开发](#开发)

<!-- /TOC -->

## 特性

- **透明（L3）客户端：NAT 后的设备拥有自己的公网 IP（仅 Linux）** `[transparent]` 表是一种独立的运行模式：服务端不绑定任何端口，把整个 IP 包路由进隧道，由**客户端自己的内核**应答访客——因此后端看到的是访客的真实源地址，TCP 保持端到端语义，服务端完全不持有该连接的 socket 与按流状态。全程不做地址转换。它下沉到转发路径所用的同一套引擎，因此协议与数据面是共用的。守护进程自己不配置任何网络：它连接你创建的 TUN 设备，并在前置条件缺失时拒绝启动、给出要执行的确切命令，而不是以静默丢包的方式失败。是否提供 L3 是**服务端运维方**的决定（`[server.transparent]`），没有它，认领会在查看任何设备之前被策略拒绝。两套配方与每种模式所需权限见[配置文档](./docs/configuration.zh.md#透明l3服务)与[部署文档](./docs/deployment.zh.md#透明服务)。
- **高性能** 具有更高的吞吐量，高并发下更稳定。
- **低资源消耗** 内存占用远低于同类工具。[二进制文件最小](docs/build-guide.md)可以到 **~500KiB**，可以部署在嵌入式设备如路由器上。
- **客户端声明服务** 从 v0.7 起，服务端不再需要逐服务配置：客户端声明要暴露的内容（包括公网端口），服务端只通过 `allow_ports` 白名单和共享 `default_token` 执行策略。
- **多路复用** 转发服务的每个数据通道都作为 yamux 流跑在一个固定隧道池（`[client.data].tunnels`，默认 4）的某条隧道上——省去每条连接的握手、显著减少文件描述符，吞吐超越单条 TCP 流并隔离队头阻塞（丢段只停滞自己的隧道）。池在服务启动时建立、永不按负载调整大小（死亡的隧道会被修复）；数据面本身跑在 KCP 上（feature `kcp`），KCP 会话建立不起来时回退到 TCP——想分开测两条路径时用 `MOLEHILL_DATA_CARRIER` 强制其一。透明（L3）认领是唯一不做多路复用的形态——它的通道**就是**它的载体连接,由车道预算分配,并随各认领流量的起落互相出借(`[transparent.data].lend`);`[client.data]` 选项见[配置文档](./docs/configuration.zh.md)。
- **安全性** 共享 token 强制鉴权，`allow_ports` 白名单限制客户端可暴露的端口。控制通道**始终**使用 Noise：它携带的是每个数据通道用来鉴权的凭证，明文控制腿会把该凭证广播给路径上的所有人，因此两端没有密钥对就拒绝启动（`molehill --genkey`）。只需一对预共享 X25519 密钥，没有 PKI、没有 CA；设置 `resume = true` 后，重连用一次 MAC 证明持有上次会话的握手摘要即可，不必重跑密钥交换(建连从每对 442.7 us 降到 38.5 us)。单个*服务*或一条 L3 车道在载荷自带加密时仍可走明文。
- **可选的状态 API** 一个带 token 的 `GET /status`，跑在自己的监听地址上，以 JSON 回答"这个客户端正在承载什么"，供面板或脚本读取。见[配置文档](./docs/configuration.zh.md)。
- **热重载** 支持配置文件热重载，动态添加或移除端口转发服务。

## 基准测试

单机对比(`访客 → 服务端 → 客户端 → 后端`,四跳都在同一台机器上),一切
**穿过隧道**测量:探针拨的是每个工具的暴露端口,绝不直接连它转发的后端。
方法、读图与复现:[基准测试方法](docs/benchmarks.zh.md);设置背后的决策树,
以及值得在你自己路径上测的两个数:
[配置文档](docs/configuration.zh.md#选择配置决策树)。

### v0.11.0 一次运行

每个工具都被驱以完全相同的工作负载,同时路径按阶段表推进、原地切换,会话
从不重建。下图是本主机上的 v0.11.0 一轮 sweep,用发布二进制的默认值——4 条隧道
的多路复用池,并且**控制通道的 Noise 密钥也作用在数据面上**,因为这正是本版本
「控制通道必须 Noise」这条规则对不按服务覆盖的客户端的含义。图例与阶段表见
[基准测试](./docs/benchmarks.zh.md#如何读图)。

![molehill 与对端在阶段日程上的形态](assets/bench-v0.11.0-timeline.png)

同一轮运行的小倍数视图——每个阶段一个面板,每个工具一根棒棒糖线:

![逐阶段、逐工具的交互流 RTT](assets/bench-v0.11.0-stages.png)

**逐阶段的交互流 RTT p99**(毫秒)。`clean` 之后的每个阶段都是被整形的——
测试台装上了主导该阶段的队列或延迟——所以这些列不标注赢家。样本数与完整读法见
[基准测试](./docs/benchmarks.zh.md#如何读一个格子)。

| 工具 | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean(重复) |
|---|---|---|---|---|---|---|---|---|
| molehill (L4) | 4.2 | 402.2 | 1071.6 | 2451.4 | 199.0 | 434.2 | 281.6 | 3.9 |
| molehill (L3) | 1.3 | 401.3 | 1064.2 | 2067.3 | 197.4 | 432.5 | 280.6 | 1.3 |
| frp | 1.2 | 401.4 | 1069.9 | 1452.9 | 196.0 | 424.6 | 277.2 | 1.4 |
| rathole | 1.5 | 401.1 | 1070.5 | 3137.8 | 196.9 | 430.3 | 281.3 | 1.4 |
| nps | 16.8 | 421.6 | 1076.1 | 1449.8 | 197.6 | 434.5 | 267.3 | 66.7 |

**逐阶段的批量吞吐**(Gbit/s,取自该阶段整个测量窗口,而非最好的那一秒)。

| 工具 | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean(重复) |
|---|---|---|---|---|---|---|---|---|
| molehill (L4) | 9.302 | 3.397 | 9.111 | 0.814 | 0.094 | 0.019 | 0.019 | 9.382 |
| molehill (L3) | 6.014 | 3.159 | 4.774 | 0.944 | 0.094 | 0.019 | 0.019 | 5.981 |
| frp | 5.987 | 3.382 | 6.086 | 1.141 | 0.094 | 0.019 | 0.019 | 5.968 |
| rathole | 28.587 | 3.389 | 14.778 | 1.176 | 0.094 | 0.019 | 0.019 | 28.338 |
| nps | 0.134 | 0.136 | 0.134 | 0.135 | 0.094 | 0.019 | 0.019 | 0.134 |

**这一轮自己的重复测量。** `clean` 在每条时间线的两端都测一次,所以每个工具
的两个读数是相隔约一小时的同一种条件的两个样本——这是读其他格子时的标尺:

| 工具 | clean 批量读数 | clean 交互 p99 |
|---|---|---|
| molehill (L4) | 9.302 – 9.382 Gbit/s(**相差 0.9 %**) | 4.2 – 3.9 ms |
| molehill (L3) | 6.014 – 5.981 Gbit/s(**相差 0.6 %**) | 1.3 – 1.3 ms |
| frp | 5.987 – 5.968 Gbit/s(**相差 0.3 %**) | 1.2 – 1.4 ms |
| rathole | 28.587 – 28.338 Gbit/s(**相差 0.9 %**) | 1.5 – 1.4 ms |
| nps | 0.134 – 0.134 Gbit/s(**相差 0.0 %**) | 16.8 – 66.7 ms |

**它能承载多少。** 同一份产物里还带着负载爬坡——一条新的交互连接在哪个批量
负载档位上首次打破 SLO——这是与阶段日程不同的另一件仪器
([基准测试](./docs/benchmarks.zh.md#场景));每个臂都跑满了它的 8 条流,所以 8
读作**下限**("至少 8"),而不是上限。

**两行其实是同一个工具。** `molehill (L3)` 是本项目的第二种架构,不是第五个
工具:frp、rathole 与 nps 都是没有 TUN 路径的转发代理,所以 L3 那一行的对照是
`molehill (L4)` 与无工具的 control 臂——同一工作负载、同一拓扑,只差一层架构。
诚实的读法是:**两种架构现在各分得一类工作负载**——clean 阶段的批量,L3 读
6.01 对 L4 的 9.30 Gbit/s;交互 p99 明显更低(1.3 对 4.2 ms)、负载下也更稳:
容量斜坡的每一档都撑住。批量与延迟反向而行的根源,是 v0.11.0 把批量差距从
根上收窄了:设备现在吃下了自己的分段 offload(GSO 超级帧,而不是每次读一个
包),单流认领在 smoke A/B 里从 2.0 到 3.4 Gbit/s,staged 运行的 L3 clean
格子反映的正是同一个修复。认领的车道是它自己的载体连接、不是多路复用池里的
流,而它**默认持有两条**
([配置页](./docs/configuration.zh.md#透明l3服务)把这份交易按连接数标了价)。

**那个曾经是这一行头条的 wedge 已经不在了,而且它从来不是工具的问题。** L3 曾在
这条日程上**静默 6 次**而 L4 一次都没有;两个成因都在基准 harness 里——一条整形腿
沿用了 netem 隐含的 1000 包队列(它把这个架构的回程流大部分丢掉,而所有设备计数器
都读零),以及一个把"慢应答"读成"静默"的 wedge 指标。现在 `just bench-gate` 通过,
**每个臂的 wedge 都是 0**。唯一幸存下来的、L3 特有的代价就在上面那个整形 `rtt100`
列里,而记录保留着它的证据而不是把它抹平:同一阶段里整形器**为 L3 丢掉 310 万个包、
为 L4 丢掉 0 个**,因为 L3 每秒提供的回程包是好几倍、而承载的数据不到一半——整形档
的交互数字正是对这件事的读数
([基准测试](./docs/benchmarks.zh.md#怎么读一个格子))。

**把 L4 那一行和 v0.10.0 的那一行对比,差别在数据面,不在工具。** v0.10.0 在
clean 阶段发布的是 16.83 Gbit/s,数据面是明文;这一轮读到 9.30,因为每个服务都
继承了控制通道的 Noise 密钥——而这个选择的价目表是量过的
([基准测试](./docs/benchmarks.zh.md#每个配置选择的代价按决策的测量)):单流
10.0 → 5.8 Gbit/s、八流 19.5 → 14.9,约 42 %,与这里看到的约 46 % 一致。想要
旧数字的部署可以给服务设 `transport = { type = "plain" }`,同时保留加密的控制
通道。**rathole 的读数高于本机的回环标定**(27.9 对约 21.8 Gbit/s 的回环探针),
这正是本页一直在标注的天花板假象:快的几个工具中有两个就贴着或超过回环测量值,
并随主机状态在不同轮次间移动,因此它们的先后次序在 sweep 之间并不稳定;而
`frp` 与 `nps` 比天花板低一个数量级,重复误差在 1 % 以内。

以上是本主机上的 v0.11.0 数字,只有同模型、同方法、同主机的运行之间才可直接
比较:每个结果文件都记录了主机、方法与两个无工具的标定,每轮运行都由它自己的
完整性、端点与 SLO 检查把关
([基准测试](./docs/benchmarks.zh.md#可比性))。这是第一个由 bench 模型测量的
版本,因此没有可比的基线——v0.10.0 的记录出自被它取代的模型,而 gate 拒绝跨
两者比较。

这一轮其余的图与上面两张一起发布:`bench-v0.11.0-capacity.png`(负载爬坡)、
`bench-v0.11.0-drift.png`(整轮运行中的 RSS、fd 与线程)和
`bench-v0.11.0-cost.png`(每承载 1 Gbit 的 CPU 秒数)。

## 快速开始

一个全功能的 `molehill` 可以从 [release](https://github.com/NIyueeE/molehill/releases) 页面下载。或者 [从源码编译](docs/build-guide.md) **获取其他平台和最小化的二进制文件**。

`molehill` 的使用和 frp 非常类似：转发服务由客户端声明，服务端只配置共享 token 和端口策略。

使用 molehill 需要一个有公网 IP 的服务器，和一个在 NAT 或防火墙后的设备，其中有些服务需要暴露在互联网上。

假设你在家里的 NAT 后面有一个 NAS，并且想把它的 ssh 服务暴露在公网上：

1. 在有一个公网 IP 的服务器上

创建 `server.toml`，内容如下，并根据你的需要调整。控制通道始终加密——它携带的是每个数据通道用来鉴权的凭证——所以两个文件都要带一对 Noise
密钥。用 `molehill --genkey` 生成一对，**私钥**留在服务端，**公钥**给客户端。

```toml
# server.toml
[server]
default_token = "use_a_secret_that_only_you_know" # 与客户端共享的密钥 # security-scan:allow documentation placeholder

# 动态注册的总开关：只有这里覆盖的端口才能被客户端占用
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333" # molehill 监听客户端连接的端口

# 必需：控制通道始终使用 Noise
[server.transport.noise]
local_private_key = "2wOFku4PdLhcWQyqUXBWRrKhQV72S/9rkN9jktqiUS8=" # security-scan:allow documentation example key
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

[client.transport]
type = "noise"

# 服务端的公钥，来自 `molehill --genkey`
[client.transport.noise]
remote_public_key = "9PE8H+fhLSgk9xFyqEwoKuQtq3n2sdGYzgY8jBtctlw=" # security-scan:allow documentation example key

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
[systemd 单元](./docs/deployment.zh.md#systemd) 或
[容器部署](./docs/deployment.zh.md#容器)。

## 配置

`molehill` 会根据配置文件自动判断运行模式（server/client），也可以通过 `--server` / `--client` 强制指定。完整的配置规范、日志和调优选项见
[配置文档](./docs/configuration.zh.md),其中也包含覆盖各种常见场景的
[完整示例](./docs/deployment.zh.md#完整示例)。

## 部署

同一个二进制跑在两端；模式由配置文件决定：

```bash
./molehill server.toml   # 在公网服务器上
./molehill client.toml   # 在 NAT 后的设备上
```

可直接使用的配置示例、systemd 单元与容器配方见[部署文档](./docs/deployment.zh.md)，
其中也包含网络要求与部署安全说明。

## 文档

给部署和使用 molehill 的人：

- [配置文档](./docs/configuration.zh.md) — 完整的配置规范、日志和调优
- [部署文档](./docs/deployment.zh.md) — 可直接使用的配置、systemd 单元与容器配方
- [传输层](./docs/transport.zh.md) — Noise Protocol 配置
- [基准测试](./docs/benchmarks.zh.md) — 已发布数字是怎么测出来的、怎么读、怎么复现

给改动这个仓库的人(贡献与治理类文档按决定只保留英文,见
[AGENTS.md](./AGENTS.md) §3):

- [检查门](./docs/checks.md) — 每个门运行什么、被拦住时怎么办
- [Lint 策略](./docs/lint-policy.md) — lint 级别与豁免规则
- [发布流程](./docs/release.md) — 发布机制、版本编号、测试构建
- [仓库结构](./docs/structure.md) — 仓库里每个文件的用途
- [构建指南](./docs/build-guide.md) — 构建定制、最小化二进制
- [内部原理](./docs/internals.md) — 控制通道和数据通道的工作原理
- [贡献指南](./CONTRIBUTING.md) — 环境搭建与工作流
- [安全策略](./SECURITY.md) — 漏洞报告
- [`HANDOFF.md`](./HANDOFF.md) — 当前工作状态；计划中的工作和将来的设计文档

## 开发

molehill 使用 Rust 编写（2024 edition）；`rust-toolchain.toml` 声明
`channel = "stable"` 并附带 clippy 与 rustfmt 组件 —— 不要硬编码版本号。
每个门运行什么、被拦住时怎么办，见[检查门](./docs/checks.md)。

```bash
just setup   # 激活 git hooks（core.hooksPath githooks）并安装检查工具
just check   # fmt / secrets / machete / docs / ruff(check + format) / clippy + audit / deny / outdated / test
just tag     # 发布审查（githooks/pre-tag）+ 创建本地 v* tag
```

molehill 是一个独立项目，最初从 [rathole](https://github.com/rapiz1/rathole) fork
而来；每个版本的变化见 [CHANGELOG.md](./CHANGELOG.md)，仓库规则见
[AGENTS.md](./AGENTS.md)。
