# 传输层

默认情况下,`molehill` 按原样转发流量(明文 TCP)。可以通过不同的
`transport` 配置来加密流量。`[client.transport]` 与 `[server.transport]`
的 `type` 在两端必须一致。

## TLS

当你已经有证书(例如来自公共 CA 或 Let's Encrypt)时,TLS 是即插即用的
选择。参见[示例](../examples/tls)。

### 客户端

通常使用自签名证书,此时客户端需要信任 CA。`trusted_root` 是根 CA 证书
PEM 文件的路径。`hostname` 是客户端校验服务端证书时使用的主机名;它不必
与 `client.remote_addr` 相同。

```toml
[client.transport]
type = "tls"

[client.transport.tls]
trusted_root = "examples/tls/rootCA.crt"
hostname = "localhost"
```

省略 `trusted_root` 时使用系统证书库,适用于公共信任的证书。

### 服务端

服务端需要 PKCS#12 归档文件,可以用 openssl 生成:

```sh
openssl pkcs12 -export -out identity.pfx -inkey server.key -in server.crt -certfile ca_chain_certs.crt
```

参数:

- `-inkey`:服务端私钥
- `-in`:服务端证书
- `-certfile`:CA 证书

用自己的 CA 创建自签名证书不是一件小事;[tls 示例目录](../examples/tls)
里提供了参考脚本。

```toml
[server.transport]
type = "tls"

[server.transport.tls]
pkcs12 = "identity.pfx"
pkcs12_password = "password"
```

### Rustls 支持

`molehill` 提供可选的 `rustls` 支持;见[构建指南](build-guide.md)。一个
差异是:加载 PKCS#12 归档所用的 crate 只处理有限类型的 PBE 算法,因此
归档必须用 legacy(openssl 1.x)格式创建。使用 openssl 3 时加 `-legacy`:

```sh
openssl pkcs12 -export -out identity.pfx -inkey server.key -in server.crt -certfile ca_chain_certs.crt -legacy
```

## Noise 协议

[Noise 协议](http://noiseprotocol.org/noise.html)是 TLS 的轻量、易配置
替代品:不需要自签名证书即可保护连接。

`molehill` 自带合理的默认配置;见最小[示例](../examples/noise_nk)。默认
pattern `Noise_NK_25519_ChaChaPoly_BLAKE2s` 对服务端进行认证(相当于
配置正确的 TLS),因此不再有中间人(MITM)问题。

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

# 服务端
[server.transport]
type = "noise"
[server.transport.noise]
local_private_key = "cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q="
```

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

## WebSocket

`websocket` 传输把 molehill 协议封装在 WebSocket 之上,适合只允许
HTTP(S) 流量的环境。两端都设置 `type = "websocket"` 并配置对应块:

```toml
[client.transport.websocket] # 或 [server.transport.websocket]
tls = true # 必填。WebSocket 连接上的 TLS(使用上面的 TLS 设置);设为 false 使用明文 WebSocket
```
