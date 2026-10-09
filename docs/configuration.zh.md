# 配置

`molehill` 可以根据配置文件的内容自动判断以服务端还是客户端模式运行:如果
`[server]` 与 `[client]` 块只出现一个,就自动选择对应模式,如
[快速开始](../README.zh.md#快速开始)中的示例。

`[client]` 与 `[server]` 块也可以放在同一个文件里:此时在服务端运行
`molehill --server config.toml`,在客户端运行 `molehill --client config.toml`,
显式指定运行模式。

开箱即用的配置、systemd 单元与容器部署见[部署与示例](./deployment.zh.md)。

加密与 `transport` 块的更多细节见[传输层文档](./transport.md)。

页面索引:

- [如何配置(v0.7+ 模型)](#如何配置v07-模型)
- [选择配置(决策树)](#选择配置决策树)
- [动态服务注册](#动态服务注册)
- [多路复用(`multiplex` 特性)](#多路复用multiplex-特性)
- [透明(L3)服务](#透明l3服务)
- [日志](#日志)
- [调优](#调优)
- [示例与部署](#示例与部署)
- [使用说明](#使用说明)
- [故障排查](#故障排查)

## 如何配置(v0.7+ 模型)

自 v0.7 起,**服务定义归客户端所有**,服务端只拥有策略:

- 客户端在自己的配置里声明每个要转发的服务——包括它应该暴露的公网地址
  (`remote_bind_addr`)。
- 服务端**没有**任何按服务的配置。客户端连接时在运行时注册服务;服务端
  在暴露任何端口前,都会用 `allow_ports` 白名单校验每次注册。
- 两端用一个共享密钥(`default_token`)鉴权。

典型配置步骤:

1. 选择传输层——`plain` 或 `noise`——`noise` 需要生成一对密钥(见
   [传输层文档](./transport.md))。
2. 编写 `server.toml`:`[server]` 只需 `default_token` 和 `allow_ports`
   白名单,监听地址是 `[server.control].bind_addr`,仅此而已。
3. 编写 `client.toml`:`[client]` 配置相同的 `default_token`,服务端地址是
   `[client.control].default_remote_addr`,每个服务一个
   `[client.services.<name>]`
   块:`local_addr`(你的服务监听的地址)和 `remote_bind_addr`(公网端点)。
4. 先启动服务端,再启动客户端。两端都会持续运行;服务端不可达时客户端
   会自动重试。

> **从 ≤0.6 迁移**:删除整个 `[server.services.*]` 段;把每个服务的
> `bind_addr` 移入客户端的 `remote_bind_addr`;用 `default_token` 替换
> 按服务的 token;在服务端添加 `allow_ports`。两端必须一起升级
> (协议版本已变更)。

> **从 0.7.x 迁移**:客户端侧的键挪进了专用块,数据面也有了显式
> 端点。旧 → 新:
> `[client].remote_addr` → `[client.control].default_remote_addr`;
> `[client].heartbeat_timeout` / `retry_interval` →
> `[client.control].default_*`;
> `[client].mux = false` → `[client.data].default_mode = "direct"`;
> `[client].mux = true` → `[client.data].default_mode = "multiplex"`(
> `mux_receive_window` / `mux_max_streams` 两个键已移除——改用内部固定
> 的 yamux 默认值);
> `[client.transport].type = "tcp"` → `"plain"`;
> `[client.transport.tcp].proxy` → `[client.transport].proxy`;
> `[client.transport.tcp].nodelay` / `keepalive_secs` / `keepalive_interval`
> 已移除(改用内部固定默认值;按服务的 `nodelay` 保留);
> `tls` 与 `websocket` 两个传输取值在 0.8 中已移除:只剩 `"plain"` 与
> `"noise"`,`[client.transport.tls]` / `[client.transport.websocket]` /
> `[server.transport.tls]` / `[server.transport.websocket]` 块也已删除
> (迁移到 `noise`);
> `[server].bind_addr` → `[server.control].bind_addr`(独立的数据面监听
> 可选,用 `[server.data].bind_addr`,默认复用控制监听);
> `[server].heartbeat_interval` → `[server.control].heartbeat_interval`。
> 0.8 客户端侧的默认块统一用 `default_` 前缀命名——`[client.control]`
> (`default_remote_addr`、`default_heartbeat_timeout`、
> `default_retry_interval`)与 `[client.data]`(`default_data_addr`、
> `default_mode`、`default_carrier`)——以便与
> `[client.services.<name>]` 上的按服务覆盖键(`protocol`、`remote_addr`、
> `token`、`retry_interval`、`mode`、`carrier`、`transport`、
> `udp_workers`、`udp_forwarder_ipv6`、`udp_send_queue_size` 等;
> 0.8 新增)清晰区分。`[client.transport]` 的 `type`/`noise` 保持无前缀:
> 它的按服务覆盖在嵌套的 `transport` 表里,服务层不存在同名冲突
> (`default_` 前缀正是为了消解同名冲突而存在)。
> 旧键会被拒绝(`deny_unknown_fields`),绝不会被静默忽略。
>
> **0.8 协议**:每条连接以 1 字节传输选择器开头(`0x00` 明文 /
> `0x01` noise),注册消息携带数据面 carrier——两端必须一起升级;版本
> 不匹配是硬错误。
>
> **升级到 0.10(协议 v4)**:客户端改说 v4——每个端点一条控制会话,承载拨向该
> 端点的所有服务,并由服务端在开通通道之前先把 stripe 组命名出来;而 0.10.0 是
> 第一个**只服务 v4** 的版本:v3 客户端的连接会在它发生的那条连接上被拒绝,且
> 不会有任何回包。因此请两端一起升级;先后顺序无所谓,因为每一端都会拒绝对方的
> 方言而不是继续通信,而被拒绝的连接会写明它期待的版本。

### 迁移到 0.10:已移除的键

隧道池现在是每个会话、每个 carrier 一个弹性池,并且**冷启动**——因此那些描述
「池的初始大小」「按服务的池」或 0.8 后期健康检查的键都已移除。仍带着这些键的
配置不会启动:拒绝信息会列出它找到的每一个键以及该改写成什么(只写「未知字段」
能告诉你有东西不对,却不能告诉你该写什么)。请改写为:

| 已移除的键 | 改写成 |
|---|---|
| `[client.data].default_count` | 无需填写:池冷启动、按需增长。`[client.data.tcp].max_tunnels`(或 `[client.data.kcp].max_tunnels`)是它可增长到的上限,默认 4 |
| `[client.services.<name>].count` | 无需填写:同样是冷启动,而且池属于会话与 carrier,不再属于单个服务。`[client.data.tcp\|kcp].max_tunnels` 是上限 |
| `[client.services.<name>].pool_size` | UDP 服务写 `[client.services.<name>].udp_workers`(默认 2)。TCP 服务按访客即时打开数据通道 |
| `[client.services.<name>].heartbeat_timeout` | 无需填写:服务端在会话确认里声明自己的心跳节奏,客户端据此推导超时。`[client.control].default_heartbeat_timeout` 仍作为可选下限保留 |
| `[server].max_pool_size` | `[server.data].max_tunnels_per_client`(一个客户端可持有的隧道数;0 = 不限) |
| `[client.services.<name>].health_check` | 无需填写:只要客户端在运行,服务就保持注册;无法转发的请求只对那个访客失败 |

下一节说明每个替代键做什么、代价是什么;[CHANGELOG.md](../CHANGELOG.md) 记录
这些移除的原因。

## 选择配置(决策树)

默认配置——`mode = "multiplex"`、`max_tunnels = 4`、`carrier = "tcp"`、
明文传输——对绝大多数人是正确的起点。只有树上有明确分支时才偏离;每次只改一项,
并在**你自己的路径上**测量结果:已发布的运行、它们的数字以及如何复现,见
[基准测试](benchmarks.zh.md)。本页负责的是**每个设置做了什么**:

```mermaid
flowchart TD
    A["起点:默认配置<br/>multiplex、max_tunnels=4、<br/>carrier=tcp、明文"] --> B{"流量经过不可信网络?"}
    B -- 是 --> C["transport type = noise<br/>+ 密钥(见传输层文档)"]
    B -- 否 --> D{"单个服务或少数<br/>长连接?"}
    C --> D
    D -- "是,且原始吞吐优先" --> E["mode = direct"]
    D -- "否:多服务、多用户、<br/>高连接频率" --> F{"并发连接很多?"}
    E --> Z["完成——按需用<br/>[client.services.*] 覆盖"]
    F -- "> ~256 并发" --> G["max_tunnels = 8 或更高"]
    F -- 一般 --> H["保持 max_tunnels = 4"]
    G --> I{"路径质量?"}
    H --> I
    I -- "高纯延迟 + UDP 游戏<br/>(100ms+ RTT)" --> J["A/B 测试 carrier = kcp"]
    I -- 其他 --> Z
    J --> Z
```

### 每个选择的花费(你交换的是什么)

| 决策 | 选项 | 你放弃 / 得到什么 |
|---|---|---|
| `mode` | `"multiplex"`(默认) | 每个 FD、每个 NAT 映射承载最多连接;一条慢流会和同隧道其它流共享隧道 |
| `mode` | `"direct"` | 每条流一条物理连接:原始单流吞吐,代价是每条流一个 FD / 端口 / NAT 映射 |
| `max_tunnels` | `1` | 所有流量共用一条隧道:没有跨流聚合,且共享同一重传域,一次丢包会一起卡住 |
| `max_tunnels` | `4`(默认) | 聚合越过单流,并在隧道之间隔离队头阻塞;`4 × 64` 并发连接 |
| `max_tunnels` | `8+` | 更多并行隧道(更多 NAT 映射)与按比例更高的连接上限 |
| `carrier` | `"tcp"`(默认) | 有损与限速路径上表现良好的默认值;前提是网络不封锁 TCP 隧道 |
| `carrier` | `"kcp"` | TCP 隧道被封锁/限速时的延迟优先 UDP 传输;它不做多路复用,因此需要配合 `noise` + 调高 `max_tunnels` 来拿连接上限 |
| transport | `"plain"` | 不加密;每字节开销最低 |
| transport | `"noise"` | 用单个预共享密钥对加密线路;RTT 代价可忽略,满载无 CPU 惩罚 |
| 冷启动池 | (没有对应的键) | 池冷启动:空闲期后的第一个访客要先付一次隧道建连才开始过字节,之后池就热了,并可按需长到 `max_tunnels` |
| `udp_workers` | 2(默认) | 仅 UDP:该服务的 worker 集合使用多少条数据通道。不同访客分片到这些通道上;单个访客绝不被拆到多条通道(会话亲和)。它是扇出,不是容量旋钮:不会提高服务的报文上限,该上限的实测见[基准测试](benchmarks.zh.md#udp-队列问题不属于-soak-模型) |

各选项的**实测代价**——这些取舍所依据的数字及其来源——见
[基准测试](benchmarks.zh.md#每个配置选择的代价逐项实测)。

**验证选择**用你关心的口径:延迟用 `ping` / 游戏手感,原始吞吐用暴露端口的
`iperf3`,真实流量看暴露服务的行为。要在自己的硬件上比较两套配置或两个构建,
命令见[基准测试](benchmarks.zh.md#自己复现)。

以下是完整的配置规范:

```toml
[client]
default_token = "change-me" # 必填。必须与 `[server].default_token` 一致

[client.control] # 必填。控制通道默认值:鉴权、注册、心跳
default_remote_addr = "example.com:2333" # 必填。服务端地址
# default_heartbeat_timeout = 65 # 可选。应用层心跳超时。不设置(默认)时由服务端在会话确认里声明的节奏推导:`max(10 秒, 2 × server.control.heartbeat_interval + 5 秒)`。低于该下限的取值会在启动时被拒绝(否则会把健康的服务端判死);设为 0 禁用检测
default_retry_interval = 1 # 可选。重连退避的上限,而非固定间隔:延迟从 1 秒开始、按 3 倍增长并带抖动,最高不超过该值(抖动会让单次睡眠最长达到该上限的两倍),共 3 次重试;退避耗尽后客户端回落到固定 1 秒的重试循环。默认:1 秒

[client.data] # 可选。所有服务的数据面默认值(特性 `multiplex`,默认构建的一部分)。每个服务都可以单独覆盖 default_mode/default_carrier——见下方 `[client.services.*]` 里的按服务键
# default_data_addr = "example.com:2343" # 可选。数据面端点;默认为服务的控制端点(设置了 `client.services.<name>.remote_addr` 时用该地址,否则用 `client.control.default_remote_addr`)。`default_carrier = "kcp"` 时 KCP 会话用 UDP 拨控制地址——TCP 控制与 UDP KCP 可以共用一个端口(协议不同互不冲突)
default_mode = "multiplex" # 可选。默认数据面模式:"multiplex"(默认)或 "direct"(每条数据通道一条连接;`carrier` 不适用)
default_carrier = "tcp" # 可选。默认数据载体:"tcp"(默认)复用控制通道的传输栈;"kcp" 使用 KCP-over-UDP 会话(特性 `kcp`;服务端在第一条 `kcp` 注册到达时才打开 KCP 监听,无需服务端配置)。两种传输都可与 KCP 组合:transport 为 `noise` 时同样的 Noise 握手包裹每个 KCP 会话,`plain` 时会话保持明文
# shared_pool = false # 可选。把一条控制会话的所有服务放进每个 carrier 一个共享隧道池(true),而不是每个服务一个池(false,默认)。两者是同一套代码路径,只有池的 key 不同
# idle_timeout = 60 # 可选。池在没有 stream、没有待打开、也没有被钉住的 UDP peer 的情况下要空闲多少秒才移除一条隧道。默认:60。池永远不会缩到少于一条隧道,也不会低于 UDP 推导出的下限
[client.data.tcp] # 可选。TCP carrier 的弹性池上限
# max_tunnels = 4 # 可选。该 carrier 的池可增长到的上限;池冷启动、按需增长到它为止。校验 `>= 1`,收敛到 1..=64。默认:4
[client.data.kcp] # 可选。KCP carrier 的上限,键与规则相同
# max_tunnels = 4

[client.transport] # 可选。指定传输层如何封装;对控制面与数据面都生效
type = "plain" # 可选。可选值:["plain", "noise"]。默认:"plain"
proxy = "socks5://user:passwd@127.0.0.1:1080" # 可选。仅客户端。通过 `http`/`socks5` 代理连接服务端

[client.transport.noise] # Noise 协议。进一步说明见 `docs/transport.md`
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # 可选。默认值如所示
local_private_key = "key_encoded_in_base64" # 可选
remote_public_key = "key_encoded_in_base64" # 可选
psk = "key_encoded_in_base64" # 可选。预共享密钥,base64 编码后必须恰好解码为 32 字节,该长度只在建立连接的 Noise 握手时才检查。仅当配置的 `pattern` 在 `psk_location` 处带有 PSK 修饰符(如 Noise_KKpsk0_...)时才会使用它;pattern 不含 PSK 时该值被静默忽略,而不是被拒绝
psk_location = 0 # 可选。pattern 中使用的 PSK 槽位索引。默认:0
resume = true # 可选。Noise 会话恢复:重连时用 MAC 证明持有上一会话的握手哈希,而不是重做握手的密钥交换(选择器 0x02)。默认:false。见 `docs/transport.md`「Noise session resume」

[client.transparent] # 可选。仅透明(L3)服务:客户端连接的 TUN 设备
tun = "molehill0" # 可选。设备必须已存在,并在上面配好所声明的地址与路由——那是运维方的事,不是守护进程的。默认:molehill0

[client.services.service1] # 需要转发的服务。名称标识该服务(显示在日志中)
protocol = "tcp" # 可选。需要转发的协议。可选值:["tcp", "udp", "transparent"]。默认:"tcp"。透明服务自己拥有公网 ip:port,而不是转发到 local_addr——见下文「透明(L3)服务」
local_addr = "127.0.0.1:1081" # 必填。需要被转发的本地服务地址。protocol = "transparent" 时会被拒绝,因为本地应用自己绑定所声明的公网地址
remote_bind_addr = "0.0.0.0:8081" # 必填。该服务在服务端暴露的公网地址(透明服务是自己声明拥有它,而不是由服务端绑定)。必须被服务端的 `allow_ports` 覆盖
nodelay = true # 可选。该服务数据通道的 TCP_NODELAY。默认:即使不设置也为 true;设为 `false` 关闭
retry_interval = 1 # 可选。按服务的重连退避上限,语义与 `client.control.default_retry_interval` 相同。默认:继承 `client.control.default_retry_interval`
token = "service-specific-token" # 可选。仅对本服务覆盖 `client.default_token`——例如对使用独立 token 的服务端做鉴权 # security-scan:allow documentation placeholder
remote_addr = "server2.example.com:2333" # 可选。仅对本服务覆盖 `client.control.default_remote_addr`——它的控制通道(默认还包括数据面)拨向这个服务端。让同一个客户端可以把服务分散到多个 molehill 服务端
mode = "multiplex" # 可选。仅对本服务覆盖 `client.data.default_mode`:"multiplex"(默认)或 "direct"
carrier = "tcp" # 可选。仅对本服务覆盖 `client.data.default_carrier`;仅在 `mode = "multiplex"` 时有效。不设则继承默认值
transport = { type = "plain" } # 可选。按服务传输覆盖:`type`("noise" = 加密,"plain" = 明文;不设 = 跟随 `client.transport.type`)与 `noise` 密钥(本服务加密时使用;不设 = 用 `client.transport.noise`)。让同一个客户端明文与加密服务并存——例如拨向不同服务端、带自己公钥的服务

[client.services.service2] # 可以定义多个服务
protocol = "udp"
local_addr = "127.0.0.1:1082"
remote_bind_addr = "0.0.0.0:8082"
udp_workers = 2 # 可选。仅 UDP 服务:该服务的 worker 集合使用多少条数据通道;不同访客分片到这些通道上,单个访客绝不被拆到多条通道。隧道池至少保留这些通道所需的隧道数。默认:2。它是扇出,不是容量旋钮:报文上限是服务自身的属性,不随该值变化(多访客下 1400 字节数据报约 1 Gbit/s 即饱和),超过上限的数据报会被丢弃——这是设计接受的取舍,以免队头阻塞其他访客,`MOLEHILL_UDP_STATS` 会把它计入(`queue_full`)。实测见[基准测试](benchmarks.zh.md#udp-队列问题不属于-soak-模型)
udp_forwarder_ipv6 = false # 可选。仅 UDP 服务:UDP 转发器连接本地服务时优先使用 IPv6。默认:false
udp_buffer_size = 2048 # 可选。UDP 接收缓冲区,单位字节。默认:2048,最大 65535
udp_idle_timeout = 60 # 可选。客户端上空闲 UDP 对端映射被丢弃的秒数(其本地 socket——即本地服务看到的源端口——随之回收)。默认:60
udp_send_queue_size = 1024 # 可选。每条数据通道的出站数据报队列大小。默认:1024

[server]
default_token = "change-me" # 必填。必须与 `[client].default_token` 一致
allow_ports = ["6000-6999", "8080"] # 启用动态注册的必填项。为空或缺失:拒绝所有注册。请求的端口只要被某个条目包含就会被放行(单个端口,或覆盖它的范围——1024 以下的特权端口同样如此)

[server.control] # 必填。控制通道监听器
bind_addr = "0.0.0.0:2333" # 必填。服务端监听客户端连接的地址。通常只需改端口
heartbeat_interval = 30 # 可选。两次应用层心跳之间的间隔;客户端据此声明的节奏推导自己的超时。设为 0 禁用发送心跳。默认:30 秒

[server.data] # 可选。数据面监听器(特性 `multiplex`)
# bind_addr = "0.0.0.0:2343" # 可选。数据面监听地址;默认为 `server.control.bind_addr`。KCP UDP 监听也在第一条 `kcp` 注册到达时绑定到这里——默认地址下,TCP 控制与 UDP KCP 共用一个端口(协议不同互不冲突)
# stripe_count = 4 # 可选。每个访客连接使用的数据通道数,收敛到 1..=64。默认:1——每个访客一条数据通道。更大的值把每个访客连接摊到这么多条并行通道上(条带组):其吞吐天花板与在途窗口变为各通道之和,代价是每连接的重排缓冲。仅对 TCP 服务生效。两端都需要支持条带数据通道格式(见 docs/internals.md"数据通道条带"):只要池里有足够多的隧道,组的各条通道会落在不同隧道上,不够时则共享隧道。实验性测量覆盖:环境变量 `MOLEHILL_STRIPE_COUNT` 在取值为合法数量(1..=64)时替换此值;无法解析或超出范围的值会被忽略并打一条警告
# max_tunnels_per_client = 0 # 可选。运维方对弹性池的阀门:一个客户端在其会话的所有服务上一共可持有多少条多路复用数据隧道。0(默认)为不限。超过上限的隧道会被带类型地拒绝,并在应答里写明上限;会话本身继续运行

[server.transport] # 可选。只有密钥,没有 `type`。连接是否加密由客户端决定(每条连接以 1 字节传输选择器开头);放置密钥后服务端可以接受 Noise 连接(除此之外也接受明文)
[server.transport.noise] # 密钥。存在 = 服务端可以接受 Noise(选择器 0x01)
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"
psk = "key_encoded_in_base64" # 可选。预共享密钥,base64 编码后必须恰好解码为 32 字节,该长度只在建立连接的 Noise 握手时才检查。仅当配置的 `pattern` 在 `psk_location` 处带有 PSK 修饰符(如 Noise_KKpsk0_...)时才会使用它;pattern 不含 PSK 时该值被静默忽略,而不是被拒绝
psk_location = 0 # 可选。pattern 中使用的 PSK 槽位索引。默认:0
resume = true # 可选。Noise 会话恢复:重连时用 MAC 证明持有上一会话的握手哈希,而不是重做握手的密钥交换(选择器 0x02)。默认:false。见 `docs/transport.md`「Noise session resume」

[server.transparent] # 可选。仅透明(L3)服务:服务端连接的 TUN 设备
tun = "molehill0" # 可选。设备必须已存在,并配有通向每个所声明地址的路由——那是运维方的事,不是守护进程的。默认:molehill0
```

## 动态服务注册

不再有 `[server.services.*]` 块。生命周期如下:

1. 客户端用 `default_token` 鉴权。
2. 对每个配置的服务,客户端发送 `RegisterService` 消息:名称、
   `protocol`(tcp/udp/transparent)、`remote_bind_addr`、将要使用的数据面
   `carrier`(tcp/kcp——`kcp` carrier 会触发服务端懒绑定 UDP 监听)与
   UDP 缓冲大小。通道数不在消息里:客户端打开自己配置的通道(TCP 每个访客
   一条,UDP 为 `udp_workers` 条,透明服务为一条长生命周期通道),服务端在
   访客到达时再要一条。
3. 服务端校验:
   - **白名单**:请求的端口必须被 `allow_ports` 覆盖;为空/缺失的
     `allow_ports` 会拒绝*每一次*注册(这也是完全禁用该特性的方式);
   - **特权端口**:没有单独规则——白名单条目(单个端口或范围)同样
     放行其中的 1024 以下端口。绑定它们仍会失败,除非服务端具备操作系统
     特权(root,或调低
     `net.ipv4.ip_unprivileged_port_start`);所以服务端确有特权时,
     建议把要暴露的特权端口逐个列出;
   - **冲突**:端口已被占用时,注册失败并返回 `Port already in use`。
4. 成功时服务端立即绑定该端口并开始转发。

被拒绝对该服务的本次运行是永久性的:客户端记录服务端返回的具体原因并
放弃,直到你修复配置或重启。服务名在同一服务端内必须唯一;重启的客户端
重新注册时会干净地接管。

## 多路复用(`multiplex` 特性)

`multiplex` 特性是默认特性集的一部分。`mode = "multiplex"`(默认)时,
注册的服务跑在一个**弹性隧道池**上(上限
`[client.data.tcp|kcp].max_tunnels`,默认 4),之后每条数据通道都变成其中
一条隧道内的 yamux 流。这消除了每条连接的握手延迟
(TCP 连接,以及 `noise` 下的 Noise 握手),并在大量并发访客下大幅减少
FD 占用。

- 决定权只在客户端(`[client.data].default_mode`);服务端按连接自动适配。
- `mode = "direct"` 恢复每通道一条连接的行为。
- 每条隧道的缓冲由内部固定默认值约束(32 MiB yamux 接收窗口、64 条流):
  丢包积压有界且吞吐无损;这两个值固定是因为 yamux 将两者耦合(见
  internals.md)。
- **池是冷启动的。** 在真的需要隧道之前什么都不会拨:某个服务的第一个访客
  会同步把池撑起来,因此这位访客要先付一次隧道建连才开始过字节(回环上
  2.0-3.2 ms,M2a);之后的访客都能用上热隧道,池也会按需继续长到
  `max_tunnels`。空闲的池在 `[client.data].idle_timeout`(默认 60 秒)之后
  归还隧道,但不会少于一条,也不会低于某 UDP 服务的 worker 所需的下限。
- `max_tunnels = N` 是该 carrier 的池可增长到的上限。独立 TCP 流
  隔离队头阻塞(丢段只停滞自己的隧道),并可超越单条流的拥塞窗口聚合吞吐。
  某条隧道死亡时,开启请求会透明地落到存活隧道,直到常规心跳重连重建整个
  池。默认:4;`1` 恢复单隧道行为。
- **实验性(传输层对比选项):** `carrier = "kcp"` 把数据面换成 KCP-over-UDP
  会话而不是 TCP 连接(特性 `kcp`,属于默认特性集)。KCP 是用户态 ARQ 协议,
  用吞吐换 UDP 会话质量,所以它是 A/B 对比选项而不是默认:与 TCP carrier 的实测
  对比见[基准测试](benchmarks.zh.md#每个配置选择的代价逐项实测)。
  加密栈不变——transport 为 `noise` 时同样的
  Noise 握手包裹每个 KCP 会话——数据通道仍由 yamux 承载,`max_tunnels` 照常生效。
  服务端在第一条声明 `kcp` carrier 的注册到达时才打开 UDP 监听——绑定失败
  会变成精确的注册拒绝;没有 KCP 客户端的服务端永远不会打开 UDP socket。
  监听绑定在数据地址上(`[server.data].bind_addr`,默认 = 控制地址),每个
  会话像 TCP 隧道一样用控制会话的 nonce 认证。如果两端都不设置数据地址,**TCP 控制
  与 UDP KCP 就共用一个端口号**:TCP 和 UDP 是不同协议,两个 socket 绑定同一端口
  不会冲突(记得在防火墙/NAT 里同时放行两种协议)。固定的 KCP 参数(为可比性记录):
  流模式、nodelay 10 ms 间隔、fast-resend 2、关闭拥塞控制、发送窗口 2048 /
  接收窗口 4096 段、MTU 1400、32 MiB socket 缓冲。保活:适配器层每 2 秒的
  PING/PONG 让空闲隧道保持活跃(NAT 映射)并探测路径 RTT;对端消失仍只在
  下一次写入时确认(约 20 次 RTO 后判死)。
- 编译时去掉该特性则完全移除这个选项,而且这样的构建根本不能看到相应的表:
  配置里出现 `[client.data]` 或 `[server.data]` 就会被拒绝(未知键——
  `deny_unknown_fields`)。删掉这两个表之后,数据面始终走每通道一条连接。

**按服务覆盖。** `[client.data]` 存放默认值;每个服务可以在自己的
`[client.services.<name>]` 块里单独覆盖 `mode` 与 `carrier`。
合并后的视图遵循与全局块相同的规则:`carrier` 只在
`mode = "multiplex"` 时有效,`carrier = "kcp"` 还额外需要 `kcp` 特性。
服务的 carrier 决定它的池长到哪个上限
(`[client.data.tcp|kcp].max_tunnels`);开启 `[client.data].shared_pool` 时,
同一会话的所有服务共用每个 carrier 一个池。于是同一个客户端可以混合:
交互式服务走 mux(握手少、对 NAT 友好),大流量传输服务走 `direct`
(原始吞吐优先),服务端无需任何配置改动:服务端按连接自动适配,并在第一条
`kcp` 注册时打开自己的 KCP 监听(没有按 carrier 的服务端配置)。同样的覆盖
模式也适用于控制默认值:`token`、`remote_addr` 分别覆盖
`[client].default_token`、`[client.control].default_remote_addr`,
`retry_interval` 覆盖 `default_retry_interval`。心跳不是按服务的旋钮:一条
会话只有一个计时器,由服务端声明的节奏推导(见
`[client.control].default_heartbeat_timeout`)。`default_data_addr` 本身不能按
服务设置——数据面端点会跟随该服务自己的服务端(见下)。

**多服务端。** 服务也可以覆盖服务端本身:`[client.services.<name>].remote_addr`
为该服务的控制通道替换 `[client.control].default_remote_addr`,数据面默认
跟随(隧道拨同一个端点,因为服务端的数据监听默认就在控制地址上)。于是
同一个客户端可以把服务分散到多个 molehill 服务端——每个区域就近的副本、
按租户分服务端,或者逐个迁移服务的窗口期。每个服务端都必须用 token 认证
服务:某服务端与客户端 `default_token` 不同时,该服务可以带自己的 `token`;
每个服务端的 `allow_ports` 必须覆盖注册在它上面的服务,而客户端会按各个
服务端声明的节奏分别推导该会话的心跳超时,因此节奏不同的服务端可以共存。
数据面端点解析链是:服务自己的 `remote_addr` →
`[client.data].default_data_addr` → `[client.control].default_remote_addr`
——因此全局 `default_data_addr` 只作用于没有自己 `remote_addr` 的服务;某个
服务端把数据监听放在独立端口(不同的 `[server.data].bind_addr`)时,需要
在 `[client.data].default_data_addr` 里全局设置该地址,它会作用于所有跟随
客户端级端点的服务。

线级设计——隧道升级、每流分帧与窗口,以及池化流为何需要 SYN 启动——
见[内部原理](./internals.md)。

## 透明(L3)服务

`protocol = "transparent"` 的服务把公网 `ip:port` 交给**客户端**拥有,而不是让
服务端绑定它。客户端的宿主机在自己的 TUN 设备上承载所声明的地址,服务端把整个
IP 包路由进隧道,由客户端自己的内核应答访客——因此后端看到访客的真实源地址,
TCP 保持端到端语义,服务端也不为该连接持有任何 socket 或按流的状态。

它**仅支持 Linux**,两端都需要 `CAP_NET_ADMIN`(各自要连接一个 TUN 设备);
`transparent` 特性属于默认特性集。其他平台上的配置、或在不含该特性的构建里,
都会在解析配置时被拒绝:信息形如 `... carries whole IP packets through a TUN
device, and this platform is not Linux`,或指明缺少 `transparent` 特性。目前只
承载 IPv4——非 IPv4 的包会被丢弃并计数。

**守护进程从不配置网络。** 它没有 netlink 代码,也从不调用 `ip`:TUN 设备与
地址、路由都由运维方创建和安装,守护进程只校验自己依赖的东西,缺什么就用要执行
的确切命令拒绝。两套配方——地址被路由到服务端,以及单 IP 服务端——见
[部署文档](./deployment.zh.md#透明l3服务)。

| 键 | 含义 |
|---|---|
| `[client.services.<name>].protocol` | `"transparent"`——该服务拥有一个公网 `ip:port`,而不是转发到 `local_addr` |
| `[client.services.<name>].remote_bind_addr` | 客户端**声明拥有**的公网 `ip:port`。端口必须被服务端的 `allow_ports` 覆盖;地址必须是客户端本地的(配方会把它配到 TUN 设备上) |
| `[client.transparent].tun` | 客户端连接的 TUN 设备。默认:`molehill0` |
| `[server.transparent].tun` | 服务端连接的 TUN 设备。默认:`molehill0` |

转发型服务会用到的四类东西在这里会被**解析期拒绝**,因为没有任何代码会读它们:
`local_addr`(本地应用自己绑定所声明的公网地址,本客户端不拨任何东西)、
`nodelay`(同理),以及仅限 UDP 的 `udp_workers`、`udp_buffer_size`、
`udp_idle_timeout`、`udp_send_queue_size` 与 `udp_forwarder_ipv6`,以及
**加密**:transparent 服务永不加密——内容由访问者自己的端到端保护(TLS,或该协议
自带的任何加密)负责,这一跳按设计就是明文链路。因此无论你在按服务的 `transport`
表里要求加密,还是 client 级的 `[client.transport].type = "noise"` 会作用到它,配置
都会被拒绝;拒绝信息会指出要删掉的键。

### 运维方需要准备什么

两端都连接到自己 `tun` 键指定的**已存在**设备;守护进程刻意不创建设备,因为
运维方的地址和路由就落在它上面。

- **设备存在**(两端)。设备缺失时,拒绝信息会给出创建它的两条命令——
  `ip tuntap add dev <tun> mode tun` 与 `ip link set <tun> up mtu 1400`。
- **客户端承载它声明的每个地址。** 所声明的 IP 就是 `remote_bind_addr` 里的
  那个,客户端必须拥有它:拒绝信息会打印 `ip addr add <ip>/32 dev <tun>`、
  `ip rule add from <ip> lookup 100` 与 `ip route add default dev <tun> table
  100`。地址必须是本地的,因为应用要绑定它;源地址规则则把该应用发出的回包送回
  隧道。
- **关闭反向路径过滤。** `net.ipv4.conf.<tun>.rp_filter` **和**
  `net.ipv4.conf.all.rp_filter` 都必须读到 `0`——注入的包携带访客的源地址,
  严格的检查会丢掉它们——拒绝信息会打印确切的 `sysctl -w` 行。这项检查在
  客户端执行;服务端只校验自己的设备是否存在。
- **路由要把所声明的地址带到服务端**,并让客户端的回包出得去。两套配方见
  [部署文档](./deployment.zh.md#透明l3服务)。
- 两个进程都需要 `CAP_NET_ADMIN`;[部署文档](./deployment.zh.md#systemd)的
  systemd 单元展示了 `AmbientCapabilities=` 行。

### 一个完整示例

```toml
# server.toml - 服务端不为该服务绑定任何东西:它把 10.99.0.1/32 路由进自己的
# TUN 设备。
[server]
default_token = "change-me"
allow_ports = ["8443"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.transparent]
tun = "molehill0"
```

```toml
# client.toml - 客户端拥有 10.99.0.1:8443,并在自己的 TUN 设备上承载该地址,
# 应用就绑定在那里。
[client]
default_token = "change-me"

[client.control]
default_remote_addr = "203.0.113.5:2333"

[client.transparent]
tun = "molehill0"

[client.services.web]
protocol = "transparent"
remote_bind_addr = "10.99.0.1:8443"
```

多个服务只有在端口不同时才能声明同一个地址;而没有端口可路由的包——ICMP,以及
首个分片之后的分片——只有在恰好一个服务声明该地址时才会投递,否则宁可丢弃也不
猜测。`MOLEHILL_L3_STATS=1` 每秒打印一次数据面的累计计数(见
[诊断开关](#诊断开关按需开启));两端各自如何判断一个包属于哪个服务,见
[内部原理](./internals.md#transparent-l3-services)。

## 日志

和许多 Rust 程序一样,`molehill` 用环境变量控制日志级别。可选 `info`、
`warn`、`error`、`debug`、`trace`。

```shell
RUST_LOG=error ./molehill config.toml
```

以上命令只输出 error 级别的日志。

未设置 `RUST_LOG` 时,默认日志级别为 `info`。

日志行带彩色级别(红色 ERROR、黄色 WARN、绿色 INFO、青色 DEBUG、紫色
TRACE)和当前 span 上下文,例如 `handle{service=ssh}:`——繁忙服务端的每
一行日志都告诉你它来自哪个服务。颜色只在终端上启用;重定向输出保持纯文本
(同样遵循 `NO_COLOR`)。`debug`/`trace` 级别下每行末尾会附加来源模块。

### 各级别的含义

级别表达的是**谁需要行动**,而不是事件听起来多严重:

| 级别 | 含义 | 例子 |
|------|------|------|
| `ERROR` | 需要人介入,工具自己解决不了 | 服务端拒绝了注册、监听器无法 accept、退避重试放弃 |
| `WARN` | 软件已自行处理,但值得留下一行 | 已被移除、当前被忽略的配置键;服务端拒绝了 token |
| `INFO` | 生命周期:某个东西启动、停止或改变了状态 | 服务注册成功、控制通道建立、关闭 |
| `DEBUG` | 单条连接或单次会话自己的事 | 访客的本地服务拒绝连接、数据通道结束、首次之后的每次重试 |

有几条值得写明的后果,它们正是让繁忙日志保持可读的原因:

- **一次失败的请求不是 WARN。** `local_addr` 拒绝连接的访客就是一条被关闭
  的连接,那一行是 `DEBUG`;访客看到什么见
  [本地服务未运行](#本地服务未运行)。
- **会重复出现的状况只报告一次。** 客户端先于服务端启动、或用错误的 token
  不断重试时,只产生一条 `INFO`/`WARN`,之后直到恢复都是 `DEBUG`;一次健康
  的运行不会产生任何 `WARN` 或 `ERROR`。`tests/log_budget_test.rs` 用真实
  二进制测量这一点,所以这是被强制执行的保证,而不是意图。
- **`RUST_LOG=debug` 是排障级别**,内容多是预期之中的:每条连接的细节都在
  那里。

### 诊断开关(按需开启)

六个环境变量用于打开聚合诊断,每个对象每秒一行 `INFO`。它们默认关闭,从不
改变转发路径;打开开关本身就是许可——需要把 `RUST_LOG` 提上去才看得见的行,
永远不会落到任何地方:

| 开关 | 输出 | 内容 |
|---|---|---|
| `MOLEHILL_MUX_STATS=1` | 每个 tunnel 每秒一行 | yamux 组帧累计计数(`written`、`read`、`bytes`)——即每秒帧数,配上一次 CPU 采样就是每帧 CPU |
| `MOLEHILL_KCP_STATS=1` | 每进程每秒一行 | KCP 适配器的累计计数(`datagrams_in`/`out`、`retransmits`、`acks_out`、`sacks_sent`、`blobs_out`、pump 轮数)以及把单个 segment 的用户态开销拆成 intake、delivery、writer drain、wire drain 与 ARQ update 的粗粒度分相计时 |
| `MOLEHILL_POOL_STATS=1` | 每个存活 pool 每秒一行 | pool 的 key、carrier、size、上限、UDP floor、存活 stream 数、pinned peer 数、每个 tunnel 的 `streams/pending/pinned`,以及每次尺寸变化的理由时间线(`+load:1->2`、`-idle:2->1`) |
| `MOLEHILL_PLACEMENT_STATS=1` | 每进程每秒一行 | 该区间的放置情况:次数、回退到其它 tunnel 的次数、候选与选中负载之和、`mean_spread`(做放置那一刻「最优候选」与「最差候选」之间平均相差多少个流槽位,也就是更聪明的规则本可以赢到多少),以及 open 延迟的均值与最大值 |
| `MOLEHILL_UDP_STATS=1` | 每进程每秒一行 | UDP affinity 表的大小、淘汰次数,以及每个 worker 的 pinned peer 数 |
| `MOLEHILL_L3_STATS=1` | 每条透明数据路径每秒一行 | 透明数据面的累计计数:`forwarded`、`dropped(not_ipv4, malformed, unclaimed, no_channel)` 与 `channel_errors` |

这些计数都是累计值:知道窗口的读者——或者取一轮运行的第一行与最后一行——
就能算出每秒速率与单位成本。pool 与 placement 两行就是共享弹性 pool 的 S1
观测(它做什么,以及为什么这些数字是聚合而不是逐个事件:
[internals.md](internals.md#the-tunnel-pool))。`MOLEHILL_STRIPE_COUNT` 是唯一
一个改变行为而不是观测行为的开关,它记录在 `stripe_count` 旁边——也就是它所
替换的那个值那里。

## 调优

按负载选择 `mode`/`max_tunnels`/`carrier`/transport 的方法就是上面的
[决策树](#选择配置决策树)(含实测花费与验证方式)。本节讲逐连接层面的
旋钮。

自 v0.4.7 起,molehill 默认在每条 TCP 连接上启用 TCP_NODELAY:控制通道、
数据面隧道、每条数据通道两端、面向访客的 socket、以及客户端连接本地服务的
连接。这对延迟与交互式应用(SSH、rdp、Minecraft 服务器)有益,但会略微
降低带宽。

`nodelay` 只有客户端会采纳,而且只作用于客户端为某个服务自己建立的两类
socket:每通道一条连接路径上的数据通道连接,以及它连向本地服务的 TCP
连接。其余 socket 一律保持 nodelay:控制通道两端始终设置 TCP_NODELAY,
客户端的多路复用隧道沿用同一套控制通道选项,服务端对每条数据通道自己这
一端以及面向访客的 socket 也始终使用固定的低延迟默认值(nodelay +
keepalive)。因此 `nodelay = false` 无法让这些 socket 重新启用 Nagle。

这些 socket 上也默认启用 TCP keepalive(空闲 20 秒、探测间隔 8 秒),因此
被 NAT 或中间设备静默丢弃的池化数据通道会被检测到,而不会发给访客。

如果带宽更重要,可以在每个服务上用 `nodelay = false` 关闭 TCP_NODELAY
——只作用于上面那些客户端侧 socket。

## 示例与部署

常见场景的完整示例(最小配置、Noise、UDP、合并一个文件、代理、iperf3)、
systemd 单元以及容器 / compose / Quadlet 部署见[部署与示例](./deployment.zh.md)。

## 使用说明

### 心跳

- 客户端根据服务端在会话确认里声明的节奏推导超时:`max(10 秒, 2 ×
  server.control.heartbeat_interval + 5 秒)`。除非需要别的取值,`client.control.default_heartbeat_timeout`
  保持不设置;低于推导下限的取值会在启动时被拒绝,并在消息中同时给出服务端的
  间隔与所需下限。
- 一条会话只有一个计时器,因此超时是会话级的事实:服务无法覆盖它(该键已
  移除——见上方的迁移表),因为想要更快检测的服务仍会与同胞服务共用这个
  计时器。`client.control.default_heartbeat_timeout` 设为 0 则禁用检测。
- 设置 `server.control.heartbeat_interval = 0` 可禁用心跳,此时客户端没有可
  推导超时的节奏。

### 本地服务未运行

- **服务在其客户端运行期间始终处于注册状态。** 没有健康检查,也没有由健康
  状态驱动的注销:`local_addr` 不必在客户端启动时就绪,它宕机时也不会从
  服务端撤下任何东西。
- 请求无法转发到 `local_addr`(连接被拒、超时……)的访客,**只有该连接**
  得到一次失败的请求——这与任何反向代理面对死掉的后端时一样。访客侧看到的
  是连接被关闭或重置;原因记录在客户端日志里(`service=<name>`)。其他访客
  以及该客户端的其他服务都不受影响。
- 对运维的含义:恢复后端不需要对 molehill 做任何操作。随时启动即可,已经
  注册的服务会重新开始转发;后端反复重启也不会让客户端付出重新注册的代价。
- **从 0.9.0 及更早版本升级:** `health_check` 键已被移除,请从
  `[client.services.<name>]` 中删除——仍带该键的配置不会启动,拒绝信息会指出
  这个键并说明应当改写成什么(见上方的迁移表)。

### UDP 服务

- 数据报大小上限遵循服务的 `udp_buffer_size`(默认 2048 字节,最大
  65535):更大的数据报会在入口处被**截断**到这个大小——前
  `udp_buffer_size` 字节照常投递、其余部分丢弃——通道仍可用,但载荷短了。
  `udp_buffer_size = 1024`
  的服务实测:2000 字节的数据报到达后端时是 1024 字节,其回包到达访客时也是
  1024 字节。按服务实际发送的最大数据报来设置,两端写同一个值,并注意服务端
  在注册时会执行自己收到的那份副本的限制。
- **会话亲和**:来自一个访客地址的所有数据报走同一条数据通道,并在访客的
  整个会话期间通过客户端上一个专用本地 socket 离开——有状态 UDP 服务
  (Minecraft Bedrock/RakNet、QUIC、WireGuard 等游戏服务器)会看到稳定的
  `(ip, port)`,会话保持完整。
- 映射(及其本地 socket)在 `udp_idle_timeout` 秒(默认 60)内双向无流量后
  被清理;下一个数据报会重新绑定新 socket,这改变了本地服务看到的源端口。
  保持默认值,或对长生命周期的有状态会话调大它。

### 传输层

传输层配置——Noise 密钥对、pattern 与 PSK——在[传输层文档](./transport.md)
中逐步说明:在上面的规范中选好 `type`,再跟着那篇指南做。

- **代理**:`[client.transport].proxy` 只作用于客户端到服务端的出站连接
  (控制通道、数据面隧道、以及直接数据通道)。支持 `socks5` 和 `http`
  (CONNECT),可带基础认证。它仅限客户端;服务端会拒绝它。

### 热重载

- 运行中编辑客户端配置:一般性修改(传输层、地址、token、数据面设置)会重启
  实例;添加、删除或修改服务则无需重启即可生效(服务在现有控制通道上
  注销/重新注册)。
- 编辑期间保持文件有效——无效配置会在启动或重载时被拒绝,并继续运行
  之前的状态。

### 多服务与多实例

- 一个客户端配置可以转发多个服务(多个 `[client.services.<name>]` 块),
  多个客户端也可以连接同一个服务端。每个注册的服务名在同一服务端的所有
  客户端之间必须唯一。
- 在一台主机上运行多个独立的 molehill 对时,让各实例的监听 `bind_addr`
  使用不同的端口,并使用独立的配置文件([systemd 单元](./deployment.zh.md#systemd)展示了模板化实例)。

## 故障排查

| 现象 | 原因 / 修复 |
|---|---|
| `Server rejected service <name>: Port N rejected ... allow_ports` | 请求的 `remote_bind_addr` 端口未在服务端白名单中,或服务端禁用了动态注册。修复 `allow_ports`。 |
| `Port N is already in use` | 服务端上另一个服务(或程序)占用了该端口。换一个 `remote_bind_addr` 端口。 |
| `Protocol version mismatched ... Please update` | 一端运行的是不说协议 v5 的 molehill(0.10 系列只服务 v4),因此旧客户端或旧服务端都会得到它;请两端一起升级。 |
| 客户端在服务端的 hello 始终不到达后以 `protocol v5` 停止 | 服务端早于本构建:它读到版本 5、自己的版本检查失败并关闭该连接。请升级服务端。 |
| 客户端出现 `Authentication failed` | 客户端与服务端的 `default_token` 不一致。 |
| `Failed to connect to <addr>: Connection refused` | 服务端未运行、`client.control.default_remote_addr` 端口错误,或 `server.control.bind_addr` 不可达。 |
| 配置能启动,但连接时报地址解析错误(`failed to lookup address information`) | 这些地址键只检查字符串里有没有 `:`,并不按 socket 地址解析:`client.control.default_remote_addr`、`client.services.<name>.remote_addr`、`client.data.default_data_addr`、`server.data.bind_addr`。因此裸 IPv6 字面量(如 `"::1"`)能通过启动校验,却没有端口,解析地址时才会失败。始终写 `主机:端口`,IPv6 字面量要加方括号——`"[::1]:2333"`。(服务的 `remote_bind_addr` 反而会按 `SocketAddr` 解析,启动时就会拒绝。) |
| 反复出现 `Heartbeat timed out` | 网络路径丢弃了连接,或服务端卡住。配置值*低于*推导下限不会出现在这里——它在启动时就被拒绝。 |
| Noise 握手失败 | 两端的密钥对、`psk` 或 pattern 不匹配。 |
| 启动时 `Proxy URL is missing the port` | `proxy` URL 缺少端口;修复配置。 |
| UDP 流量不通 | 检查 `protocol = "udp"`;大于 `udp_buffer_size` 的数据报会被截断到该大小;空闲映射在 `udp_idle_timeout` 秒后超时。 |
| 有状态 UDP 会话(游戏、QUIC、WireGuard)中途断开 | 确保两端运行带 UDP 会话亲和的版本(≥ 本修复);空闲超过 `udp_idle_timeout` 的对端会在下一个数据报时重新绑定到新本地 socket(源端口变化)——调大超时或发送周期流量。 |
| `Failed to read cmd: early eof` 警告 | 对端关闭了通道(重启或关停);客户端会自动重连。 |
| 客户端报 `Interface <tun> does not exist. Prepare it first`(服务端则表现为注册被拒) | 透明服务连接的是运维方创建的设备,守护进程绝不自己创建。执行信息里打印的 `ip tuntap add dev <tun> mode tun` 与 `ip link set <tun> up mtu 1400`(配方见[部署文档](./deployment.zh.md#透明l3服务))。 |
| `Transparent service claims <ip>, but no local interface carries it` | 客户端必须拥有它声明的地址:执行信息里打印的 `ip addr add <ip>/32 dev <tun>`、`ip rule add from <ip> lookup 100` 与 `ip route add default dev <tun> table 100`。 |
| `Reverse-path filtering is on (net.ipv4.conf.<tun>.rp_filter = 1)` | 注入的包携带访客的源地址,严格的检查会丢掉它们。执行信息里打印的 `sysctl -w` 行;`net.ipv4.conf.<tun>.rp_filter` 与 `net.ipv4.conf.all.rp_filter` 都必须读到 `0`。 |
| `Address <ip:port> is already claimed by another transparent service on this server` | 两个客户端声明了同一个端点;先到的声明在其注册存续期间一直持有。给其中一个换地址或端口。 |
| `protocol = "transparent"` 在启动时被拒,信息为 `... and this platform is not Linux`,或指明缺少 `transparent` 特性 | 该服务类型需要 Linux 构建并启用 `transparent` 特性(默认特性集的一部分)。在其他平台上请改用 `tcp`/`udp`。 |
| 透明服务的访客拿不到任何响应,`MOLEHILL_L3_STATS=1` 计入 `unclaimed` 丢弃 | 路由不完整:服务端需要一条把所声明地址送进自己设备的路由,客户端需要 `from <ip>` 规则及其表内路由(见[部署文档](./deployment.zh.md#透明l3服务))。`unclaimed` 也会出现在没有任何通道持有该端点时的重连窗口;`no_channel` 则表示该端点的队列已满。 |
