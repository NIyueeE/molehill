# 传输层

默认情况下,`molehill` 按原样转发流量(明文 TCP)。客户端的
`[client.transport]` 块支持两种类型——`plain` 与 `noise`——**由客户端
决定**:每条连接以 v3 传输选择器字节开头,服务端接受客户端说的任何一种
语言(服务端的 `[server.transport]` 块只放置 Noise 密钥,没有服务端侧的
`type`)。noise 对比明文的 benchmark 就是这份选择的价目表:见 README 的
配置指南。本文只讲 `noise`;`plain` 除默认值外无需任何配置。

## Noise 协议

[Noise 协议](http://noiseprotocol.org/noise.html)是轻量、易配置的传输加密
方式:一对 X25519 密钥对,不需要 PKI。

`molehill` 自带合理的默认配置;见[noise 示例](./configuration.zh.md#noise加密传输)。默认
pattern `Noise_NK_25519_ChaChaPoly_BLAKE2s` 对服务端进行认证,因此不再有
中间人(MITM)问题。

> **ring-accelerated 改变了什么?** 默认构建链接了 `snow` 的
> **ring-accelerated** resolver,于是 ChaCha20-Poly1305 数据路径——每个
> 加密字节的热路径——走 ring 的硬件分派实现:x86-64 上传输层实测约为
> 纯 Rust resolver 的 1.5 倍,端到端约 1.3 倍。pattern 的哈希(默认
> BLAKE2s,其他 pattern 用 SHA-256/SHA-512)只在一次性握手中运行,不影响
> 吞吐:所有 pattern 都走加速后的密码算法。线缆格式不变。

使用它需要一个 X25519 密钥对。

### 生成密钥对

运行 `molehill --genkey`,用默认的 X25519 算法生成密钥对(传 `x448` 可用
X448):

```sh
$ molehill --genkey
Private Key:
cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q=

Public Key:
GQYTKSbWLBUSZiGfdWPSgek9yoOuaiwGD/GIX8Z1kkE=
```

(警告:不要使用网上出现的密钥对,包括上面这一对)

服务端保留私钥用于标识自己,客户端保留公钥用于验证服务端:

```toml
# 客户端
[client.transport]
type = "noise"
[client.transport.noise]
remote_public_key = "GQYTKSbWLBUSZiGfdWPSgek9yoOuaiwGD/GIX8Z1kkE="

# 服务端(只有密钥,没有 `type`;是否加密由客户端选择决定)
[server.transport.noise]
local_private_key = "cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q="
```

### 按服务加密

客户端全局的 `[client.transport].type` 是每个服务的默认值,每个服务都可以
单独覆盖——包括自己的密钥,这正是多服务端场景需要的(每个服务端持有自己的
密钥对):

```toml
[client.transport]
type = "noise"            # 默认:所有服务加密
[client.transport.noise]
remote_public_key = "server-a-pub-key"

[client.services.ssh]     # 继承:用全局密钥加密

[client.services.bulk]
transport = { type = "plain" }   # 退出:明文

[client.services.region-b]       # 全局明文 + 本服务加密
remote_addr = "region-b.example.com:2333"
transport = { type = "noise", noise = { remote_public_key = "server-b-pub-key" } }
```

规则:`transport.type` 不设则跟随全局 `type`;`"noise"` 强制加密,`"plain"`
强制明文。有效传输为 Noise 的服务必须要有密钥——优先用自己
`transport.noise` 的密钥,否则用全局 `[client.transport].noise`;有效
Noise 但任何地方都没有密钥是启动错误。数据面跟随服务:TCP 隧道以及
(`carrier = "kcp"` 时的)Noise-over-KCP 包裹都用该服务的有效密钥。服务端
只需要放置自己的密钥(v3 选择器:它接受每条连接所说的语言)。

### 指定 pattern

默认 pattern 满足大多数场景,但其他 pattern 也有用武之地:

**无认证**(`Noise_XX_...`):加密流量但不提供认证,因此对 MITM 攻击
脆弱,但能抵抗嗅探与重放攻击。在无需担心 MITM 时使用:

```toml
[server.transport.noise]
pattern = "Noise_XX_25519_ChaChaPoly_BLAKE2s"

[client.transport.noise]
pattern = "Noise_XX_25519_ChaChaPoly_BLAKE2s"
```

**双向认证**(`Noise_KK_...`):两端互相认证:

```toml
[server.transport.noise]
pattern = "Noise_KK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "server-priv-key-here"
remote_public_key = "client-pub-key-here"

[client.transport.noise]
pattern = "Noise_KK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "client-priv-key-here"
remote_public_key = "server-pub-key-here"
```

### 预共享密钥

`psk` 与 `psk_location` 为握手增加预共享密钥。pattern 必须包含 PSK
修饰符(如 `Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s`),密钥必须是 32 字节
base64 编码,且两端使用相同的 `psk` 与 `psk_location`:

```toml
[server.transport.noise]
pattern = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s"
local_private_key = "server-priv-key-here"
remote_public_key = "client-pub-key-here"
psk = "the-same-32-byte-key-in-base64"
psk_location = 0

[client.transport.noise]
pattern = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s"
local_private_key = "client-priv-key-here"
remote_public_key = "server-pub-key-here"
psk = "the-same-32-byte-key-in-base64"
psk_location = 0
```

### 其他 pattern

要弄清该用哪个 pattern,请参考:

- [7.5. 交互式握手 pattern(基础)](https://noiseprotocol.org/noise.html#interactive-handshake-patterns-fundamental)
- [8. 协议名与修饰符](https://noiseprotocol.org/noise.html#protocol-names-and-modifiers)
