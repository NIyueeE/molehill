# 配置

`molehill` 可以根据配置文件的内容自动判断以服务端还是客户端模式运行:如果
`[server]` 与 `[client]` 块只出现一个,就自动选择对应模式,如
[快速开始](../README.zh.md#快速开始)中的示例。

`[client]` 与 `[server]` 块也可以放在同一个文件里:此时在服务端运行
`molehill --server config.toml`,在客户端运行 `molehill --client config.toml`,
显式指定运行模式。

在阅读完整配置规范之前,建议先浏览[配置示例](../examples)熟悉配置格式。
覆盖全部选项、开箱即用的完整示例在 [examples/full](../examples/full/)。

加密与 `transport` 块的更多细节见[传输层文档](./transport.md)。

## 如何配置(v0.7+ 模型)

自 v0.7 起,**服务定义归客户端所有**,服务端只拥有策略:

- 客户端在自己的配置里声明每个要转发的服务——包括它应该暴露的公网地址
  (`remote_bind_addr`)。
- 服务端**没有**任何按服务的配置。客户端连接时在运行时注册服务;服务端
  在暴露任何端口前,都会用 `allow_ports` 白名单校验每次注册。
- 两端用一个共享密钥(`default_token`)鉴权。

典型配置步骤:

1. 选择传输层——`tcp`(明文)、`tls`、`noise` 或 `websocket`——并生成所需
   的密钥或证书(见[传输层文档](./transport.md))。
2. 编写 `server.toml`:`[server]` 只需 `bind_addr`、`default_token` 和
   `allow_ports` 白名单,仅此而已。
3. 编写 `client.toml`:`[client]` 配置 `remote_addr` + 相同的
   `default_token`,每个服务一个 `[client.services.<name>]` 块:
   `local_addr`(你的服务监听的地址)和 `remote_bind_addr`(公网端点)。
4. 先启动服务端,再启动客户端。两端都会持续运行;服务端不可达时客户端
   会自动重试。

> **从 ≤0.6 迁移**:删除整个 `[server.services.*]` 段;把每个服务的
> `bind_addr` 移入客户端的 `remote_bind_addr`;用 `default_token` 替换
> 按服务的 token;在服务端添加 `allow_ports`。两端必须一起升级
> (协议版本已变更)。

以下是完整的配置规范:

```toml
[client]
remote_addr = "example.com:2333" # 必填。服务端地址
default_token = "change-me" # 必填。必须与 `[server].default_token` 一致
heartbeat_timeout = 40 # 可选。设为 0 可禁用应用层心跳检测。取值必须大于 `server.heartbeat_interval`。默认:40 秒
retry_interval = 1 # 可选。重连服务端的间隔。默认:1 秒
prefer_ipv6 = false # 可选。解析远端地址时优先使用 IPv6。默认:false
mux = true # 可选。把每条数据通道多路复用到一条物理连接上(yamux)。编译进 `multiplex` 特性时默认开启(它是默认特性集的一部分)。设为 false 恢复每通道一条连接
mux_receive_window = 67108864 # 可选。每条隧道连接的 yamux 总接收窗口上限,单位字节。仅在 `multiplex` 下生效。默认:rust-yamux 默认值(1 GiB)
mux_max_streams = 512 # 可选。每条隧道连接的最大并发流数。仅在 `multiplex` 下生效。默认:512

[client.transport] # 整个块可选。指定使用哪种传输层
type = "tcp" # 可选。可选值:["tcp", "tls", "noise", "websocket"]。默认:"tcp"

[client.transport.tcp] # 可选。TCP socket 选项(同样适用于 `tls` 和 `noise` 传输)
proxy = "socks5://user:passwd@127.0.0.1:1080" # 可选。连接服务端时使用的代理。支持 `http` 和 `socks5`。
nodelay = true # 可选。是否启用 TCP_NODELAY(如适用),改善延迟但略微降低带宽。默认:true
keepalive_secs = 20 # 可选。设置 `tcp(7)` 中的 `tcp_keepalive_time`(如适用)。默认:20 秒
keepalive_interval = 8 # 可选。设置 `tcp(7)` 中的 `tcp_keepalive_intvl`(如适用)。默认:8 秒

[client.transport.tls] # 当 `type` 为 "tls" 时必填
trusted_root = "ca.pem" # 必填。为服务端证书签名的 CA 证书
hostname = "example.com" # 可选。客户端校验证书时使用的主机名。未设置时回退到 `client.remote_addr`

[client.transport.noise] # Noise 协议。进一步说明见 `docs/transport.md`
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # 可选。默认值如所示
local_private_key = "key_encoded_in_base64" # 可选
remote_public_key = "key_encoded_in_base64" # 可选
psk = "key_encoded_in_base64" # 可选。预共享密钥(32 字节,base64 编码)。pattern 必须包含 PSK 修饰符(如 Noise_KKpsk0_...)
psk_location = 0 # 可选。pattern 中使用的 PSK 槽位索引。默认:0

[client.transport.websocket] # 当 `type` 为 "websocket" 时必填
tls = true # 必填。设为 `true` 在 WebSocket 连接上启用 TLS(使用 `client.transport.tls` 的设置)。设为 `false` 使用明文 WebSocket。

[client.services.service1] # 需要转发的服务。名称标识该服务(显示在日志中)
type = "tcp" # 可选。需要转发的协议。可选值:["tcp", "udp"]。默认:"tcp"
local_addr = "127.0.0.1:1081" # 必填。需要被转发的本地服务地址
remote_bind_addr = "0.0.0.0:8081" # 必填。该服务在服务端暴露的公网地址。必须被服务端的 `allow_ports` 覆盖
nodelay = true # 可选。该服务数据通道的 TCP_NODELAY。默认:即使不设置也为 true;设为 `false` 关闭
retry_interval = 1 # 可选。重连服务端的间隔。默认:继承全局配置
prefer_ipv6 = false # 可选。按服务覆盖 `client.prefer_ipv6`
pool_size = 8 # 可选。预建立的数据通道数。默认:TCP 为 8,UDP 为 2。受服务端 `max_pool_size` 限制。对 UDP 而言,这会把不同的访客分片到不同通道;每个访客固定钉在一个通道上(会话亲和)
health_check = { type = "tcp", interval = 10, timeout = 3, max_failed = 1 } # 可选。仅 TCP 服务。探测本地服务,在其宕机期间从服务端移除(见下方"健康检查")

[client.services.service2] # 可以定义多个服务
type = "udp"
local_addr = "127.0.0.1:1082"
remote_bind_addr = "0.0.0.0:8082"
udp_buffer_size = 2048 # 可选。UDP 接收缓冲区,单位字节。默认:2048,最大 65535
udp_idle_timeout = 60 # 可选。客户端上空闲 UDP 对端映射被丢弃的秒数(其本地 socket——即本地服务看到的源端口——随之回收)。默认:60
udp_sendq_size = 1024 # 可选。每条数据通道的出站数据报队列大小。默认:1024

[server]
bind_addr = "0.0.0.0:2333" # 必填。服务端监听客户端连接的地址。通常只需改端口。
default_token = "change-me" # 必填。必须与 `[client].default_token` 一致
allow_ports = ["6000-6999", "8080"] # 启用动态注册的必填项。为空或缺失:拒绝所有注册。特权端口(<1024)必须显式列出
max_pool_size = 16 # 可选。应用于每个服务请求的 pool_size 的上限。默认:不限制
heartbeat_interval = 30 # 可选。两次应用层心跳之间的间隔。设为 0 禁用发送心跳。默认:30 秒
mux_receive_window = 67108864 # 可选。服务端每条隧道连接的 yamux 总接收窗口。仅在 `multiplex` 下生效。默认:rust-yamux 默认值(1 GiB)
mux_max_streams = 512 # 可选。服务端每条隧道连接的最大并发流数。仅在 `multiplex` 下生效。默认:512

[server.transport] # 同 `[client.transport]`
type = "tcp"

[server.transport.tcp] # 同客户端
nodelay = true
keepalive_secs = 20
keepalive_interval = 8

[server.transport.tls] # 当 `type` 为 "tls" 时必填
pkcs12 = "identity.pfx" # 必填。服务端证书与私钥的 pkcs12 文件
pkcs12_password = "password" # 必填。pkcs12 文件的密码

[server.transport.noise] # 同 `[client.transport.noise]`
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"
psk = "key_encoded_in_base64" # 可选。预共享密钥(32 字节,base64 编码)。pattern 必须包含 PSK 修饰符(如 Noise_KKpsk0_...)
psk_location = 0 # 可选。pattern 中使用的 PSK 槽位索引。默认:0

[server.transport.websocket] # 当 `type` 为 "websocket" 时必填
tls = true # 必填。设为 `true` 在 WebSocket 连接上启用 TLS(使用 `server.transport.tls` 的设置)。设为 `false` 使用明文 WebSocket。
```

## 动态服务注册

不再有 `[server.services.*]` 块。生命周期如下:

1. 客户端用 `default_token` 鉴权。
2. 对每个配置的服务,客户端发送 `RegisterService` 消息:名称、类型、
   `remote_bind_addr`、`pool_size`。
3. 服务端校验:
   - **白名单**:请求的端口必须被 `allow_ports` 覆盖;为空/缺失的
     `allow_ports` 会拒绝*每一次*注册(这也是完全禁用该特性的方式);
   - **特权端口**:低于 1024 的端口必须显式列出;
   - **冲突**:端口已被占用时,注册失败并返回 `Port already in use`。
4. 成功时服务端立即绑定该端口并开始转发。

被拒绝对该服务的本次运行是永久性的:客户端记录服务端返回的具体原因并
放弃,直到你修复配置或重启。服务名在同一服务端内必须唯一;重启的客户端
重新注册时会干净地接管。

## 多路复用(`multiplex` 特性)

`multiplex` 特性是默认特性集的一部分。`mux = true`(默认)时,每个服务在
注册后会额外打开**一条连接**——*隧道*——之后每条数据通道都变成隧道内的
一条 yamux 流。这消除了每条连接的 TCP + TLS/Noise 握手延迟,并在大量并发
访客下大幅减少 FD 占用。

- 决定权只在客户端(`mux = true/false`);服务端按连接自动适配。
- `mux = false` 恢复每通道一条连接的行为。
- `mux_receive_window` / `mux_max_streams` 限制每条隧道的缓冲与流数;
  不设置时使用合理的默认值。
- 编译时去掉该特性则完全移除这个选项,始终使用每通道一条连接。

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

## 调优

自 v0.4.7 起,molehill 默认在传输层连接上启用 TCP_NODELAY,现在转发服务
的每一段也都默认启用:数据通道两端、面向访客的 socket、以及客户端连接
本地服务的连接。这对延迟与交互式应用(SSH、rdp、Minecraft 服务器)有益,
但会略微降低带宽。

这些 socket 上也默认启用 TCP keepalive(空闲 20 秒、探测间隔 8 秒),因此
被 NAT 或中间设备静默丢弃的池化数据通道会被检测到,而不会发给访客。

如果带宽更重要,可以在每个服务上用 `nodelay = false` 关闭 TCP_NODELAY。

## 使用说明

### 网络要求

- **服务端**必须能从互联网访问:`server.bind_addr` 和每个注册的
  `remote_bind_addr` 都需要入站访问(在防火墙中开放端口,或在公网服务器上
  做端口转发)。
- **客户端**只需要到 `server.bind_addr` 的出站访问;NAT 后不需要任何入站
  端口。
- `client.remote_addr` 必须与 `server.bind_addr` 使用相同的端口。

### 安全

- 共享 token 是强制的。使用长随机值。
- `allow_ports` 是你的授权边界:只列出客户端真正需要的端口。没有它,无论
  客户端请求什么,服务端都不会暴露任何东西。
- 配置文件以明文包含 token,请限制其权限(如 `chmod 600 config.toml`)。
  token 在日志中被掩码显示(`MASKED`)。
- 流量穿越不受信任的网络时使用 `noise` 或 `tls` 传输;明文 `tcp` 是
  不加密转发的。
- PKCS#12 文件和 Noise 私钥同样是机密。

### 心跳

- `client.heartbeat_timeout` 必须大于 `server.heartbeat_interval`,否则
  客户端会把健康的服务端当成已死,陷入重连循环。
- 设置 `heartbeat_interval = 0` 可禁用心跳(同时也要设置
  `heartbeat_timeout = 0`)。

### 健康检查

- `health_check` 可选,仅支持 TCP 服务。它让客户端每 `interval` 秒
  (默认 10)探测 `local_addr` 一次,探测 `timeout` 秒(默认 3)。连续
  `max_failed` 次(默认 1)失败后服务被判定为不健康:其控制通道被丢弃,
  服务端停止提供服务,访客快速失败而不是被转发到已死的本地服务。一旦探测
  再次成功,客户端会自动重新注册该服务。
- 两种探测类型:`type = "tcp"`(默认)向服务打开一条 TCP 连接;
  `type = "http"` 向 `http_path`(默认 `/`)发送 HTTP GET,接受任何
  2xx/3xx 响应。
- 示例:`health_check = { type = "http", interval = 5, timeout = 2, max_failed = 3, http_path = "/healthz" }`。

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
- `health_check` 不适用于 UDP 服务。

### 传输层

传输层配置——TLS 证书(包括 rustls legacy-PBE 注意事项)、Noise 密钥对、
pattern 与 PSK,以及 WebSocket 传输——在[传输层文档](./transport.md)中
逐步说明:在上面的规范中选好 `type`,再跟着那篇指南做。

- **代理**:`proxy` 只作用于客户端到服务端的出站连接(控制通道、隧道、
  以及非多路复用的数据通道)。支持 `socks5` 和 `http`(CONNECT),可带
  基础认证。

### 热重载

- 运行中编辑客户端配置:一般性修改(传输层、地址、token、mux 设置)会重启
  实例;添加、删除或修改服务则无需重启即可生效(服务在现有控制通道上
  注销/重新注册)。
- 编辑期间保持文件有效——无效配置会在启动或重载时被拒绝,并继续运行
  之前的状态。

### 多服务与多实例

- 一个客户端配置可以转发多个服务(多个 `[client.services.<name>]` 块),
  多个客户端也可以连接同一个服务端。每个注册的服务名在同一服务端的所有
  客户端之间必须唯一。
- 在一台主机上运行多个独立的 molehill 对时,为 `bind_addr` 使用不同的
  端口和独立的配置文件(systemd 示例展示了模板化实例)。

## 故障排查

| 现象 | 原因 / 修复 |
|---|---|
| `Server rejected service <name>: Port N rejected ... allow_ports` | 请求的 `remote_bind_addr` 端口未在服务端白名单中,或服务端禁用了动态注册。修复 `allow_ports`。 |
| `Port N is already in use` | 服务端上另一个服务(或程序)占用了该端口。换一个 `remote_bind_addr` 端口。 |
| `Protocol version mismatched ... Please update` | 一端运行的是旧版 molehill。两端一起升级(协议 v2 自 0.7.0 起)。 |
| 客户端出现 `Authentication failed` | 客户端与服务端的 `default_token` 不一致。 |
| `Failed to connect to <addr>: Connection refused` | 服务端未运行、`remote_addr` 端口错误,或 `server.bind_addr` 不可达。 |
| 反复出现 `Heartbeat timed out` | `heartbeat_timeout <= heartbeat_interval`,或网络路径丢弃了连接。 |
| TLS `certificate verify failed` | `trusted_root`/`hostname` 不匹配、证书过期,或自签场景缺少 `trusted_root`。 |
| Noise 握手失败 | 两端的密钥对、`psk` 或 pattern 不匹配。 |
| 启动时 `Proxy URL is missing the port` | `proxy` URL 缺少端口;修复配置。 |
| UDP 流量不通 | 检查 `type = "udp"`;大于 `udp_buffer_size` 的数据报会被丢弃;空闲映射在 `udp_idle_timeout` 秒后超时。 |
| 有状态 UDP 会话(游戏、QUIC、WireGuard)中途断开 | 确保两端运行带 UDP 会话亲和的版本(≥ 本修复);空闲超过 `udp_idle_timeout` 的对端会在下一个数据报时重新绑定到新本地 socket(源端口变化)——调大超时或发送周期流量。 |
| `Failed to read cmd: early eof` 警告 | 对端关闭了通道(重启或关停);客户端会自动重连。 |
