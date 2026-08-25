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

<!-- /TOC -->

## 特性

- **高性能** 具有更高的吞吐量，高并发下更稳定。
- **低资源消耗** 内存占用远低于同类工具。[二进制文件最小](docs/build-guide.md)可以到 **~500KiB**，可以部署在嵌入式设备如路由器上。
- **安全性** 每个服务单独强制鉴权。Server 和 Client 负责各自的配置。使用 Noise Protocol 可以简单地配置传输加密，而不需要自签证书。同时也支持 TLS。
- **热重载** 支持配置文件热重载，动态修改端口转发服务。

## 快速开始

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
token = "use_a_secret_that_only_you_know" # 用于验证客户端身份的 token，改为只有你知道的任意值
bind_addr = "0.0.0.0:5202" # `5202` 配置了将 `my_nas_ssh` 暴露给互联网的端口
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
remote_addr = "myserver.com:2333" # 服务器的地址，端口必须和 `server.bind_addr` 中的端口一致

[client.services.my_nas_ssh]
token = "use_a_secret_that_only_you_know" # 必须和服务端一致才能通过验证
local_addr = "127.0.0.1:22" # 需要被转发的服务地址
```

然后运行:

```bash
./molehill client.toml
```

3. 现在客户端会尝试连接服务器的 `myserver.com:2333`，任何访问 `myserver.com:5202` 的流量都会被转发到客户端的 `22` 端口。

这样你就可以通过 `ssh myserver.com:5202` 来 ssh 到你的 NAS。

如果想在 Linux 上把 `molehill` 作为后台服务运行，可以参考
[systemd 示例](./examples/systemd) 或
[容器示例](./examples/container)。

## 配置

`molehill` 会根据配置文件自动判断运行模式（server/client），也可以通过 `--server` / `--client` 强制指定。完整的配置规范、日志和调优选项见
[配置文档](./docs/configuration.md)。[示例配置](./examples) 覆盖了各种常见场景。

## 部署

### 二进制

从 [release 页面](https://github.com/NIyueeE/molehill/releases) 下载对应平台的预编译二进制，或者
[从源码编译](./docs/build-guide.md) 获取其他平台和最小化的二进制。

```bash
./molehill server.toml   # 在公网服务器上
./molehill client.toml   # 在 NAT 后的设备上
```

### systemd

[systemd 示例](./examples/systemd) 演示了如何把 molehill 作为 systemd 服务运行，包含 root 和 rootless 两种方式，以及多实例管理。

### 容器

官方多架构镜像（linux/amd64、linux/arm64）发布在
`ghcr.io/niyueee/molehill`。镜像是构建在 `scratch` 上的单个静态 musl 二进制（约 8 MiB），以非 root UID 1000 运行，并内置了 CA 证书用于 TLS 验证。

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

镜像内不包含任何配置——挂载你的配置文件，并把文件名作为参数传入。更多部署方式见
[容器示例](./examples/container)，包括 Docker Compose（`compose.yaml` / `compose.bridge.yaml`）和 Podman
Quadlet（`molehill-server.container` / `molehill-client.container`）。

## 文档

- [配置文档](./docs/configuration.md) — 完整的配置规范、日志和调优
- [传输层](./docs/transport.md) — TLS 和 Noise Protocol 配置
- [构建指南](./docs/build-guide.md) — 构建定制、rustls 支持、最小化二进制
- [内部原理](./docs/internals.md) — 控制通道和数据通道的工作原理
- [示例](./examples) — 常见场景的配置

计划中的工作和将来的设计文档统一收录在 [`HANDOFF.md`](./HANDOFF.md)。
