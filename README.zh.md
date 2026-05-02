<p align="center">
  <img src="https://raw.githubusercontent.com/NIyueeE/molehill/main/assets/molehill.svg" width="81" height="81">
</p>

<h1 align="center">molehill</h1>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/NIyueeE/molehill.svg"></a>
  <img src="https://img.shields.io/github/v/release/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/badge/rust-1.95.0+-93450a.svg">
  <img src="https://github.com/NIyueeE/molehill/actions/workflows/ci.yml/badge.svg">
</p>
<p align="center">
  <img src="https://img.shields.io/github/stars/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/github/forks/NIyueeE/molehill.svg">
  <img src="https://img.shields.io/github/last-commit/NIyueeE/molehill.svg">
</p>

<p align="center">[rathole](https://github.com/rapiz1/rathole) 的社区维护 fork 版本。</p>

[English](README.md) | [简体中文](README-zh.md)

molehill，类似于 [frp](https://github.com/fatedier/frp) 和 [ngrok](https://github.com/inconshreveable/ngrok)，可以让 NAT 后的设备上的服务通过具有公网 IP 的服务器暴露在公网上。

<!-- TOC -->

- [molehill](#molehill)
  - [Features](#features)
  - [Quickstart](#quickstart)
  - [Configuration](#configuration)
    - [Logging](#logging)
    - [Tuning](#tuning)
  - [Planning](#planning)

<!-- /TOC -->

## Features

- **高性能** 具有更高的吞吐量，高并发下更稳定。
- **低资源消耗** 内存占用远低于同类工具。[二进制文件最小](docs/build-guide.md)可以到 **~500KiB**，可以部署在嵌入式设备如路由器上。
- **安全性** 每个服务单独强制鉴权。Server 和 Client 负责各自的配置。使用 Noise Protocol 可以简单地配置传输加密，而不需要自签证书。同时也支持 TLS。
- **热重载** 支持配置文件热重载，动态修改端口转发服务。HTTP API 正在开发中。

## Quickstart

一个全功能的 `molehill` 可以从 [release](https://github.com/NIyueeE/molehill/releases) 页面下载。或者 [从源码编译](docs/build-guide.md) **获取其他平台和最小化的二进制文件**。

`molehill` 的使用和 frp 非常类似，如果你有后者的使用经验，那配置对你来说非常简单，区别只是转发服务的配置分离到了服务端和客户端，并且必须要设置 token。

使用 molehill 需要一个有公网 IP 的服务器，和一个在 NAT 或防火墙后的设备，其中有些服务需要暴露在互联网上。

假设你在家里的 NAT 后面有一个 NAS，并且想把它的 ssh 服务暴露在公网上：

1. 在有一个公网 IP 的服务器上

创建 `server.toml`，内容如下，并根据你的需要调整。

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333" # `2333` 配置了服务端监听客户端连接的端口

[server.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # 用于验证的 token
bind_addr = "0.0.0.0:5202" # `5202` 配置了将 `my_nas_ssh` 暴露给互联网的端口
```

然后运行:

```bash
./molehill server.toml
```

2. 在 NAT 后面的主机（你的 NAS）上

创建 `client.toml`，内容如下，并根据你的需要进行调整。

```toml
# client.toml
[client]
remote_addr = "myserver.com:2333" # 服务器的地址。端口必须与 `server.bind_addr` 中的端口相同。
[client.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # 必须与服务器相同以通过验证
local_addr = "127.0.0.1:22" # 需要被转发的服务的地址
```

然后运行：

```bash
./molehill client.toml
```

3. 现在 `molehill` 客户端会连接运行在 `myserver.com:2333`的 `molehill` 服务器，任何到 `myserver.com:5202` 的流量将被转发到客户端所在主机的 `22` 端口。

所以你可以 `ssh myserver.com:5202` 来 ssh 到你的 NAS。

[Systemd examples](./examples/systemd) 中提供了一些让 `molehill` 在 Linux 上作为后台服务运行的配置示例。

## Configuration

如果只有一个 `[server]` 和 `[client]` 块存在的话，`molehill` 可以根据配置文件的内容自动决定在服务器模式或客户端模式下运行，就像 [Quickstart](#quickstart) 中的例子。

但 `[client]` 和 `[server]` 块也可以放在一个文件中。然后在服务器端，运行 `molehill --server config.toml`。在客户端，运行 `molehill --client config.toml` 来明确告诉 `molehill` 运行模式。

**推荐首先查看 [examples](./examples) 中的配置示例来快速理解配置格式**，如果有不清楚的地方再查阅完整配置格式。

关于如何配置 Noise Protocol 和 TLS 来进行加密传输，参见 [Transport](./docs/transport.md)。

下面是完整的配置格式。

```toml
[client]
remote_addr = "example.com:2333" # 必填。服务端地址
default_token = "default_token_if_not_specify" # 可选。服务的默认 token，当服务未单独设置时使用
heartbeat_timeout = 40 # 可选。设为 0 禁用应用层心跳检测。该值必须大于 `server.heartbeat_interval`。默认：40 秒
retry_interval = 1 # 可选。连接服务端失败后的重试间隔。默认：1 秒
prefer_ipv6 = false # 可选。解析远程地址时优先使用 IPv6。默认：false

[client.transport] # 整个块都是可选的。指定使用的传输协议
type = "tcp" # 可选。可选值：["tcp", "tls", "noise", "websocket"]。默认："tcp"

[client.transport.tcp] # 可选。TCP socket 选项（对 `tls` 和 `noise` 传输同样生效）
proxy = "socks5://user:passwd@127.0.0.1:1080" # 可选。连接服务端使用的代理。支持 `http` 和 `socks5`
nodelay = true # 可选。是否启用 TCP_NODELAY，启用可降低延迟但会减少带宽。默认：true
keepalive_secs = 20 # 可选。设置 `tcp_keepalive_time`。默认：20 秒
keepalive_interval = 8 # 可选。设置 `tcp_keepalive_intvl`。默认：8 秒

[client.transport.tls] # 必填（当 `type` 为 "tls" 时）
trusted_root = "ca.pem" # 必填。签署服务端证书的 CA 证书
hostname = "example.com" # 可选。客户端验证证书时使用的主机名。不设置则回退到 `client.remote_addr`

[client.transport.noise] # Noise 协议。详见 `docs/transport.md`
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s" # 可选。默认值如上所示
local_private_key = "key_encoded_in_base64" # 可选
remote_public_key = "key_encoded_in_base64" # 可选
psk = "key_encoded_in_base64" # 可选。预共享密钥（32 字节，base64 编码）。使用前 pattern 须包含 PSK 修饰符（如 Noise_KKpsk0_...）
psk_location = 0 # 可选。PSK 在 pattern 中的槽位索引。默认：0

[client.transport.websocket] # 必填（当 `type` 为 "websocket" 时）
tls = true # 必填。设为 `true` 启用 WebSocket 上的 TLS（使用 `client.transport.tls` 中的配置）。设为 `false` 使用普通 WebSocket。

[client.services.service1] # 需要转发的服务。服务名 `service1` 可任意修改，只要与服务端配置一致
type = "tcp" # 可选。需要转发的协议。可选值：["tcp", "udp"]。默认："tcp"
token = "whatever" # 必填（如果 `client.default_token` 未设置）
local_addr = "127.0.0.1:1081" # 必填。需要转发的服务地址
nodelay = true # 可选。为当前服务单独覆盖 `client.transport.nodelay`
retry_interval = 1 # 可选。连接服务端的重试间隔。默认：继承全局配置
prefer_ipv6 = false # 可选。为当前服务单独覆盖 `client.prefer_ipv6`

[client.services.service2] # 可以定义多个服务
local_addr = "127.0.0.1:1082"

[server]
bind_addr = "0.0.0.0:2333" # 必填。服务端监听客户端连接的地址。通常只需修改端口
default_token = "default_token_if_not_specify" # 可选
heartbeat_interval = 30 # 可选。应用层心跳发送间隔。设为 0 禁用。默认：30 秒

[server.transport] # 与 `[client.transport]` 相同
type = "tcp"

[server.transport.tcp] # 与客户端相同
nodelay = true
keepalive_secs = 20
keepalive_interval = 8

[server.transport.tls] # 必填（当 `type` 为 "tls" 时）
pkcs12 = "identity.pfx" # 必填。服务端证书和私钥的 pkcs12 文件
pkcs12_password = "password" # 必填。pkcs12 文件的密码

[server.transport.noise] # 与 `[client.transport.noise]` 相同
pattern = "Noise_NK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "key_encoded_in_base64"
remote_public_key = "key_encoded_in_base64"
psk = "key_encoded_in_base64" # 可选。预共享密钥（32 字节，base64 编码）。使用前 pattern 须包含 PSK 修饰符（如 Noise_KKpsk0_...）
psk_location = 0 # 可选。PSK 在 pattern 中的槽位索引。默认：0

[server.transport.websocket] # 必填（当 `type` 为 "websocket" 时）
tls = true # 必填。设为 `true` 启用 WebSocket 上的 TLS（使用 `server.transport.tls` 中的配置）。设为 `false` 使用普通 WebSocket。

[server.services.service1] # 服务名必须与客户端保持一致
type = "tcp" # 可选。与客户端 `[client.services.X.type]` 相同
token = "whatever" # 必填（如果 `server.default_token` 未设置）
bind_addr = "0.0.0.0:8081" # 必填。暴露服务的公网地址。通常只需修改端口
nodelay = true # 可选。与客户端相同

[server.services.service2]
bind_addr = "0.0.0.1:8082"
```

### Logging

`molehill`，像许多其他 Rust 程序一样，使用环境变量来控制日志级别。

支持的 Logging Level 有 `info`, `warn`, `error`, `debug`, `trace`

比如将日志级别设置为 `error`:

```shell
RUST_LOG=error ./molehill config.toml
```

如果 `RUST_LOG` 不存在，默认的日志级别是 `info`。

### Tuning

从 v0.4.7 开始, molehill 默认启用 TCP_NODELAY。这能够减少延迟并使交互式应用受益，比如 RDP，Minecraft 服务器。但它会减少一些带宽。

如果带宽更重要，比如网盘类应用，TCP_NODELAY 仍然可以通过配置 `nodelay = false` 关闭。

## Planning

- [ ] HTTP APIs for configuration
