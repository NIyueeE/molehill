# 部署与示例

本页是让 molehill 跑起来的开箱即用起点:每个常见场景一份配置,再加上围绕
它们的 systemd、容器与网络说明。这里用到的每个键都在
[配置文档](./configuration.zh.md)中规范——每个设置的含义、默认值与可选值
都由那篇页面负责。

- [完整示例](#完整示例)
- [透明(L3)服务](#透明l3服务)
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

## 透明(L3)服务

`protocol = "transparent"` 的服务让**客户端**成为公网 `ip:port` 的拥有者:
服务端不绑定任何东西,只把整个 IP 包路由进隧道,由客户端内核应答访客。各个键、
前置条件与确切的拒绝信息归
[配置文档](./configuration.zh.md#透明l3服务)负责;本节讲的是运维方要搭建的
网络。它仅支持 Linux,两端都需要 `CAP_NET_ADMIN`(各自要打开 TUN 设备),而且
自己不配置任何网络。

服务端只有在**它自己的**配置要求时才提供 L3:下面 `server.toml` 里的
`[server.transparent]` 就是那个开关,没有它,注册会在碰到任何设备之前被拒绝。
把提供 L3 的服务端放在带该能力的 unit 下,只做 L4 的服务端放在不带该能力的
unit 下(见 [systemd](#systemd))。

两套配方都基于同样两个事实:

- **所声明的地址不是监听器。** 服务端上没有任何东西可以绑定它;服务端需要一条
  把它交给 TUN 设备的路由。
- **地址归客户端所有。** 客户端的 TUN 设备承载所声明的地址,一条源地址规则把
  本地应用发出的回包送回隧道。

两套配方最终用同一份配置:

```toml
# server.toml - 最后一张表就是开关:没有它,本服务端会按策略拒绝透明注册,
# 也完全不会打开 TUN 设备。
[server]
default_token = "change-me"
allow_ports = ["8443"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.transparent]
tun = "molehill0"
```

```toml
# client.toml
[client]
default_token = "change-me"

[client.control]
default_remote_addr = "<server-address>:2333"

[client.transparent]
tun = "molehill0"

[client.services.web]
protocol = "transparent"
remote_bind_addr = "<public-ip>:8443"
```

设备名是按主机各自取的,因此两端的 `tun` 值不必相同。

### 配方 A:地址被路由到服务端

最干净、无 NAT 的情形:上游网络把所声明的地址(一条被路由的 /32,或交付给本
主机的一个地址块中的地址)路由到服务端,而且该地址**没有**配在服务端任何接口
上——于是发往它的包会被转发,这正是 TUN 设备需要的。服务端一侧只需要一条路由。

```bash
# ---------- 服务端 ----------
# <public-ip> 由上游路由到本主机;它不在任何接口上
sudo ip tuntap add dev molehill0 mode tun
sudo ip link set molehill0 up mtu 1400
sudo ip route add <public-ip>/32 dev molehill0
# 访客的包从面向访客的接口进来,再被转发出去
sudo sysctl -w net.ipv4.ip_forward=1
# 注入的包携带的源地址是本栈在该设备上不期望看到的
sudo sysctl -w net.ipv4.conf.molehill0.rp_filter=0
sudo sysctl -w net.ipv4.conf.all.rp_filter=0

# ---------- 客户端 ----------
sudo ip tuntap add dev molehill0 mode tun
sudo ip link set molehill0 up mtu 1400
# 所声明的地址,以及把这些回包送回隧道的策略:按源地址匹配,只影响该服务
# 自己发出的流量
sudo ip addr add <public-ip>/32 dev molehill0
sudo ip rule add from <public-ip> lookup 100
sudo ip route add default dev molehill0 table 100
sudo sysctl -w net.ipv4.conf.molehill0.rp_filter=0
sudo sysctl -w net.ipv4.conf.all.rp_filter=0
```

事后要核对两个路由决策:服务端上 `ip route get <public-ip>` 必须指向 TUN 设备;
客户端上 `ip route get <visitor-ip> from <public-ip>` 也必须如此。

### 配方 B:单 IP 服务端

当所声明的地址就是服务端**自己**的公网 IP 时,一条路由还不够:内核会在任何更靠
后的路由表被查询之前,就把发往本机地址的包本地投递。因此运维方在路由决策之前
给所声明的端口打上 mark,并让被标记的包走自己的表。这里没有任何地址转换——
mark 只用来选择路由——而且 mark 限定了端口,所以服务端自身的流量不受影响。

```bash
# ---------- 服务端 ----------
sudo ip tuntap add dev molehill0 mode tun
sudo ip link set molehill0 up mtu 1400
sudo sysctl -w net.ipv4.ip_forward=1
sudo sysctl -w net.ipv4.conf.molehill0.rp_filter=0
sudo sysctl -w net.ipv4.conf.all.rp_filter=0

# 在路由决策之前给所声明的端口打 mark(iptables;用 nftables 时同一条规则以
# `meta mark set 0x1` 结尾)。服务同时使用 UDP 时也要标记 UDP。发往
# <public-ip> 本身的 ICMP 仍由服务端应答——这是共用一个地址的代价。
sudo iptables -t mangle -A PREROUTING -d <public-ip> -p tcp --dport 8443 -j MARK --set-mark 0x1

# 被标记的包必须在核内的 local 表(优先级 0)把发往本机地址的包本地投递之前
# 到达表 100,因此把 local 查询移到 mark 规则之后;未标记的流量照旧走它。
sudo ip rule del pref 0
sudo ip rule add pref 200 lookup local
sudo ip rule add pref 100 fwmark 0x1 lookup 100
sudo ip route add default dev molehill0 table 100
```

客户端一侧与配方 A 完全相同:客户端声明的是谁的地址并不重要——它承载该地址,
应用绑定该地址。事后 `ip route get <public-ip> mark 0x1` 必须指向 TUN 设备,
`ip rule show` 里 mark 规则必须排在 local 查询之前。

### 运维注意事项

- 上面的 `ip` 与 `sysctl` 命令都只是运行时状态。请用发行版的机制让它们持久化
  (开机执行的单元、networkd/NetworkManager 的 dispatcher、`rp_filter` 两个键
  用 `sysctl.d`);守护进程只在服务启动时校验它看到的东西。
- `mtu 1400` 是守护进程自己的拒绝信息里给出的值,也是这两套配方使用的值。请把
  它保持在路径 MTU 或以下:客户端内核根据 TUN 设备的 MTU 推导自己要宣告的 MSS,
  隧道 MTU 小于路径 MTU 正是让分片不会超出路径的原因。
- `MOLEHILL_L3_STATS=1`(见
  [配置文档](./configuration.zh.md#诊断开关按需开启))每秒打印一次数据面的
  计数,这是判断两端设备是否真的在过包的办法。

## systemd

把 molehill 作为 systemd 服务运行,支持 root 与 rootless,以及多实例。
单元名中 `molehills` 代表 `molehill --server`,`molehillc` 代表
`molehill --client`,`molehill` 是自动判断模式。单元名里的 `@` 表示按
配置文件实例化。配置文件建议权限 `600`(内含共享 token)。提供透明服务的服务端
需要 `CAP_NET_ADMIN` 才能打开自己的 TUN 设备——也只有这种服务端需要:这项能力是
由 `[server.transparent]` 这张表索取的,所以配置里没有它的 `molehill --server`
不需要该能力运行,并按策略拒绝 L3 注册。root 隐含具备;以其他用户运行的
单元请使用下面单元里那行(默认注释掉的)`AmbientCapabilities=`。

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
# 透明服务:进程要打开 TUN 设备。本单元不以 root 运行时取消注释。
# AmbientCapabilities=CAP_NET_ADMIN

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
# 透明服务:进程要打开 TUN 设备。本单元不以 root 运行时取消注释。
# AmbientCapabilities=CAP_NET_ADMIN

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
`multiplex`、`kcp`、`transparent`),所以 `default_carrier = "kcp"` 不需要换
镜像。想要可复现的升级就固定 release tag(`ghcr.io/niyueee/molehill:v0.9.0`),
而不是用 `:latest`。

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

