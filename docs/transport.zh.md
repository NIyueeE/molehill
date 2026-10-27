# 传输

控制通道永远是 Noise,而且只有一种握手:
**Noise_KK_25519_ChaChaPoly_BLAKE2s**。KK 双向认证——客户端知道服务端的静态
公钥,服务端也知道客户端的——所以中间人无法冒充任何一端,配置里也没有可选的
匿名套件。本页讲密钥、唯一的数据加密开关,以及按服务和 L3 的覆盖。已移除的表
见[配置](configuration.zh.md#迁移到-0112每条会话一个池)。

## Noise 协议

[Noise 协议](http://noiseprotocol.org/noise.html)是一种轻量、易配置的连接
加密方式:X25519 密钥,不需要 PKI。每次握手都是新的,因此每条连接派生出自己
的密钥。

### 生成密钥对

在每台主机上运行 `molehill --genkey`,它会打印 X25519 私钥与对应公钥:

`sh
$ molehill --genkey
Private Key:
cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q=

Public Key:
GQYTKSbWLBUSZiGfdWPSgek9yoOuaiwGD/GIX8Z1kkE=
`

(不要使用来自互联网的密钥对,包括上面这一对。)每台主机保留自己的私钥,写
**对方**的公钥。

`toml
# 服务端
[server.noise]
private_key = "服务端的私钥"
public_key = "客户端的公钥"

# 客户端
[client.noise]
private_key = "客户端的私钥"
public_key = "服务端的公钥"
data_enable = true
`

两个块里的两把密钥都必填,且都必须解码为 32 字节,否则守护进程拒绝启动并点
名该块与该键。同一对密钥也用于数据面,所以通常每台主机一对密钥。

### 固定握手

没有 `pattern` 键,没有 `psk` / `psk_location`,没有
`resume`,也没有拨号代理。所有构建都用 KK + ChaCha20-Poly1305 +
BLAKE2s;仍写这些键的配置会被拒绝,并给出替代写法(或说明没有替代)。BLAKE2s
只在一次性握手中运行;ChaCha20-Poly1305 数据路径由 ring 加速,每个会话相同。

### 控制通道与数据通道

**控制通道永远用客户端密钥对加密**。它发布数据通道认证所用的会话 nonce,明文
控制段等于把该凭证交给路径上的所有人;服务端会在连接到达时拒绝明文控制通道。

**数据通道**由 `data_enable` 决定:

- `true`(客户端默认)把每条 TCP 隧道、直连通道和 mhd 会话都包进同一个
  KK 记录流。两端为每个字节支付密码学 CPU,路径上学不到转发载荷的任何信息。
- `false` 让数据通道走明文。此时访客自己的 TLS/SSH 就是端到端保护,
  这一跳不再为第二次加密花 CPU。控制通道无论如何都是加密的。

`toml
[client.noise]
private_key = "客户端的私钥"
public_key = "服务端的公钥"
data_enable = false # 数据通道走明文;控制通道仍为 KK
`

服务端不需要开关:它在同一个监听器上接受选择 `data_enable = false` 的客户端的
明文数据通道,以及其他客户端的 Noise 数据通道。

### 按服务覆盖

转发服务可以覆盖密钥对(用另一套密钥连另一台服务器)和数据开关。未设置的字段
继承 `[client.noise]`:

`toml
[client.services.ssh]              # 继承客户端密钥对与 data_enable

[client.services.bulk]
[client.services.bulk.noise]
data_enable = false                # 这个服务的数据通道走明文

[client.services.region-b]
remote_addr = "region-b.example.com:2333"
[client.services.region-b.noise]
private_key = "这个客户端的另一把私钥"
public_key = "region-b 的公钥"
`

密钥对是会话的身份:端点相同、密钥对相同的服务共用一条控制连接,不同的密钥对
得到自己的会话。单靠 `data_enable` 永远不会拆分会话。

### 透明(L3)客户端

L3 客户端把同样的三个键写在 `[transparent.noise]` 下。它的控制通道是 KK;
lane 默认**明文**(`data_enable = false`),因为 lane 承载的是访客自己的
IP 包,访客的 TLS 就是端到端保护:

`toml
[transparent.noise]
private_key = "客户端的私钥"
public_key = "服务端的公钥"
data_enable = false

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"

[transparent.claims.web.noise]
data_enable = true                 # 只加密这个 claim 的 lane
`

claim 也可以覆盖密钥对去连第二台服务器;未设置的字段继承
`[transparent.noise]`。

### 迁移

旧的 `[client.transport]`、`[server.transport]` 与
`[transparent.transport]` 表分别移到 `[client.noise]`、
`[server.noise]` 与 `[transparent.noise]`;
`local_private_key` 变成 `private_key`,
`remote_public_key` 变成 `public_key`。现在的握手需要两端的密钥,
所以要在 `[server.noise]` 里补上客户端的公钥,在 `[client.noise]` 里
补上客户端的私钥。`type = "noise"` 变成不写 `data_enable`(默认
true);`type = "plain"` 变成 `data_enable = false`;
`[client.services.<name>.transport]` 变成
`[client.services.<name>.noise]`。
