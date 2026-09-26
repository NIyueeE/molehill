# 配置

`molehill` 可以根据配置文件的内容自动判断以服务端还是客户端模式运行:如果
`[server]` 与 `[client]` 块只出现一个,就自动选择对应模式,如
[快速开始](../README.zh.md#快速开始)中的示例。

`[client]` 与 `[server]` 块也可以放在同一个文件里:此时在服务端运行
`molehill --server config.toml`,在客户端运行 `molehill --client config.toml`,
显式指定运行模式。

在阅读完整配置规范之前,建议先浏览文末的[完整示例](#完整示例)熟悉配置格式。

加密与 `transport` 块的更多细节见[传输层文档](./transport.md)。

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
> `default_mode`、`default_count`、`default_carrier`)——以便与
> `[client.services.<name>]` 上的按服务覆盖键(`protocol`、`remote_addr`、
> `token`、`heartbeat_timeout`、`retry_interval`、`mode`、`count`、
> `carrier`、`transport`、`udp_forwarder_ipv6`、`udp_send_queue_size` 等;
> 0.8 新增)清晰区分。`[client.transport]` 的 `type`/`noise` 保持无前缀:
> 它的按服务覆盖在嵌套的 `transport` 表里,服务层不存在同名冲突
> (`default_` 前缀正是为了消解同名冲突而存在)。
> 旧键会被拒绝(`deny_unknown_fields`),绝不会被静默忽略。
>
> **0.8 协议 v3**:每条连接以 1 字节传输选择器开头(`0x00` 明文 /
> `0x01` noise),注册消息携带数据面 carrier——两端必须一起升级;版本
> 不匹配是硬错误。

## 选择配置(决策树)

默认配置——`mode = "multiplex"`、`count = 4`、`carrier = "tcp"`、明文
传输——对绝大多数人是正确的起点。只有树上有明确分支时才偏离;每次只改一项,
并在**你自己的路径上**测量结果:已发布的运行、它们的数字以及如何复现,见
[基准测试](benchmarks.zh.md)。本页负责的是**每个设置做了什么**:

```mermaid
flowchart TD
    A["起点:默认配置<br/>multiplex、count=4、carrier=tcp、明文"] --> B{"流量经过不可信网络?"}
    B -- 是 --> C["transport type = noise<br/>+ 密钥(见传输层文档)"]
    B -- 否 --> D{"单个服务或少数<br/>长连接?"}
    C --> D
    D -- "是,且原始吞吐优先" --> E["mode = direct"]
    D -- "否:多服务、多用户、<br/>高连接频率" --> F{"并发连接很多?"}
    E --> Z["完成——按需用<br/>[client.services.*] 覆盖"]
    F -- "> ~256 并发" --> G["count = 8 或更高"]
    F -- 一般 --> H["保持 count = 4"]
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
| `count` | `1` | 所有流量共用一条隧道:没有跨流聚合,且共享同一重传域,一次丢包会一起卡住 |
| `count` | `4`(默认) | 聚合越过单流,并在隧道之间隔离队头阻塞;`count × 64` 并发连接 |
| `count` | `8+` | 更多并行隧道(更多 NAT 映射)与按比例更高的连接上限 |
| `carrier` | `"tcp"`(默认) | 有损与限速路径上表现良好的默认值;前提是网络不封锁 TCP 隧道 |
| `carrier` | `"kcp"` | TCP 隧道被封锁/限速时的延迟优先 UDP 传输;它不做多路复用,因此需要配合 `noise` + `count` 来拿连接上限 |
| transport | `"plain"` | 不加密;每字节开销最低 |
| transport | `"noise"` | 用单个预共享密钥对加密线路;RTT 代价亚毫秒,满载无 CPU 惩罚 |
| `pool_size` | 8 TCP / 2 UDP(默认) | 足够的热数据通道吸收建连抖动;UDP 把不同访客分片到不同通道,绝不拆分单个会话(会话亲和) |

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
default_heartbeat_timeout = 40 # 可选。设为 0 可禁用应用层心跳检测。取值必须大于 `server.control.heartbeat_interval`。默认:40 秒
default_retry_interval = 1 # 可选。重连退避的上限,而非固定间隔:延迟从 1 秒开始、按 3 倍增长并带抖动,最高不超过该值(抖动会让单次睡眠最长达到该上限的两倍),共 3 次重试;退避耗尽后客户端回落到固定 1 秒的重试循环。默认:1 秒

[client.data] # 可选。所有服务的数据面默认值(特性 `multiplex`,默认构建的一部分)。每个服务都可以单独覆盖 default_mode/default_count/default_carrier——见下方 `[client.services.*]` 里的按服务键
# default_data_addr = "example.com:2343" # 可选。数据面端点;默认为服务的控制端点(设置了 `client.services.<name>.remote_addr` 时用该地址,否则用 `client.control.default_remote_addr`)。`default_carrier = "kcp"` 时 KCP 会话用 UDP 拨控制地址——TCP 控制与 UDP KCP 可以共用一个端口(协议不同互不冲突)
default_mode = "multiplex" # 可选。默认数据面模式:"multiplex"(默认)或 "direct"(每条数据通道一条连接;`count`/`carrier` 不适用)
default_count = 4 # 可选。每个服务的默认并行隧道连接数;仅在 `default_mode = "multiplex"` 时生效。默认:4(吞吐与队头阻塞隔离优于单条 TCP 流;1 = 单隧道行为),范围 1..=64
default_carrier = "tcp" # 可选。默认数据载体:"tcp"(默认)复用控制通道的传输栈;"kcp" 使用 KCP-over-UDP 会话(特性 `kcp`;服务端在第一条 `kcp` 注册到达时才打开 KCP 监听,无需服务端配置)。两种传输都可与 KCP 组合:transport 为 `noise` 时同样的 Noise 握手包裹每个 KCP 会话,`plain` 时会话保持明文

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

[client.services.service1] # 需要转发的服务。名称标识该服务(显示在日志中)
protocol = "tcp" # 可选。需要转发的协议。可选值:["tcp", "udp"]。默认:"tcp"
local_addr = "127.0.0.1:1081" # 必填。需要被转发的本地服务地址
remote_bind_addr = "0.0.0.0:8081" # 必填。该服务在服务端暴露的公网地址。必须被服务端的 `allow_ports` 覆盖
nodelay = true # 可选。该服务数据通道的 TCP_NODELAY。默认:即使不设置也为 true;设为 `false` 关闭
retry_interval = 1 # 可选。按服务的重连退避上限,语义与 `client.control.default_retry_interval` 相同。默认:继承 `client.control.default_retry_interval`
token = "service-specific-token" # 可选。仅对本服务覆盖 `client.default_token`——例如对使用独立 token 的服务端做鉴权 # security-scan:allow documentation placeholder
remote_addr = "server2.example.com:2333" # 可选。仅对本服务覆盖 `client.control.default_remote_addr`——它的控制通道(默认还包括数据面)拨向这个服务端。让同一个客户端可以把服务分散到多个 molehill 服务端
heartbeat_timeout = 60 # 可选。仅对本服务覆盖 `client.control.default_heartbeat_timeout`——例如该服务对端的服务端心跳间隔不同
udp_forwarder_ipv6 = false # 可选。UDP 转发器连接本地服务时优先使用 IPv6(仅 UDP 服务)。默认:false
mode = "multiplex" # 可选。仅对本服务覆盖 `client.data.default_mode`:"multiplex"(默认)或 "direct"
count = 4 # 可选。仅对本服务覆盖 `client.data.default_count`;仅在 `mode = "multiplex"` 时有效,并收敛到 1..=64。不设则继承默认值
carrier = "tcp" # 可选。仅对本服务覆盖 `client.data.default_carrier`;仅在 `mode = "multiplex"` 时有效。不设则继承默认值
transport = { type = "plain" } # 可选。按服务传输覆盖:`type`("noise" = 加密,"plain" = 明文;不设 = 跟随 `client.transport.type`)与 `noise` 密钥(本服务加密时使用;不设 = 用 `client.transport.noise`)。让同一个客户端明文与加密服务并存——例如拨向不同服务端、带自己公钥的服务
pool_size = 8 # 可选。预建立的数据通道数。默认:TCP 为 8,UDP 为 2。受服务端 `max_pool_size` 限制。对 UDP 而言,这会把不同的访客分片到不同通道;每个访客固定钉在一个通道上(会话亲和)

[client.services.service2] # 可以定义多个服务
protocol = "udp"
local_addr = "127.0.0.1:1082"
remote_bind_addr = "0.0.0.0:8082"
udp_buffer_size = 2048 # 可选。UDP 接收缓冲区,单位字节。默认:2048,最大 65535
udp_idle_timeout = 60 # 可选。客户端上空闲 UDP 对端映射被丢弃的秒数(其本地 socket——即本地服务看到的源端口——随之回收)。默认:60
udp_send_queue_size = 1024 # 可选。每条数据通道的出站数据报队列大小。默认:1024

[server]
default_token = "change-me" # 必填。必须与 `[client].default_token` 一致
allow_ports = ["6000-6999", "8080"] # 启用动态注册的必填项。为空或缺失:拒绝所有注册。请求的端口只要被某个条目包含就会被放行(单个端口,或覆盖它的范围——1024 以下的特权端口同样如此)
max_pool_size = 16 # 可选。应用于每个服务请求的 pool_size 的上限。默认:不限制

[server.control] # 必填。控制通道监听器
bind_addr = "0.0.0.0:2333" # 必填。服务端监听客户端连接的地址。通常只需改端口
heartbeat_interval = 30 # 可选。两次应用层心跳之间的间隔。设为 0 禁用发送心跳。默认:30 秒

[server.data] # 可选。数据面监听器(特性 `multiplex`)
# bind_addr = "0.0.0.0:2343" # 可选。数据面监听地址;默认为 `server.control.bind_addr`。KCP UDP 监听也在第一条 `kcp` 注册到达时绑定到这里——默认地址下,TCP 控制与 UDP KCP 共用一个端口(协议不同互不冲突)
# stripe_count = 4 # 可选。每个访客连接使用的数据通道数,收敛到 1..=64。默认:1——每个访客一条数据通道。更大的值把每个访客连接摊到这么多条并行通道上(条带组):其吞吐天花板与在途窗口变为各通道之和,代价是每连接的重排缓冲。仅对 TCP 服务生效。两端都需要支持条带数据通道格式(见 docs/internals.md"数据通道条带")。实验性测量覆盖:环境变量 `MOLEHILL_STRIPE_COUNT` 在取值为合法数量(1..=64)时替换此值;无法解析或超出范围的值会被忽略并打一条警告

[server.transport] # 可选。只有密钥,没有 `type`。连接是否加密由客户端决定(每条连接以 v3 传输选择器字节开头);放置密钥后服务端可以接受 Noise 连接(除此之外也接受明文)
[server.transport.noise] # 密钥。存在 = 服务端可以接受 Noise(选择器 0x01)
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"
psk = "key_encoded_in_base64" # 可选。预共享密钥,base64 编码后必须恰好解码为 32 字节,该长度只在建立连接的 Noise 握手时才检查。仅当配置的 `pattern` 在 `psk_location` 处带有 PSK 修饰符(如 Noise_KKpsk0_...)时才会使用它;pattern 不含 PSK 时该值被静默忽略,而不是被拒绝
psk_location = 0 # 可选。pattern 中使用的 PSK 槽位索引。默认:0
resume = true # 可选。Noise 会话恢复:重连时用 MAC 证明持有上一会话的握手哈希,而不是重做握手的密钥交换(选择器 0x02)。默认:false。见 `docs/transport.md`「Noise session resume」
```

## 动态服务注册

不再有 `[server.services.*]` 块。生命周期如下:

1. 客户端用 `default_token` 鉴权。
2. 对每个配置的服务,客户端发送 `RegisterService` 消息:名称、
   `protocol`(tcp/udp)、`remote_bind_addr`、将要使用的数据面
   `carrier`(tcp/kcp——`kcp` carrier 会触发服务端懒绑定 UDP 监听)、
   `pool_size` 与 UDP 缓冲大小。
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
每个服务在注册后会打开 **N 条隧道连接**(`count`,默认 4),之后每条数据
通道都变成其中一条隧道内的 yamux 流。这消除了每条连接的握手延迟
(TCP 连接,以及 `noise` 下的 Noise 握手),并在大量并发访客下大幅减少
FD 占用。

- 决定权只在客户端(`[client.data].default_mode`);服务端按连接自动适配。
- `mode = "direct"` 恢复每通道一条连接的行为。
- 每条隧道的缓冲由内部固定默认值约束(64 MiB yamux 接收窗口、64 条流):
  丢包积压有界且吞吐无损;这两个值固定是因为 yamux 将两者耦合(见
  internals.md)。
- `count = N` 为每个服务打开 N 条并行隧道,数据通道轮询分摊。独立 TCP 流
  隔离队头阻塞(丢段只停滞自己的隧道),并可超越单条流的拥塞窗口聚合吞吐。
  某条隧道死亡时,开启请求会透明地落到存活隧道,直到常规心跳重连重建整个
  池。默认:4;`1` 恢复单隧道行为。
- **实验性(传输层对比选项):** `carrier = "kcp"` 把数据面换成 KCP-over-UDP
  会话而不是 TCP 连接(特性 `kcp`,属于默认特性集)。KCP 是用户态 ARQ 协议,
  用吞吐换 UDP 会话质量:它在每个实测格子的吞吐都输给 TCP carrier(常常差一个
  数量级),但丢包和高 RTT 下的 UDP 回声实测更干净(rtt100 下 0% 丢包、最大
  包间隔约 20 ms,而 TCP 各 arm 在 100 ms 以上),CPU 与 RSS 是数倍。加密栈不变——transport 为 `noise` 时同样的
  Noise 握手包裹每个 KCP 会话——数据通道仍由 yamux 承载,`count` 照常生效。
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
`[client.services.<name>]` 块里单独覆盖 `mode`、`count` 与 `carrier`。
合并后的视图遵循与全局块相同的规则:`count` 和 `carrier` 只在
`mode = "multiplex"` 时有效,`carrier = "kcp"` 还额外需要 `kcp` 特性。
隧道池本来就已经按服务各自建立——这次只是
把开关也变成按服务的,于是同一个客户端可以混合:交互式服务走 mux(握手少、
对 NAT 友好),大流量传输服务走 `direct`(原始吞吐优先),服务端无需任何
配置改动:服务端按连接自动适配,并在第一条 `kcp` 注册时打开自己的 KCP
监听(没有按 carrier 的服务端配置)。同样的覆盖模式也适用于控制默认值:`token`、
`remote_addr`、`heartbeat_timeout` 分别覆盖 `[client].default_token`、
`[client.control].default_remote_addr`、
`[client.control].default_heartbeat_timeout`,`retry_interval` 覆盖
`default_retry_interval`。`default_data_addr` 本身不能按服务设置——数据面端点
会跟随该服务自己的服务端(见下)。

**多服务端。** 服务也可以覆盖服务端本身:`[client.services.<name>].remote_addr`
为该服务的控制通道替换 `[client.control].default_remote_addr`,数据面默认
跟随(隧道拨同一个端点,因为服务端的数据监听默认就在控制地址上)。于是
同一个客户端可以把服务分散到多个 molehill 服务端——每个区域就近的副本、
按租户分服务端,或者逐个迁移服务的窗口期。每个服务端都必须用 token 认证
服务:某服务端与客户端 `default_token` 不同时,该服务可以带自己的 `token`;
某服务端 `heartbeat_interval` 不同时,该服务可以带自己的
`heartbeat_timeout`;每个服务端的 `allow_ports` 必须覆盖注册在它上面的
服务。数据面端点解析链是:服务自己的 `remote_addr` →
`[client.data].default_data_addr` → `[client.control].default_remote_addr`
——因此全局 `default_data_addr` 只作用于没有自己 `remote_addr` 的服务;某个
服务端把数据监听放在独立端口(不同的 `[server.data].bind_addr`)时,需要
在 `[client.data].default_data_addr` 里全局设置该地址,它会作用于所有跟随
客户端级端点的服务。

线级设计——隧道升级、每流分帧与窗口,以及池化流为何需要 SYN 启动——
见[内部原理](./internals.md)。

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

## 调优

按负载选择 `mode`/`count`/`carrier`/transport 的方法就是上面的
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

## 完整示例

开箱即用的配置(以前作为独立文件放在 `examples/` 目录;现收录于此作为
参考——以下每个代码块都能被当前二进制解析,配置测试套件会持续校验)。

### 最小配置

一对最小客户端与服务端:

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

```toml
# server.toml
[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

### 全部选项(完整参考)

```toml
# Complete client configuration example.
# Every option is documented in the specification above.

[client]
default_token = "default_token_if_not_specify" # security-scan:allow documentation placeholder # Optional. Default token for services without their own

[client.control]
default_remote_addr = "myserver.com:2333" # Necessary. The address of the server
default_heartbeat_timeout = 40 # Optional. Set to 0 to disable the application-layer heartbeat test. Must be greater than `server.control.heartbeat_interval`. Default: 40 seconds
default_retry_interval = 1 # 可选。重连退避的上限,而非固定间隔:延迟从 1 秒开始、按 3 倍增长并带抖动,最高不超过该值(抖动会让单次睡眠最长达到该上限的两倍),共 3 次重试;退避耗尽后客户端回落到固定 1 秒的重试循环。默认:1 秒

# Data-plane options (`[client.data]`) live here too; see the specification.
# They require the `multiplex` feature, which is part of the default build.
# Every service may also override mode/count/carrier on its own block.

[client.transport] # Optional. The whole block is optional
type = "plain" # Optional. Possible values: ["plain", "noise"]. Default: "plain"
proxy = "socks5://user:passwd@127.0.0.1:1080" # Optional. Connect to the server via a proxy. `socks5` and `http` are supported

[client.transport.noise] # Necessary only if `type` is "noise". See docs/transport.md
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # 可选。预共享密钥,base64 编码后必须恰好解码为 32 字节,该长度只在建立连接的 Noise 握手时才检查。仅当配置的 `pattern` 在 `psk_location` 处带有 PSK 修饰符(如 Noise_KKpsk0_...)时才会使用它;pattern 不含 PSK 时该值被静默忽略,而不是被拒绝
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
resume = true # 可选。Noise 会话恢复:重连时用 MAC 证明持有上一会话的握手哈希,而不是重做握手的密钥交换(选择器 0x02)。默认:false。见 `docs/transport.md`「Noise session resume」

[client.services.ssh] # A service to forward
protocol = "tcp" # Optional. Possible values: ["tcp", "udp"]. Default: "tcp"
local_addr = "127.0.0.1:22" # Necessary. The address of the local service
nodelay = true # Optional. Per-service TCP_NODELAY override. Default: true
retry_interval = 1 # Optional. Override the global `client.control.default_retry_interval` per service
udp_forwarder_ipv6 = false # Optional. Prefer IPv6 for the UDP forwarder's connection to the local service (UDP services only)
remote_bind_addr = "0.0.0.0:5202"

[client.services.dns] # A UDP service example
protocol = "udp"
local_addr = "127.0.0.1:53"
remote_bind_addr = "0.0.0.0:53"
```

```toml
# Complete server configuration example.
# Every option is documented in the specification above.

[server]
default_token = "default_token_if_not_specify" # security-scan:allow documentation placeholder # Optional. Default token for services without their own
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["53", "5202"]

[server.control]
bind_addr = "0.0.0.0:2333" # Necessary. The address that the server listens for clients
heartbeat_interval = 30 # Optional. The interval between two application-layer heartbeats; set to 0 to disable. Default: 30 seconds

# Data-plane options (`[server.data]`) live here too; see the specification.
# They require the `multiplex` feature, which is part of the default build.

[server.transport] # Optional. Keys only - no `type`: the client decides whether a connection is encrypted (v3 selector byte); placing the keys lets the server accept Noise connections too
[server.transport.noise] # Keys for accepting Noise connections. See docs/transport.md
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # Optional. Default value as shown
local_private_key = "key_encoded_in_base64" # Optional
remote_public_key = "key_encoded_in_base64" # Optional
psk = "key_encoded_in_base64" # 可选。预共享密钥,base64 编码后必须恰好解码为 32 字节,该长度只在建立连接的 Noise 握手时才检查。仅当配置的 `pattern` 在 `psk_location` 处带有 PSK 修饰符(如 Noise_KKpsk0_...)时才会使用它;pattern 不含 PSK 时该值被静默忽略,而不是被拒绝
psk_location = 0 # Optional. The PSK slot index used in the pattern. Default: 0
resume = true # 可选。Noise 会话恢复:重连时用 MAC 证明持有上一会话的握手哈希,而不是重做握手的密钥交换(选择器 0x02)。默认:false。见 `docs/transport.md`「Noise session resume」
```

### Noise(加密传输)

用 `molehill --genkey` 生成密钥对,把服务端公钥放到客户端、服务端私钥
放到服务端(见[传输层文档](./transport.md)):

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.transport]
type = "noise"

[client.transport.noise]
remote_public_key = "xrpknQcAagcd/b9foMwxSCD+EindWxq450NEONk8XQo="

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

```toml
# server.toml
[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.transport.noise]
local_private_key = "QLYMByBnjgM254zT6YKaBVvuAA61swyZfFxoA/SKZHM="
```

### UDP 服务

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.services.foo1]
protocol = "udp"
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

```toml
# server.toml
[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

### 服务端与客户端合并到一个文件

配置只含 `[client]` 或 `[server]` 之一时,molehill 自动判断模式;两者都
在时用命令行显式指定:

```toml
# config.toml - run: molehill --server config.toml  /  molehill --client config.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"

[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

### 通过代理连接

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "127.0.0.1:2333"

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"

[client.transport]
type = "plain"
proxy = "socks5://myuser:mypass@127.0.0.1:1080"
```

### iperf3 测试服务

同时以 TCP 和 UDP 转发本地 iperf3 服务:

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"

[client.services.iperf3-udp]
protocol = "udp"
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"

[client.services.iperf3-tcp]
protocol = "tcp"
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

```toml
# server.toml
[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

## 部署

### systemd

把 molehill 作为 systemd 服务运行,支持 root 与 rootless,以及多实例。
单元名中 `molehills` 代表 `molehill --server`,`molehillc` 代表
`molehill --client`,`molehill` 是自动判断模式。单元名里的 `@` 表示按
配置文件实例化。配置文件建议权限 `600`(内含共享 token)。

```ini
# molehills@.service - 每个配置一个服务端实例:systemctl enable molehills@app1 --now
[Unit]
Description=Molehill Server Service (%i)
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill -s /etc/molehill/%i.toml
# without root
# ExecStart=%h/.local/bin/molehill -s %h/.local/etc/molehill/%i.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehills.service - 单服务端实例
[Unit]
Description=Molehill Server Service
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill -s /etc/molehill/molehill.toml
# without root
# ExecStart=%h/.local/bin/molehill -s %h/.local/etc/molehill/molehill.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehillc@.service - 每个配置一个客户端实例:systemctl enable molehillc@app1 --now
[Unit]
Description=Molehill Client Service (%i)
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill -c /etc/molehill/%i.toml
# without root
# ExecStart=%h/.local/bin/molehill -c %h/.local/etc/molehill/%i.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehillc.service - 单客户端实例
[Unit]
Description=Molehill Client Service
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill -c /etc/molehill/molehill.toml
# without root
# ExecStart=%h/.local/bin/molehill -c %h/.local/etc/molehill/molehill.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehill@.service - 自动判断模式,每个配置一个实例
[Unit]
Description=Molehill Service (%i)
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576

# with root
ExecStart=/usr/bin/molehill /etc/molehill/%i.toml
# without root
# ExecStart=%h/.local/bin/molehill %h/.local/etc/molehill/%i.toml

[Install]
WantedBy=multi-user.target
```

root 方式(假设 `molehill` 在 `/usr/bin`,配置在
`/etc/molehill/app1.toml`):

```bash
sudo cp molehills@.service /etc/systemd/system/
sudo mkdir -p /etc/molehill        # 然后在里面创建 app1.toml
sudo systemctl daemon-reload
sudo systemctl enable molehills@app1 --now
```

rootless 方式(假设 `molehill` 在 `~/.local/bin`,配置在
`~/.local/etc/molehill/app1.toml`):先取消单元里 `%h` 那行 ExecStart 的
注释,然后:

```bash
mkdir -p ~/.config/systemd/user
cp molehills@.service ~/.config/systemd/user/
mkdir -p ~/.local/etc/molehill    # 然后在里面创建 app1.toml
systemctl --user daemon-reload
systemctl --user enable molehills@app1 --now
```

多实例:再加一个配置(`app2.toml`)并 `enable molehills@app2`(`molehillc@.service`
与 `molehill@.service` 同理)。

### 容器

官方镜像 `ghcr.io/niyueee/molehill:latest` 是 `scratch` 上的单个静态
musl 二进制(约 1.2 MiB),以非 root UID 1000 运行,**不含任何配置**——把
自己的 `server.toml` / `client.toml` 只读挂载到 `/app/server.toml`
(或 `/app/client.toml`),把文件名作为命令行参数传入。

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

镜像自带完整的默认特性集(`server`、`client`、`noise`、`hot-reload`、
`multiplex`、`kcp`),所以 `default_carrier = "kcp"` 不需要换镜像。想要可
复现的升级就固定 release tag(`ghcr.io/niyueee/molehill:v0.9.0`),而不是
用 `:latest`。

以 UID 1000 运行带来两个后果:

- 挂载进去的配置文件必须对 UID 1000 可读——`chmod 644`(或 `chown 1000`),
  否则容器会以权限错误退出。
- **host** 网络下进程无法绑定 1024 以下的端口(生效的是宿主机的
  `ip_unprivileged_port_start`,通常是 1024),所以所有 `remote_bind_addr`
  以及 control/data 监听端口都要 ≥ 1024。bridge 网络下容器自己的 netns
  通常允许低位端口,但通用的做法一样:容器端口保持高位,把特权宿主端口
  映射上去(`-p 80:8080`,配置里写
  `remote_bind_addr = "0.0.0.0:8080"`)。

Docker / Podman Compose(host 网络——Linux 下最简单;服务端需要暴露任意
服务端口):

```yaml
# compose.yaml - usage: docker compose up -d  (or: podman compose up -d)
services:
  molehill-server:
    image: ghcr.io/niyueee/molehill:latest
    container_name: molehill-server
    restart: unless-stopped
    network_mode: host
    environment:
      RUST_LOG: info
    volumes:
      - ./server.toml:/app/server.toml:ro
    command: server.toml

  molehill-client:
    image: ghcr.io/niyueee/molehill:latest
    container_name: molehill-client
    restart: unless-stopped
    network_mode: host
    environment:
      RUST_LOG: info
    volumes:
      - ./client.toml:/app/client.toml:ro
    command: client.toml
```

桥接网络变体(用于 Docker Desktop 的 macOS/Windows);此时客户端通过
compose 网络的 DNS 名访问服务端,因此 `client.toml` 里要写
`default_remote_addr = "molehill-server:2333"`:

```yaml
# compose.bridge.yaml - usage: docker compose -f compose.bridge.yaml up -d
services:
  molehill-server:
    image: ghcr.io/niyueee/molehill:latest
    container_name: molehill-server
    restart: unless-stopped
    environment:
      RUST_LOG: info
    volumes:
      - ./server.toml:/app/server.toml:ro
    command: server.toml
    ports:
      - "2333:2333"     # 控制通道与 TCP 数据面(客户端连接到这里)
      - "2333:2333/udp" # KCP 数据面,仅当服务使用 carrier = "kcp" 时需要
      - "5202:5202"     # 暴露的 SSH 服务

  molehill-client:
    image: ghcr.io/niyueee/molehill:latest
    container_name: molehill-client
    restart: unless-stopped
    environment:
      RUST_LOG: info
    volumes:
      - ./client.toml:/app/client.toml:ro
    command: client.toml
```

Podman Quadlet——`.container` 文件把镜像变成 systemd 服务(root:复制到
`/etc/containers/systemd/`,`daemon-reload`,`systemctl enable --now
molehill-server`;rootless:复制到 `~/.config/containers/systemd/`,用
`systemctl --user`,并把 `WantedBy=` 改成 `default.target`):

```ini
# molehill-server.container
[Unit]
Description=Molehill server (container)
After=network-online.target
Wants=network-online.target

[Container]
Image=ghcr.io/niyueee/molehill:latest
Volume=/etc/molehill/server.toml:/app/server.toml:ro
Network=host
Environment=RUST_LOG=info
Exec=server.toml

[Service]
Restart=always

[Install]
WantedBy=multi-user.target
```

```ini
# molehill-client.container
[Unit]
Description=Molehill client (container)
After=network-online.target
Wants=network-online.target

[Container]
Image=ghcr.io/niyueee/molehill:latest
Volume=/etc/molehill/client.toml:/app/client.toml:ro
Network=host
Environment=RUST_LOG=info
Exec=client.toml

[Service]
Restart=always

[Install]
WantedBy=multi-user.target
```

## 使用说明

### 网络要求

- **服务端**必须能从互联网访问:`server.control.bind_addr`、
  `server.data.bind_addr`(设置时)和每个注册的 `remote_bind_addr` 都需要
  入站访问(在防火墙中开放端口,或在公网服务器上做端口转发)。服务使用
  `carrier = "kcp"` 时还要额外开放对应的 **UDP** 端口——KCP 监听器绑定
  `server.data.bind_addr`,默认就是控制端口,同一端口号上 TCP 与 UDP 并存。
- **客户端**只需要到 `server.control.bind_addr` 的出站访问(数据端点不同时
  也包括它;TCP,`carrier = "kcp"` 时还包括 UDP);NAT 后不需要任何入站端口。
- 容器部署:镜像以 UID 1000 运行,无法绑定 1024 以下的端口——端口与配置
  文件权限的后果见[容器](#容器)。
- `client.control.default_remote_addr` 必须与 `server.control.bind_addr` 使用相同的
  端口,除非服务端改动了控制监听地址。

### 安全

- 共享 token 是强制的。使用长随机值。
- `allow_ports` 是你的授权边界:只列出客户端真正需要的端口。没有它,无论
  客户端请求什么,服务端都不会暴露任何东西。
- 配置文件以明文包含 token,请限制其权限(如 `chmod 600 config.toml`)。
  token 在日志中被掩码显示(`MASKED`)。
- 流量穿越不受信任的网络时使用 `noise` 传输;明文 `plain` 是不加密转发的。
- Noise 私钥同样是机密。

### 心跳

- `client.control.default_heartbeat_timeout` 必须大于
  `server.control.heartbeat_interval`,否则客户端会把健康的服务端当成已死,
  陷入重连循环。
- 设置 `server.control.heartbeat_interval = 0` 可禁用心跳(同时也要设置
  `client.control.default_heartbeat_timeout = 0`)。

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
  `[client.services.<name>]` 中删除。仍带该键的配置在本版本中会正常启动并
  输出一条警告;从下一个版本起该键会成为错误。

### UDP 服务

- 数据报大小上限遵循服务的 `udp_buffer_size`(默认 2048 字节,最大
  65535);更大的数据报被丢弃,通道仍可用。在服务上配置相同的值,并注意
  服务端在注册时会执行自己收到的那份副本的限制。
- **会话亲和**:来自一个访客地址的所有数据报走同一条数据通道,并在访客的
  整个会话期间通过客户端上一个专用本地 socket 离开——有状态 UDP 服务
  (Minecraft Bedrock/RakNet、QUIC、WireGuard 等游戏服务器)会看到稳定的
  `(ip, port)`,会话保持完整。`pool_size` 把*不同的访客*分片到不同通道以
  并行;绝不会把同一个访客拆到多个通道。
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
  使用不同的端口,并使用独立的配置文件([systemd 单元](#systemd)展示了模板化实例)。

## 故障排查

| 现象 | 原因 / 修复 |
|---|---|
| `Server rejected service <name>: Port N rejected ... allow_ports` | 请求的 `remote_bind_addr` 端口未在服务端白名单中,或服务端禁用了动态注册。修复 `allow_ports`。 |
| `Port N is already in use` | 服务端上另一个服务(或程序)占用了该端口。换一个 `remote_bind_addr` 端口。 |
| `Protocol version mismatched ... Please update` | 一端运行的是旧版 molehill。两端一起升级(协议 v3 自 0.8 起;v2 自 0.7.0 起)。 |
| 客户端出现 `Authentication failed` | 客户端与服务端的 `default_token` 不一致。 |
| `Failed to connect to <addr>: Connection refused` | 服务端未运行、`client.control.default_remote_addr` 端口错误,或 `server.control.bind_addr` 不可达。 |
| 配置能启动,但连接时报地址解析错误(`failed to lookup address information`) | 这些地址键只检查字符串里有没有 `:`,并不按 socket 地址解析:`client.control.default_remote_addr`、`client.services.<name>.remote_addr`、`client.data.default_data_addr`、`server.data.bind_addr`。因此裸 IPv6 字面量(如 `"::1"`)能通过启动校验,却没有端口,解析地址时才会失败。始终写 `主机:端口`,IPv6 字面量要加方括号——`"[::1]:2333"`。(服务的 `remote_bind_addr` 反而会按 `SocketAddr` 解析,启动时就会拒绝。) |
| 反复出现 `Heartbeat timed out` | `client.control.default_heartbeat_timeout <= server.control.heartbeat_interval`,或网络路径丢弃了连接。 |
| Noise 握手失败 | 两端的密钥对、`psk` 或 pattern 不匹配。 |
| 启动时 `Proxy URL is missing the port` | `proxy` URL 缺少端口;修复配置。 |
| UDP 流量不通 | 检查 `protocol = "udp"`;大于 `udp_buffer_size` 的数据报会被丢弃;空闲映射在 `udp_idle_timeout` 秒后超时。 |
| 有状态 UDP 会话(游戏、QUIC、WireGuard)中途断开 | 确保两端运行带 UDP 会话亲和的版本(≥ 本修复);空闲超过 `udp_idle_timeout` 的对端会在下一个数据报时重新绑定到新本地 socket(源端口变化)——调大超时或发送周期流量。 |
| `Failed to read cmd: early eof` 警告 | 对端关闭了通道(重启或关停);客户端会自动重连。 |
