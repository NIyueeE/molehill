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
(`[client.data]`、`[server.data]`)需要 `multiplex` 特性,它属于默认构建。
一个进程只运行一种模式(`[client]`、`[server]` 或 `[transparent]`);数据面的
形态由服务类型决定,carrier 在运行时选择,两者都不是逐服务的键
(见[配置文档](./configuration.zh.md#迁移数据面的形态不再是键))。

### 最小配置

一对最小客户端与服务端:

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"


[client.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

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

# 必需:控制通道始终使用 Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
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


[client.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

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

[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
```

### UDP 服务

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"


[client.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

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

# 必需:控制通道始终使用 Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
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


[client.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"

[server]
default_token = "123"
# 动态注册的总开关:为空/缺失 = 拒绝所有注册
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

# 必需:控制通道始终使用 Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
```

### 通过代理连接

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "127.0.0.1:2333"


[client.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"
```

### iperf3 测试服务

同时以 TCP 和 UDP 转发本地 iperf3 服务:

```toml
# client.toml
[client]
default_token = "123"

[client.control]
default_remote_addr = "localhost:2333"


[client.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

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

# 必需:控制通道始终使用 Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
```

## 透明(L3)服务

透明(L3)客户端让**客户端**成为公网 `ip:port` 的拥有者:
服务端不绑定任何东西,只把整个 IP 包路由进隧道,由客户端内核应答访客。各个键、
前置条件与确切的拒绝信息归
[配置文档](./configuration.zh.md#透明l3服务)负责;本节讲的是运维方要搭建的
网络。Linux 与 macOS 两端都支持;Windows 可做服务端,但不能做客户端(它的
协议栈没有源地址策略路由)。守护进程自己不安装路由;哪一半属于运维方由平台决定,
见下表。

在进入配方之前先给一条容量建议:认领的内部流会摊在它的车道上,而同一客户端各认领的车道会
随流量起落互相出借——但前提是预算给某条认领留下了余量(`[transparent.data].tunnels`,
每条认领至少两条)。实测:当**载体连接**本身是瓶颈时(长途或丢包路径)收益很大,而在快速短
路径上什么也买不到,代价是更多连接与更多 CPU;数字与 `lend = false` 开关见
[配置文档](./configuration.zh.md)与[基准页](./benchmarks.zh.md)。

服务端只有在**它自己的**配置要求时才提供 L3:下面 `server.toml` 里的
`[server.transparent]` 就是那个开关,没有它,注册会在碰到任何设备之前被拒绝。
把提供 L3 的服务端放在带该能力的 unit 下,只做 L4 的服务端放在不带该能力的
unit 下(见 [systemd](#systemd))。

支持的组合如下:

| | Linux | macOS | Windows |
|---|---|---|---|
| 透明客户端 | 是 | 是 | 否 —— 请跑在 WSL2 里 |
| 透明服务端 | 是 | 是 | 是(Wintun + 管理员) |
| 设备 | 缺失时由守护进程创建持久 TUN,并配置缺失的认领地址 | 守护进程创建 `utunN`,并配置所声明的 `/32`/`/128` | 守护进程创建 Wintun 适配器 |
| 路由 | `ip route` / `ip rule` | `route` + pf `route-to` | `New-NetRoute` + 转发 |
| 权限 | `CAP_NET_ADMIN` | root | 管理员 + `wintun.dll` |

下面 Linux 的两套配方是基准形态;macOS 与 Windows 服务端的配方紧随其后。

两套配方都基于同样两个事实:

- **所声明的地址不是监听器。** 服务端上没有任何东西可以绑定它;服务端需要一条
  把它交给 TUN 设备的路由。
- **地址归客户端所有。** 客户端的 TUN 设备承载所声明的地址,一条源地址规则把
  本地应用发出的回包送回隧道。
- **TUN 的 MTU 是包大小的杠杆——自 v0.11.0 起,它降为其中较小的一半。** 用户态数据面按
  包付费,所以 MTU 决定同样的字节速率要花多少个包。同一台主机上实测:同一条穿过 L3 认领
  的 bulk 流,用 8000 字节的 TUN MTU(链路 MTU 相应匹配)吞吐是 1400 字节时的 **1.84 倍**,
  每字节 CPU 只有原来的**三分之一**——而且在这个包大小下,单条 L3 流与同一主机上的转发路径
  打平(见[基准测试](benchmarks.zh.md#l3-数据面瓶颈在哪2026-10-10本模型))。v0.11.0 把
  1400 字节下的差距也收窄了:守护进程现在以 `IFF_VNET_HDR` 附加设备并声明其分段 offload,
  内核随之把 GSO 超级帧(最大 64 KiB 的单元)整块交给读端,而不是每次读一个 1400 字节的包
  ——这个杠杆曾经买到的打包效果,现在由二进制自带。MTU 在两端之间路径确实允许时仍值得
  调大(更少的每系统调用包头、更少的 ACK 流量),但 1400 字节的 TUN 不再把 bulk 流锁死在
  逐包系统调用上。L3 路径不随连接数扩展,所以包大小——无论来自哪个来源——依然是关键的
  那个杠杆。下面的配方用 1400,它能塞进任何承载隧道的路径;请把它提到路径实际允许的值。

两套配方最终用同一份配置:

```toml
# server.toml - 最后一张表就是开关:没有它,本服务端会按策略拒绝透明注册,
# 也完全不会打开 TUN 设备。
[server]
default_token = "change-me"
allow_ports = ["8443"]

[server.control]
bind_addr = "0.0.0.0:2333"

# 必需:控制通道始终使用 Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key

[server.transparent]
tun = "molehill0"
```

```toml
# client.toml - 整个进程就是 L3 客户端:没有转发服务,没有要拨的地址,只有一条认领。
[transparent]
default_token = "change-me"
tun = "molehill0"

[transparent.control]
default_remote_addr = "<server-address>:2333"

# 必需:控制通道始终使用 Noise
[transparent.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

[transparent.claims.web]
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
# 客户端交回来的回复包,其源地址是本机自己的地址,内核会按 martian 拒绝转发:
# 必须让隧道这个接口接受本地源。
sudo sysctl -w net.ipv4.conf.molehill0.accept_local=1

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

### IPv6 认领

同一套配方,把地址换成 `/128`,命令加 `-6`。认领写成方括号形式:

```text
[transparent.claims.web]
remote_bind_addr = "[2001:db8:99::1]:8443"
```

```bash
# Linux 服务端
sudo sysctl -w net.ipv6.conf.all.forwarding=1
sudo ip -6 route add 2001:db8:99::1/128 dev molehill0

# Linux 客户端
sudo ip -6 addr add 2001:db8:99::1/128 dev molehill0
sudo ip -6 rule add from 2001:db8:99::1 lookup 100
sudo ip -6 route add default dev molehill0 table 100
```

macOS 用 `ifconfig utunN inet6 <address> prefixlen 128`、`inet6` 的 pf
`route-to` 规则、`net.inet6.ip6.forwarding=1` 与 `route add -inet6
<address>/128 -interface utunN`。Windows 服务端用 `/128` 的
`New-NetRoute` 与 `Set-NetIPInterface -AddressFamily IPv6 -Forwarding
Enabled`。

上游网络必须把所声明的 `/128` 路由到服务端,与 IPv4 相同;守护进程不做 NDP
代理,也不安装路由。IPv6 jumbogram 不承载。数据面与地址族无关:一条认领可以
是 IPv4 或 IPv6,两端对两种地址族协商同一套 `Ip`/`Vnet` 帧格式。

### Linux 的设备与地址自动化

配方里只有两步可以由守护进程代劳:设备,以及客户端上的认领地址。透明服务在设备
缺失时创建 TUN(持久设备,与 `ip tuntap add` 留下的状态一致)、用 MTU 1400 拉起,
并在客户端把尚未配置的认领地址配上去。已存在的设备原样使用,所以想指定别的 MTU
就先自己创建。这正是 macOS 守护进程无条件执行的那两步(`utunN` 与其认领地址),
也是 Windows 守护进程为 Wintun 适配器执行的那一步,三个平台因此对称。

其余一切在任何平台都仍是上面的配方:服务端的转发开关、`rp_filter` 与每条认领的
路由(或配方 B 的 mark 与策略路由),以及客户端的 `from <claim>` 规则与表路由。
每一步都是幂等的,守护进程退出时状态保留,所以重启、reload 或第二条认领都会收敛。
需要守护进程执行这些步骤时:`PATH` 上有 `ip`,以及 `CAP_NET_ADMIN`。

设备由守护进程创建带来两个后果:属于"创建时"的设备设置要写进
`net.ipv4.conf.default.*`——启动前设好 `net.ipv4.conf.default.rp_filter=0` 与
`net.ipv4.conf.all.rp_filter=0`,否则客户端会拒绝它刚创建设备;而引用设备的
路由要在设备出现之后再应用,这正是 macOS 与 Windows 配方已有的顺序。守护进程
创建的设备使用 MTU 1400;如果路径需要别的 MTU,请在启动前自己创建设备,守护进程
会原样使用。

### macOS:utun、pf 与转发

macOS 没有可由运维方创建并交给守护进程的持久 TUN 设备:守护进程打开
`/dev/utun`,内核创建 `utunN`,它只在进程持有期间存在。因此守护进程创建设备并
把每个所声明的 `/32`/`/128` 配置到它上面;pf 规则、路由与转发仍归运维方。pf 规则是
静态的时,请写固定的 `tun = "utunN"`。

```toml
# server.toml
[server]
default_token = "change-me"
allow_ports = ["8443"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key

[server.transparent]
tun = "utun7"
```

```toml
# client.toml
[transparent]
default_token = "change-me"
tun = "utun7"

[transparent.control]
default_remote_addr = "<server-address>:2333"

[transparent.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

[transparent.claims.web]
remote_bind_addr = "<public-ip>:8443"
```

```bash
# ---------- 服务端(macOS)----------
# 守护进程会在第一次透明注册时创建 utun7。
sudo sysctl -w net.inet.ip.forwarding=1
# <public-ip> 由上游路由到本机;把它送进 utun。
sudo route add -host <public-ip> -interface utun7

# ---------- 客户端(macOS)----------
# 守护进程创建 utun7 并配置 <public-ip>/32。本地应用从该地址发出的回包
# 必须回到 utun:
sudo tee /etc/pf.anchors/molehill >/dev/null <<'EOF'
pass out quick route-to (utun7) from <public-ip> to any
EOF
# 在 /etc/pf.conf 里加两行,然后重新加载:
#   anchor "molehill"
#   load anchor "molehill" from "/etc/pf.anchors/molehill"
sudo pfctl -f /etc/pf.conf
sudo pfctl -E
```

事后核对:服务端 `route -n get <public-ip>` 必须指向 utun;客户端
`pfctl -sr` 必须显示 `route-to` 规则,访客的回包从 utun 出去。具体的 pf 语法
可能取决于 utun 的点对点对端:`route-to (utunN)` 是只写接口的形式,上线前请在
该主机上验证。

### Windows 服务端:Wintun、路由与转发

Windows 服务端可以为 Linux/macOS 透明客户端提供服务。它需要管理员权限,以及
放在 `molehill.exe` 旁边(或 `PATH` 上)的 `wintun.dll`;守护进程创建 `tun`
指定的适配器。Windows 没有源地址策略路由,所以 Windows **客户端**会被拒绝——
请把客户端跑在 WSL2 里。

```toml
# server.toml
[server]
default_token = "change-me"
allow_ports = ["8443"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key

[server.transparent]
tun = "molehill0"
```

```powershell
# 以管理员运行。<uplink> 是面向访客的接口。
New-NetRoute -DestinationPrefix "<public-ip>/32" -InterfaceAlias "molehill0" `
  -NextHop "0.0.0.0" -PolicyStore ActiveStore
Set-NetIPInterface -InterfaceAlias "molehill0" -Forwarding Enabled
Set-NetIPInterface -InterfaceAlias "<uplink>" -Forwarding Enabled
# 接口之间的 IPv4 转发:
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" `
  -Name IPEnableRouter -Value 1
```

Windows 防火墙必须放行被转发的流量;具体规则取决于部署。发送访客流量之前用
`Find-NetRoute -RemoteIPAddress <public-ip>` 核对路由(必须指向 Wintun 适配器)。

WSL2 里的 Windows **客户端**在 WSL2 发行版内部运行 Linux 客户端与后端:所声明
的地址落在 WSL2 的 TUN 上,服务端把访客的包隧道送到它,本地后端绑定该地址。
WSL2 实例需要与上面 Linux 配方相同的前置条件。

### 每种运行模式需要什么

三种模式向主机索取的东西不同,只有一种需要额外的东西:

| 需要什么 | `molehill --server`(转发) | `molehill --client`(转发) | L3 两端(`--server` + `--transparent`) |
|---|---|---|---|
| `CAP_NET_ADMIN` / root / 管理员 | 否 | 否 | **是,两端都要** —— Linux/macOS 上透明进程要打开 TUN 设备;Windows 上需要管理员 + `wintun.dll` |
| TUN 设备 | 否 | 否 | Linux:**是,两端都要** —— 守护进程创建缺失的设备(MTU 1400),已存在则原样使用;macOS/Windows:守护进程创建(utun/Wintun) |
| `net.ipv4.ip_forward = 1` | 否 | 否 | **Linux 服务端上是**;macOS 上是 `net.inet.ip.forwarding`;Windows 上是接口转发 + `IPEnableRouter` |
| TUN 与 `all` 上的 `rp_filter = 0` | 否 | 否 | **仅 Linux 上是** —— 注入的包携带的是访客的源地址 |
| 源地址策略路由 | 否 | 否 | Linux `ip rule`;macOS pf `route-to`;**Windows 不支持**(用 WSL2) |
| 1024 以下的端口 | 只有服务要求时才需要 | 否 | 只有认领要求时才需要 |

其中两条值得说清楚,因为它们是部署真正会栽在上面的地方:

- **在 Linux 上,只有提供 L3 的服务端才需要 `CAP_NET_ADMIN`。** macOS 用 root,Windows 用管理员 + `wintun.dll`;这项 Linux 能力是由
  `[server.transparent]` 表索取的,所以配置里没有它的 `molehill --server`
  就没有这项能力,并按策略拒绝 L3 注册。只做转发的服务端什么都不需要,这正是
  能力应该跟着需要它的那个单元走的原因(见 [systemd](#systemd))。
- **在 Linux 上,`ip_forward` 是被检查的,不是被假设的。** 它关着的时候,内核会在守护进程
  读到一个字节之前就丢弃所有发往认领地址的包:进程看到的是一个健康的 TUN 和
  一个沉默的设备——这是唯一一个看起来什么都不像的失败形态。守护进程会在第一次
  透明注册时拒绝,并给出要执行的 `sysctl` 命令行。

### 运维注意事项

- 上面的 `ip` 与 `sysctl` 命令都只是运行时状态。请用发行版的机制让它们持久化
  (开机执行的单元、networkd/NetworkManager 的 dispatcher、`rp_filter` 两个键
  用 `sysctl.d`);守护进程只在服务启动时校验它看到的东西。
- 守护进程创建的设备使用 `mtu 1400`,它的拒绝信息也打印同一个值;这两套配方也用
  1400。请把它保持在路径 MTU 或以下:客户端内核根据 TUN 设备的 MTU 推导自己要宣告的
  MSS,隧道 MTU 小于路径 MTU 正是让分片不会超出路径的原因。
- `MOLEHILL_L3_STATS=1`(见
  [配置文档](./configuration.zh.md#诊断开关按需开启))每秒打印一次数据面的
  计数,这是判断两端设备是否真的在过包的办法。

## systemd

把 molehill 作为 systemd 服务运行,支持 root 与 rootless,以及多实例。
单元名中 `molehills` 代表 `molehill --server`,`molehillc` 代表
`molehill --client`,`molehill` 是自动判断模式。单元名里的 `@` 表示按
配置文件实例化。配置文件建议权限 `600`(内含共享 token)。提供透明服务的服务端
需要 `CAP_NET_ADMIN` 才能打开自己的 TUN 设备——也只有这种 Linux 服务端需要:这项能力是
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
`multiplex`、`kcp`、`transparent`),所以数据面的 KCP 会话和透明(L3)服务
都不需要换镜像。想要可复现的升级就固定 release tag
(`ghcr.io/niyueee/molehill:v0.11.0`),而不是用 `:latest`。

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
TCP 数据面,`"2333:2333/udp"` 用于 KCP 数据面(数据面会优先尝试的 carrier),
再加每个对外暴露的服务端口(如 `"5202:5202"`);
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
  数据面会尝试 KCP,因此还要额外开放对应的 **UDP** 端口——KCP 监听器绑定
  `server.data.bind_addr`,默认就是控制端口,同一端口号上 TCP 与 UDP 并存。
- **客户端**只需要到 `server.control.bind_addr` 的出站访问(数据端点不同时
  也包括它;TCP,数据面的 KCP 还包括 UDP);NAT 后不需要任何入站端口。
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

