# 部署与示例

本页是让 molehill 跑起来的开箱即用起点:每个常见场景一份配置,再加上围绕
它们的 systemd、容器与网络说明。这里用到的每个键都在
[配置文档](./configuration.zh.md)中规范——每个设置的含义、默认值与可选值
都由那篇页面负责。

- [完整示例](#完整示例)
- [systemd](#systemd)
- [容器](#容器)
- [网络要求](#网络要求)
- [安全](#安全)

## 完整示例

以下每个代码块都能被当前二进制解析,配置测试套件会持续校验。数据面表
(`[client.data]`、`[server.data]`)需要 `multiplex` 特性,它属于默认构建;
同一个客户端可以混用模式,因为每个服务都可以在自己的
`[client.services.<name>]` 块里覆盖 `mode`/`carrier`。

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
# 动态注册的总开关:为空/缺失 = 拒绝所有注册
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
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
# 动态注册的总开关:为空/缺失 = 拒绝所有注册
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
# 动态注册的总开关:为空/缺失 = 拒绝所有注册
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
# 动态注册的总开关:为空/缺失 = 拒绝所有注册
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
# 动态注册的总开关:为空/缺失 = 拒绝所有注册
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

## systemd

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

两个单元都是模板;其余用法由两个变体覆盖:

- **单实例**——去掉单元名里的 `@` 与路径里的 `%i`,把 `ExecStart` 指向一个
  固定配置文件:`molehills.service` / `molehillc.service`
  (`ExecStart=/usr/bin/molehill -s /etc/molehill/molehill.toml`)。
- **自动判断模式**——`molehill@.service` 不带 `-s` / `-c`,由二进制根据配置
  内容判断模式(`ExecStart=/usr/bin/molehill /etc/molehill/%i.toml`)。

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

## 容器

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

桥接网络变体(Docker Desktop,macOS/Windows):文件不变,两个服务都去掉
`network_mode: host`,并给服务端加上 `ports:`——`"2333:2333"` 用于控制通道与
TCP 数据面,`"2333:2333/udp"` 用于 KCP 数据面(仅当某个服务使用
`carrier = "kcp"` 时需要),再加每个对外暴露的服务端口(如 `"5202:5202"`);
客户端则通过 compose 的 DNS 名访问服务端,即
`default_remote_addr = "molehill-server:2333"`。

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

客户端变体就是同一个文件,把 `Description=Molehill client (container)`、
`Volume=/etc/molehill/client.toml:/app/client.toml:ro` 与
`Exec=client.toml` 换掉。

## 网络要求

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

## 安全

- 共享 token 是强制的。使用长随机值。
- `allow_ports` 是你的授权边界:只列出客户端真正需要的端口。没有它,无论
  客户端请求什么,服务端都不会暴露任何东西。
- 配置文件以明文包含 token,请限制其权限(如 `chmod 600 config.toml`)。
  token 在日志中被掩码显示(`MASKED`)。
- 流量穿越不受信任的网络时使用 `noise` 传输;明文 `plain` 是不加密转发的。
- Noise 私钥同样是机密。

