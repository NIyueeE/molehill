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
- **多路复用** 默认情况下，每个数据通道都作为 yamux 流跑在 N 条并行隧道中的一条上（`[client.data].default_count = 4`）——省去每条连接的握手、显著减少文件描述符，吞吐超越单条 TCP 流并隔离队头阻塞（丢段只停滞自己的隧道）。可选的 `default_carrier = "kcp"`（feature `kcp`）把数据面换成 KCP-over-UDP 会话。`[client.data]` 默认选项与 `mode = "direct"` 回退路径见[配置文档](./docs/configuration.zh.md)。
- **安全性** 共享 token 强制鉴权，`allow_ports` 白名单限制客户端可暴露的端口。可选的 Noise Protocol 只需一对预共享 X25519 密钥即可加密传输——没有 PKI、没有 CA；设置 `resume = true` 后，重连用一次 MAC 证明持有上次会话的握手摘要即可，不必重跑密钥交换(建连从每对 442.7 us 降到 38.5 us)。`plain` 为明文转发。
- **热重载** 支持配置文件热重载，动态添加或移除端口转发服务。

## 基准测试

单机对比(`访客 → 服务端 → 客户端 → 后端`,四跳都在同一台机器上);一切
**穿过隧道**测量——探针拨的是每个工具的暴露端口,绝不直接连它转发的后端。
对端工具为最新 GitHub release 构建(frp、rathole 上游、nps——版本按次运行
记录在结果 meta 中)。每个工具都被驱以**完全相同的工作负载**,同时网络条
件按脚本化的阶段表变化(netem 塑造整个 `lo`,每一跳都受影响,且原地切换,工
具会话从不重建);指标集合与测试类型见[方法论](#方法论)。以下是本主机的
v0.9.0 数字;只有同模型、同主机的运行之间才可互相比较。

### 如何选配置

下面的测量为默认值背书,并告诉你在何时偏离:

| 配置 | 何时使用 | 实测代价 |
|---|---|---|
| **`mode = "multiplex"`(默认)** | 一个客户端暴露**多个服务**,或连接高频开合(HTTP/游戏会话);连接资源重要(FD、端口、**NAT 映射**——NAT 后每条物理隧道占一个映射) | 回环单流 10.0 Gbit/s(`direct` 为 19.2——一条 yamux 流受限于单条隧道流),8 流 19.5;yamux 上限把并发连接钉在 `count × 64`(默认 `count = 4` 即 256——64 可用) |
| **`mode = "direct"`** | 单个服务或少数长连接(SSH);**原始吞吐优先**(大流量传输):回环 19.2/23.3 Gbit/s | 每条流一条物理隧道:FD/端口/NAT 映射随流数增长;每连接建连成本真实存在(churn p99 ~3.5 ms,16 路并发)但在 `pool_size = 8` 下不可见;足迹最小(约 15.5 MiB)、CPU 更低(单隧道 216% 单核) |
| **`count = 4`(默认)** | 并发流多,或链路有损:独立隧道隔离队头阻塞并**聚合超过单流** | 每服务 4 条物理连接(FD/端口/NAT 映射),CPU 约 494% 单核(单隧道 216%);回环 8 流 19.5 vs `count = 1` 的 9.2 Gbit/s,1% 丢包 12.3 vs 4.5,突发丢包 13.3 vs 4.5;10 ms 的 HoL 最大值更低(80.7 vs 100.1 ms) |
| **`count = 1`** | 单条长连接、连接预算紧张,或要最小足迹与更低的 CPU(约 16 MiB / 216%) | 单 TCP 流天花板;没有聚合(回环 8 流 9.2 Gbit/s);所有流共享一个重传域 |
| **`carrier = "kcp"`**(实验性) | TCP 数据隧道被封锁/限速时,或**高延迟下的延迟优先 UDP** | 只要路径不是瓶颈就远落后于 TCP 载体(回环 8 流 1.1 vs 14.9 Gbit/s、rtt10 0.79 vs 5.45、loss1 0.71 vs 7.74),RSS 约 2.5-3 倍(83 vs 26 MiB)、CPU 更低;最明确的优势是 rtt100 会话质量(最大包间隔 20 ms vs TCP 各 arm 的 100+) |
| **`[server.data] stripe_count = K`**(实验性) | 单条长连接不能被单条隧道流钉死:每个访客连接摊到 `K` 条数据通道上,其天花板与在途窗口成为各通道之和 | 每访客 `K×` 数据通道与任务,接收侧重排缓冲;单流 A/B 及其代价面记录在 HANDOFF.md"Stripe A/B (K=4)" |
| **noise** | 要加密且**内存与简单性优先**:预共享公钥、无 PKI | 单流约为明文的 58%、8 流约 76%(5.8/14.9 vs 10.0/19.5 Gbit/s),RTT 亚毫秒,RSS 多约 4 MiB;CPU 持平(470% vs 494% 单核) |

如何应用:全局默认在 `[client.data]`,每个服务可在自己的
`[client.services.<name>]` 上单独覆盖 `mode`/`count`/`carrier`——同一
客户端可以混跑 mux 交互服务与 `direct` 大流量服务,还能用 `remote_addr`
把个别服务指向不同的 molehill 服务端(服务端按连接自适应,无需改配置)。
`[transport]` 见[配置](docs/configuration.md);noise 密钥见
[传输](docs/transport.md);可运行的配置见[快速开始](#快速开始)与
[完整示例](./docs/configuration.zh.md#完整示例)。

**怎么选:分步走。** 从默认值(`multiplex`、`count = 4`、
`carrier = "tcp"`、明文)出发,回答三个关于你负载的问题;一次只改一项,
改完复测:

1. **需要加密吗?** 需要 → `[client.transport] type = "noise"` 并放置密钥
   (代价:单流约 -42%,5.8 vs 10.0 Gbit/s,8 流约 -24%;需求低于
   ~2 Gbit/s 时无感;RTT 亚毫秒、RSS 多约 4 MiB)。不需要 → 保持
   `"plain"`。
2. **单人还是多人?并发连接多少?** 单条长连接(SSH、单玩家 Minecraft)→
   `direct` 或默认 mux 都行;低并发下 mux 还省 NAT 映射。多人/高频开合/
   多服务 → 保持或加大 `count`(每条隧道约承载 64 条并发连接,yamux
   上限——`count = 8` ≈ 512)。若这一条流不能被单条隧道流钉死(单会话
   大流量),设置 `[server.data] stripe_count`(K=4)——连接随即跑在 K
   条并行数据通道上,代价是每访客 K× 通道与有界的重排缓冲。
3. **路径什么状况,是否转发 UDP?** 若 TCP 数据隧道被封锁/限速,或需要
   高延迟下的延迟优先 UDP,值得 A/B 试 `carrier = "kcp"`(rtt100 会话最大
   间隔 20 ms vs TCP 各 arm 的 100+)。否则保持 TCP 载体:UDP 阶梯与队头
   探针都没有显示默认配置存在可复现的"负载下 UDP 惩罚"(两轮里出现的
   100% pinger 丢包在第三轮回到 2%)。有损/wifi 路径 → 保持
   `count >= 4`:它能聚合(1% 丢包 8 流 12.3 vs 4.5 Gbit/s)并让 10 ms 的
   HoL 最大值更低;`count` 按"每隧道连接上限"选
   (`count = 1 → 64` 条连接,`count = 4 → 256`)。


**为自己的场景测一遍。** v0.9.0 模型对每个配置给两个数,而不是一个吞吐
数字:**可持续负载**(交互流仍满足 50 ms SLO 时,工具能扛多少条 bulk 流)和
**工作点成本**(每承载 1 Gbit/s 的 CPU 秒)。`just soak --test=screen --ab
<parent>,<head>` 可在分钟级对*你自己的*负载做 A/B,并打印该差异是 claim
还是 directional。退役矩阵在这里引用逐格平均值;它们已删除,因为“每格冷启
动的一个平均値”回答不了“路径变化时会发生什么”。

### molehill vs 明文 TCP 对端

每个工具跑同一份工作负载——1 条交互流(SLO 仪器)、N = 20 条 bulk TCP 流、
每秒 16 次短连接、1 条 UDP 会话——同时路径按阶段表推进(netem 塑造整个
`lo`,控制面保持不整形)。下图是本主机上的 v0.9.0 一次运行(molehill 默认
`multiplex`、`count = 4`、明文):橙色线是 bulk 吞吐,蓝色点是交互流 RTT,
阴影带是路径档,虚线是 SLO(p99 ≤ 50 ms)。

![Soak: molehill 与对端在阶段日程上的形态](assets/soak-v0.9.0.png)

**交互流 RTT p99,逐阶段**(ms;"wedge" = 该流静默超过 5 秒后恢复):

| 工具 | clean | rtt100 | loss1 | loss5 | rate100 | rate20 | jitter | clean(回归) |
|---|---|---|---|---|---|---|---|---|
| **molehill (mux)** | **7.6** | wedge | 1334 | 3494 | wedge | 3123 | 4354 | **4.9** |
| frp 0.71.0 | **2.9** | wedge | 3900 | wedge | 162 | 3919 | 5007 | **2.9** |
| rathole 0.5.0 | 81 | wedge | 1311 | wedge | wedge | 2600 | 4675 | 78 |
| nps 0.26.10 | 74 | 856 | 1139 | 4270 | wedge | 3265 | 1785 | 82 |

**Bulk 吞吐逐阶段**(Gbit/s):molehill clean 17.0 → rtt100 2.4 → rate100
0.02 → **回归 clean 20.1**;frp 5.9 → 2.2 → 5.9;rathole 16.9 → 2.5 → 17.0;
nps 全程 0.14。

**这些形状说明什么。** 每个工具在劣化档上都退化、在回归 clean 档上都恢复
——最后一段的意义就在恢复;一个保持 wedge 的工具就是一个发现(确有一个:
本 harness 的第一个版本把控制通道也整形了,rate100 档的心跳超时直接把工具
打楔;现在 harness 让控制面保持不整形,方法里已写明)。交互流的 p99 才是新
访客真正感受到的东西:饱和状态下它是区分工具的那个数,也正是吞吐轴的盲区
——molehill 与 rathole 在 clean 档上 bulk 几乎相同(17.0 对 16.9 Gbit/s),
而一次全新交互连接的代价是 7.6 ms 对 81 ms;在 1% 丢包档两者 bulk 都约 4.9
Gbit/s,交互流则是 1334 ms 对 1311 ms。对端由同一份工作负载驱动,画在同一批
面板里;漂移轴(全程的打开 fd、RSS 与 CPU 斜率)见
`soak-v0.9.0-drift.png`,UDP 会话的 RTT/丢包见 `soak-v0.9.0-udp.png`。

### 怎么读这些数(以及旧表格被什么取代)

v0.9.0 替换了测量模型:退役的矩阵测的是**格**(每工具每网络条件一个平均
值,每格冷启动),现在测的是**时间上的工作负载**。旧的逐格表格及其图表
(逐格吞吐、count/carrier/transport 各自为图)随之删除;v0.8.x 的发布说明
保留其历史数字,模型方法见[方法论](#方法论)与
[docs/release.md](docs/release.md)。退役模型的数字永远不是对本模型的回归
信号。

### 方法论

- **测量的单位是工作负载,不是格。** 每个工具都被驱以同一份客户端侧的工作
  负载——1 条交互流(每 ping 一次到 echo 服务的新 TCP 连接,即 SLO 仪器)、
  N 条 bulk TCP 流(iperf3)、每秒 C 次短连接(churn 连接器)、1 条 UDP 会
  话——同时路径按脚本化的阶段表推进。工具进程只启动一次,整形原地切换
  (每工具 class 一次 `tc qdisc change`),因此会话从不重建,**适应过程本身
  就是测量的一部分**。
- **阶段**:`clean`(150 s——冷启动加基线)、`rtt100`、`loss1`、`loss5`、
  `rate100`、`rate20`、`jitter`(各 120 s),然后再 `clean`(150 s——恢复
  轴)。顺序、时长、保护带与每个采样率都记入结果 meta,因为它们是方法参数
  (AGENTS.md §10)。
- **SLO 是方法常量**:交互流 RTT p99 = 50 ms 且零错误——它是每张图上的那
  根线,也是容量测试的中止条件。
- **测试类型**:`capacity`(ramp bulk 负载直到交互流破 SLO——可持续负载加
  完整响应时间曲线)、`rrul`(N = CPU 核数,看交互流 RTT 分布**随时间**——
  饱和排队检测器)、`soak`(长时间轮换路径:漂移/泄漏轴)、`cost`(固定工作
  点上每承载 Gbit 的 CPU 秒)、`screen`(快速开发 A/B,在每个负载档内交替
  两个构建)。
- **隔离**:每个工具拥有自己的端口带;并发运行时还各有一个 HTB class 与独立
  netem——同一批里的两个工具绝不共享速率桶、丢包过程或队列。批大小来自
  主机 CPU 预算(核数 ÷ 每对 CPU,记入 meta)。交互与 UDP 探针跑在各自独立
  的进程里,harness 永远不在被测路径上。
- **一切外部可测**:吞吐与重传来自 iperf3 的每区间流,RTT/丢包/抖动来自探
  针,RSS/CPU/打开 fd/线程数来自 `/proc`。这正是对端能用同一份工作负载被
  测量的原因——也正是 molehill 自己的内部计数器(mux/KCP)被排除在比较之
  外的原因。
- **派生数字**:每阶段每条序列的 p50/p99/max、最差 1 秒窗口(稳定性轴)、
  漂移斜率(泄漏是斜率不是水位)与压平段——交互流静默超过 5 s 记为一次
  wedge 及其时长,绝不是一个裸 null。
- **并行是被验证的,不是被假设的**:同一工具单独跑一次、再放进满批里跑一
  次;若逐阶段数字在 claim 规则之外不一致,那么本主机的批大小就是实测出
  来的那个数,并记入 meta。
- **纪律**:热身保护带不计入派生统计,每次失败都留下其类型化原因,任何判据
  工具都不会在分布之外单独发布一个平均值。退役的矩阵(v0.8.x 及更早)测的
  是冷启动格与 rep 中位数——那是另一套仪器;它的数字留在 git 历史与发布
  说明里,永远不是对本模型的回归信号。
- **复现**:`just soak-peers` → `just soak` → `just soak-plot` →
  `just soak-check`(原始数据在
  `benches/scripts/soak/results-soak-v0.9.0.json`;仪式与门见
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

### 二进制

从 [release 页面](https://github.com/NIyueeE/molehill/releases) 下载对应平台的预编译二进制，或者
[从源码编译](./docs/build-guide.md) 获取其他平台和最小化的二进制。

```bash
./molehill server.toml   # 在公网服务器上
./molehill client.toml   # 在 NAT 后的设备上
```

### systemd

[systemd 单元](./docs/configuration.zh.md#systemd) 演示了如何把 molehill 作为 systemd 服务运行，包含 root 和 rootless 两种方式，以及多实例管理。

### 容器

官方多架构镜像（linux/amd64、linux/arm64）发布在
`ghcr.io/niyueee/molehill`。镜像是构建在 `scratch` 上的单个静态 musl 二进制（约 1.2 MiB），以非 root UID 1000 运行，并且与常规发布构建使用相同的默认特性集（包含多路复用与 `kcp` 载体）。

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

镜像内不包含任何配置——挂载你的配置文件，并把文件名作为参数传入。两个容器相关的注意点：进程以 UID 1000 运行（因此配置文件要对其他用户可读，端口尽量用 ≥ 1024），以及在 bridge 网络下，使用 `carrier = "kcp"` 的服务还需要把数据面端口按 **UDP** 发布出去。更多部署方式见
[容器部署](./docs/configuration.zh.md#容器)，包括 Docker Compose（`compose.yaml` / `compose.bridge.yaml`）和 Podman
Quadlet（`molehill-server.container` / `molehill-client.container`）。

## 文档

使用 molehill：

- [配置文档](./docs/configuration.zh.md) — 完整的配置规范、日志和调优
- [传输层](./docs/transport.zh.md) — Noise Protocol 配置
- [构建指南](./docs/build-guide.md) — 构建定制、最小化二进制
- [内部原理](./docs/internals.md) — 控制通道和数据通道的工作原理
- [配置示例](./docs/configuration.zh.md#完整示例) — 常见场景的配置(含 systemd 与容器部署)

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
git hooks 守护每次 commit、push 与发布 tag，CI 对**涉及代码**的改动运行同一条
链；只改文档的改动则只跑文档一致性检查（`docs.yml`）：

```bash
just setup   # 激活 git hooks（core.hooksPath githooks）并安装检查工具
just check   # fmt / secrets / machete / docs / clippy + audit / deny / outdated / test
just tag     # 发布审查（githooks/pre-tag）+ 创建本地 v* tag
```

molehill 起源于 [rathole](https://github.com/rapiz1/rathole) 的 fork
（Apache-2.0），此后独立发展；上游历史完整保留在 fork 点之下，版本号也从
该点续计（上游最后一个版本是 v0.5.0）。发布机制见
[docs/release.md](./docs/release.md)，仓库规则见
[AGENTS.md](./AGENTS.md)。
