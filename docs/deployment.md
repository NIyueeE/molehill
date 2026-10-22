# Deployment & examples

These are ready-to-run starting points for putting molehill on a host: a
configuration per common scenario, then the systemd, container and network
material around them. Every key they use is specified in
[Configuration](./configuration.md), which owns what each setting means, its
default and its allowed values.

- [Worked examples](#worked-examples)
- [Transparent services](#transparent-services)
- [systemd](#systemd)
- [Container](#container)
- [Network requirements](#network-requirements)
- [Security](#security)

## Worked examples

Every block below parses with the current binary, enforced by the config
test suite. The data-plane tables (`[client.data]`, `[server.data]`) require
the `multiplex` feature, which is part of the default build. A process runs
one mode (`[client]`, `[server]` or `[transparent]`); the data plane's shape
follows the service type and its carrier is chosen at runtime, so neither is a
per-service key
([Configuration](./configuration.md#migrating-the-data-planes-shape-is-no-longer-a-key)).

### Minimal

A minimal client and server pair:

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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

# Required: the control channel is always Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
```

### Noise (encrypted transport)

Generate a keypair with `molehill --genkey`, put the client's copy of the
server's public key on the client and the server's private key on the
server (see [Transport](./transport.md)):

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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
```

### UDP service

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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

# Required: the control channel is always Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
```

### Server and client in one file

molehill can determine the mode from the config when only one of
`[client]` / `[server]` is present; with both, pass the mode explicitly:

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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

# Required: the control channel is always Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
```

### Connect through a proxy

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

### iperf3 test services

Forward a local iperf3 server over both TCP and UDP:

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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

# Required: the control channel is always Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key
```

## Transparent services

A transparent (L3) client makes the **client** the owner of a public
`ip:port`: the server binds nothing, routes whole IP packets into the tunnel,
and the client's kernel answers the visitor. The keys, the prerequisites and
the exact refusals belong to
[Configuration](./configuration.md#transparent-l3-services); this section is the
network the operator has to build. Linux and macOS run both ends; Windows can
serve transparent clients but cannot be one (its stack has no source-address
policy routing). The daemon installs no route itself; the platform decides
which half is the operator's, as the table below states.

One sizing note before the recipes: a claim's inner flows are spread over its
lanes, and the lanes of a client's claims are lent between them as their flows
come and go — but only if the budget leaves a claim with something to spare
(`[transparent.data].tunnels`, at least two per claim). Measured, that is
worth a great deal when the *carrier connections* are the limit (a long-haul or
lossy path) and nothing on a fast, short one, at the cost of more connections
and more CPU; the numbers and the `lend = false` switch are
[Configuration](./configuration.md#transparent-l3-services) and
[Benchmarks](./benchmarks.md#what-each-configuration-choice-costs-per-decision-measurements).

The server serves L3 only if **its own** config asks for it: the
`[server.transparent]` table in `server.toml` below is that switch, and without
it the registration is refused before any device is touched. Run the L3 server
under a unit that has the capability, and an L4-only server under one that does
not (see [systemd](#systemd)).

The supported combinations are:

| | Linux | macOS | Windows |
|---|---|---|---|
| Transparent client | yes | yes | no — run it under WSL2 |
| Transparent server | yes | yes | yes (Wintun + Administrator) |
| Device | daemon-created TUN when missing; daemon assigns missing claim addresses | daemon-created `utunN`; daemon assigns the claimed `/32`s | daemon-created Wintun adapter |
| Routing | `ip route` / `ip rule` | `route` + pf `route-to` | `New-NetRoute` + forwarding |
| Privilege | `CAP_NET_ADMIN` | root | Administrator + `wintun.dll` |

The Linux recipes below are the reference shape; the macOS and Windows server
sections follow them.

Both Linux recipes start from the same two facts:

- **The claimed address is not a listener.** Nothing may bind it on the server;
  the server needs a route that hands its packets to the TUN device.
- **The client owns the address.** Its TUN device carries the claimed address,
  and a source rule sends the replies the local application emits back into the
  tunnel.
- **The TUN MTU is the packet-size lever — and, since v0.11.0, the smaller
  half of it.** A userspace data path pays per packet, so the MTU decides how
  many packets a given byte rate costs. Measured on one host, the same bulk
  flow through an L3 claim moved **1.84×** the throughput at an 8000-byte TUN
  MTU (with a link MTU to match) than at 1400, at **a third** of the CPU per
  byte — and at that packet size one L3 flow matches the forwarding path on
  the same host
  ([Benchmarks](./benchmarks.md#the-l3-data-path-what-limits-it-2026-10-10-this-model)).
  v0.11.0 narrows the gap at 1400 too: the daemon now attaches the device with
  `IFF_VNET_HDR` and declares its segmentation offloads, so the kernel hands
  the reader GSO super-frames (up-to-64 KiB units) instead of one 1400-byte
  packet per read — the packing that lever bought, taken by the binary. The
  MTU remains worth raising where the path between the two ends actually
  allows it (fewer per-syscall headers, less ACK traffic), but a 1400-byte TUN
  no longer caps a bulk flow at per-packet syscalls. The L3 path does not
  scale with the number of connections, so packet size — from either source —
  is still the lever that matters. The recipes below use 1400, which fits any
  path that carries the tunnel; raise it to what the path actually allows.

Both recipes end with the same configuration:

```toml
# server.toml - the last table is the switch: without it this server refuses
# transparent registrations by policy, and never opens a TUN device at all.
[server]
default_token = "change-me"
allow_ports = ["8443"]

[server.control]
bind_addr = "0.0.0.0:2333"

# Required: the control channel is always Noise
[server.noise]
public_key = "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=" # example key # security-scan:allow documentation example key
private_key = "kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE=" # example key # security-scan:allow documentation example key

[server.transparent]
tun = "molehill0"
```

```toml
# client.toml - the whole process is the L3 client: no forwarding services,
# nothing to dial, one claim.
[transparent]
default_token = "change-me"
tun = "molehill0"

[transparent.control]
default_remote_addr = "<server-address>:2333"

# Required: the control channel is always Noise
[transparent.noise]
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE=" # example key # security-scan:allow documentation example key
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=" # example key # security-scan:allow documentation example key

[transparent.claims.web]
remote_bind_addr = "<public-ip>:8443"
```

The device names are per host, so the two `tun` values need not match.

The mode is selected by the flag and the block, not by `tun`: `tun` only names
the device. Start the two ends with the same binary:

```bash
# server: [server.transparent] is the L3 switch; --server runs it
sudo ./molehill --server server.toml
# client: [transparent] is the L3 model; --transparent (or -t) runs it
sudo ./molehill --transparent client.toml
```

A `[transparent]` config started with `--client` is refused; a server with
`[server.transparent]` still uses `--server`. The client needs
`CAP_NET_ADMIN`/`root` for its TUN; the server needs it only when its own
`[server.transparent]` table is present.

### Recipe A: an address routed to the server

The clean, NAT-free case: the upstream network routes the claimed address (a
routed /32, or an address out of a block delivered to this host) to the server,
and the address is **not** configured on any of its interfaces — so packets
addressed to it are forwarded, which is exactly what the TUN device needs. One
route is the whole server side.

```bash
# ---------- server ----------
# <public-ip> is routed to this host by the upstream; it is NOT on any interface
sudo ip tuntap add dev molehill0 mode tun
sudo ip link set molehill0 up mtu 1400
sudo ip route add <public-ip>/32 dev molehill0
# the visitor's packet arrives on the visitor-facing interface and is forwarded
sudo sysctl -w net.ipv4.ip_forward=1
# injected packets carry a source this stack does not expect on that device
sudo sysctl -w net.ipv4.conf.molehill0.rp_filter=0
sudo sysctl -w net.ipv4.conf.all.rp_filter=0

# ---------- client ----------
sudo ip tuntap add dev molehill0 mode tun
sudo ip link set molehill0 up mtu 1400
# the claimed address, and a policy that sends its replies back into the tunnel:
# matching on the source takes only the traffic this service emits
sudo ip addr add <public-ip>/32 dev molehill0
sudo ip rule add from <public-ip> lookup 100
sudo ip route add default dev molehill0 table 100
sudo sysctl -w net.ipv4.conf.molehill0.rp_filter=0
sudo sysctl -w net.ipv4.conf.all.rp_filter=0
```

Two decisions to check afterwards: on the server, `ip route get <public-ip>`
must name the TUN device; on the client, `ip route get <visitor-ip> from
<public-ip>` must as well.

### Recipe B: a single-IP server

When the claimed address is the server's **own** public IP, a route is not
enough: the kernel would deliver a packet addressed to one of its own addresses
locally before any later routing table is consulted. The operator therefore
marks the claimed port before the routing decision and gives the marked packets
a table of their own. Nothing is translated — the mark only selects a route —
and because the mark names the port, the server's own traffic is untouched.

```bash
# ---------- server ----------
sudo ip tuntap add dev molehill0 mode tun
sudo ip link set molehill0 up mtu 1400
sudo sysctl -w net.ipv4.ip_forward=1
sudo sysctl -w net.ipv4.conf.molehill0.rp_filter=0
sudo sysctl -w net.ipv4.conf.all.rp_filter=0
# The replies the client hands back carry this host's own address as their
# source, so the kernel would refuse to forward them as martians: the tunnel
# is the interface that must accept a local source.
sudo sysctl -w net.ipv4.conf.molehill0.accept_local=1

# Mark the claimed port before the routing decision (iptables; with nftables the
# same rule ends in `meta mark set 0x1`). Mark UDP as well if the service uses
# it. ICMP addressed to <public-ip> itself is still answered by the server.
sudo iptables -t mangle -A PREROUTING -d <public-ip> -p tcp --dport 8443 -j MARK --set-mark 0x1

# A marked packet must reach table 100 before the kernel's local table
# (priority 0) can deliver it locally, so the local lookup moves below the
# mark rule; everything unmarked still resolves through it as before.
sudo ip rule del pref 0
sudo ip rule add pref 200 lookup local
sudo ip rule add pref 100 fwmark 0x1 lookup 100
sudo ip route add default dev molehill0 table 100
```

The client side is recipe A's **plus one rule**, because the address it claims
is the server's own public IP: once the client assigns `<public-ip>` to its
tunnel, the kernel treats that address as local — including the client's own
control channel to `<server-ip>:2333`, which is the same address and would be
delivered back to this host instead of leaving by the gateway.

```bash
# ---------- client ----------
sudo ip tuntap add dev molehill0 mode tun
sudo ip link set molehill0 up mtu 1400
sudo ip addr add <public-ip>/32 dev molehill0
sudo ip rule add from <public-ip> lookup 100
sudo ip route add default dev molehill0 table 100
sudo sysctl -w net.ipv4.conf.molehill0.rp_filter=0
sudo sysctl -w net.ipv4.conf.all.rp_filter=0

# The claimed address is the server's own, so the local table would answer
# for it: demote that lookup and send this host's own traffic to the server
# through the main table. Packets the server delivers arrive on the tunnel
# (iif molehill0), so they still reach the demoted local lookup — only
# locally generated traffic is redirected.
sudo ip rule add pref 200 lookup local
sudo ip rule del pref 0
sudo ip rule add pref 100 iif lo to <server-ip> lookup main
```

Afterwards, the server's `ip route get <public-ip> mark 0x1` must name the TUN
device and its `ip rule show` must list the mark rule above the local lookup;
the client's `ip rule show` must list the `iif lo` rule above the demoted local
lookup, and its control channel must connect.

### IPv6 claims

The same recipes apply with `/128` addresses and the `-6` tools. The claim is
written in brackets:

```text
[transparent.claims.web]
remote_bind_addr = "[2001:db8:99::1]:8443"
```

```bash
# Linux server
sudo sysctl -w net.ipv6.conf.all.forwarding=1
sudo ip -6 route add 2001:db8:99::1/128 dev molehill0

# Linux client
sudo ip -6 addr add 2001:db8:99::1/128 dev molehill0
sudo ip -6 rule add from 2001:db8:99::1 lookup 100
sudo ip -6 route add default dev molehill0 table 100
```

macOS uses `ifconfig utunN inet6 <address> prefixlen 128`, an `inet6` pf
`route-to` rule, `net.inet6.ip6.forwarding=1` and `route add -inet6
<address>/128 -interface utunN`. A Windows server uses a `/128`
`New-NetRoute` and `Set-NetIPInterface -AddressFamily IPv6 -Forwarding
Enabled`.

The upstream network must route the claimed `/128` to the server, exactly as
for IPv4; the daemon does not proxy NDP and does not install the route. IPv6
jumbograms are not carried. The data path itself is family-agnostic: a claim
can be IPv4 or IPv6, and the server/client negotiate the same `Ip`/`Vnet`
frame format for either.

### Device and address automation on Linux

Two steps of the recipes are the daemon's to take, and only those two: the
device and, on the client, the claimed address. A transparent service creates
the TUN device when it is missing (persistent, what `ip tuntap add` leaves),
brings it up with MTU 1400, and on the client assigns each claimed address
that is not already on it. An existing device is used as it is, so pre-create
it to choose another MTU. That is the same pair of steps the macOS daemon
takes unconditionally (a `utunN` and its claimed addresses) and the device
step the Windows daemon takes for its adapter, so the three stay symmetric.

Everything else stays the recipes above on every platform: the server's
forwarding switch, `rp_filter` and each claim's route (or the mark and policy
routing of recipe B), and the client's `from <claim>` rule with its table
route. Every step is idempotent and the state is left in place when the daemon
stops, so a restart, a reload or a second claim converges. When the daemon has
to create the device or assign an address, it needs `ip` on `PATH` and
`CAP_NET_ADMIN`.

Two consequences of the daemon creating the device: a setting that belongs to
the device at creation time comes from `net.ipv4.conf.default.*`, so set
`net.ipv4.conf.default.rp_filter=0` and `net.ipv4.conf.all.rp_filter=0` before
starting (the client refuses a device whose reverse-path filter is on), and
routes that name the device are applied after it appears — the same ordering
the macOS and Windows recipes already have. A device the daemon creates gets
MTU 1400; create it yourself before starting if the path wants another MTU,
and the daemon uses it as it is.

### macOS: a daemon-created utun, operator pf and forwarding

macOS has no persistent TUN device an operator can create and hand over: the
daemon opens `/dev/utun` and the kernel creates a `utunN`, which exists only
while the process holds it. The daemon therefore creates the device and assigns
each claimed `/32` to it; the operator owns the pf rule, the routes and
forwarding. Use a fixed `tun = "utunN"` when the pf rule is static.

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
# ---------- server (macOS) ----------
# The daemon creates utun7 on the first transparent registration.
sudo sysctl -w net.inet.ip.forwarding=1
# <public-ip> is routed to this host by the upstream; send it into the utun.
sudo route add -host <public-ip> -interface utun7

# ---------- client (macOS) ----------
# The daemon creates utun7 and assigns <public-ip>/32. The replies the local
# application emits from that address must return into the utun:
sudo tee /etc/pf.anchors/molehill >/dev/null <<'EOF'
pass out quick route-to (utun7) from <public-ip> to any
EOF
# Add these two lines to /etc/pf.conf, then reload:
#   anchor "molehill"
#   load anchor "molehill" from "/etc/pf.anchors/molehill"
sudo pfctl -f /etc/pf.conf
sudo pfctl -E
```

Check the decisions afterwards: on the server, `route -n get <public-ip>` names
the utun; on the client, `pfctl -sr` shows the `route-to` rule and a visitor's
reply leaves through the utun. The exact pf syntax can depend on the utun's
point-to-point peer: `route-to (utunN)` is the form that names only the
interface; validate it on the host before relying on it.

### Windows server: Wintun, a route and forwarding

A Windows server can serve Linux/macOS transparent clients. It needs
Administrator privileges and `wintun.dll` next to `molehill.exe` (or on
`PATH`); the daemon creates the adapter named by `tun`. Windows has no
source-address policy routing, so a Windows **client** is refused — run that
client under WSL2 instead.

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
# Run elevated. <uplink> is the visitor-facing interface.
New-NetRoute -DestinationPrefix "<public-ip>/32" -InterfaceAlias "molehill0" `
  -NextHop "0.0.0.0" -PolicyStore ActiveStore
Set-NetIPInterface -InterfaceAlias "molehill0" -Forwarding Enabled
Set-NetIPInterface -InterfaceAlias "<uplink>" -Forwarding Enabled
# IPv4 routing between interfaces:
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" `
  -Name IPEnableRouter -Value 1
```

Windows Firewall must allow the forwarded traffic; a rule for the claimed port
or for the forwarding profile is deployment-specific. Check the route with
`Find-NetRoute -RemoteIPAddress <public-ip>` (it must name the Wintun adapter)
before sending visitors.

A Windows **client** under WSL2 runs the Linux client and its backend inside
the WSL2 distribution: the claimed address lives on the WSL2 TUN, the server
tunnels the visitor's packets to it, and the local backend binds it. The WSL2
instance needs the same Linux prerequisites as the recipe above.

### What each run mode needs

The three modes ask the host for different things, and only one of them asks
for anything unusual:

| What is needed | `molehill --server` (forwarding) | `molehill --client` (forwarding) | An L3 pair (`--server` + `--transparent`) |
|---|---|---|---|
| `CAP_NET_ADMIN` / root / Administrator | no | no | **yes, both ends** — the transparent process opens a TUN device on Linux/macOS; Administrator + `wintun.dll` on Windows |
| A TUN device | no | no | Linux: **yes, both ends** — the daemon creates a missing one (MTU 1400) and uses an existing one as it is; macOS/Windows: the daemon creates it (utun/Wintun) |
| `net.ipv4.ip_forward = 1` | no | no | **yes, on a Linux server**; on macOS `net.inet.ip.forwarding`; on Windows interface forwarding + `IPEnableRouter` |
| `rp_filter = 0` on the TUN and `all` | no | no | **yes on Linux** — injected packets carry the visitor's source |
| Source policy routing | no | no | Linux `ip rule`; macOS pf `route-to`; **Windows cannot** (use WSL2) |
| Ports below 1024 | only if a service asks for one | no | only if a claim asks for one |

Two of those are worth stating precisely, because they are the ones a working
deployment fails on:

- **On Linux, a server that serves L3 — and only such a server — needs `CAP_NET_ADMIN`.**
  The capability is asked for by the `[server.transparent]` table, so a
  `molehill --server` whose config does not carry it runs without it and
  refuses L3 registrations by policy. A forwarding-only server needs nothing
  special, which is why the capability belongs to the unit that needs it
  (see [systemd](#systemd)).
- **On Linux, `ip_forward` is checked, not assumed.** With it off, the kernel drops
  every packet addressed to a claim before the daemon reads a byte: the
  process sees a healthy TUN and a silent device, which is the one failure
  mode that looks like nothing at all. The daemon refuses the first
  transparent registration with the exact `sysctl` line to run.

### Operational notes

- The `ip` and `sysctl` commands are runtime state. Make them persistent with
  your distribution's mechanism (a boot unit, a networkd/NetworkManager
  dispatcher, `sysctl.d` for the `rp_filter` keys); the daemon only verifies
  what it finds when the service starts.
- A device the daemon creates gets `mtu 1400`, and its refusal prints the same
  value; these recipes use 1400 too. Keep it at or below the path MTU: the
  client's kernel derives the MSS it advertises from the TUN device's MTU, and a
  tunnel MTU smaller than the path is what keeps a segment from being too large
  for it.
- `MOLEHILL_L3_STATS=1` (see
  [Configuration](./configuration.md#diagnostics-switches-opt-in)) prints the
  data path's counters once a second, which is how you see whether the two
  devices are actually moving packets.

## systemd

Run molehill as a systemd service, with root or rootless, including
multiple instances. In the unit names, `molehills` stands for
`molehill --server`, `molehillc` for `molehill --client`, and `molehill`
for the auto-detect mode. The `@` in a unit name instantiates it per config
file. Store config files with permission `600` (they contain the shared
token). A server that serves transparent services needs `CAP_NET_ADMIN` to
open its TUN device — and only such a server does: the capability is asked for
by the `[server.transparent]` table, so a `molehill --server` whose config does
not carry it runs without it and refuses L3 registrations by policy. Root has
the capability implicitly, and the units below carry the
`AmbientCapabilities=` line (commented out) for a unit that runs as another
user.

```ini
# molehills@.service - one server instance per config: systemctl enable molehills@app1 --now
[Unit]
Description=Molehill Server Service (%i)
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576
# Transparent services: the process opens a TUN device. Uncomment when this
# unit does not run as root.
# AmbientCapabilities=CAP_NET_ADMIN

# with root
ExecStart=/usr/bin/molehill -s /etc/molehill/%i.toml
# without root
# ExecStart=%h/.local/bin/molehill -s %h/.local/etc/molehill/%i.toml

[Install]
WantedBy=multi-user.target
```

```ini
# molehillc@.service - one client instance per config: systemctl enable molehillc@app1 --now
[Unit]
Description=Molehill Client Service (%i)
After=network.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
LimitNOFILE=1048576
# Transparent services: the process opens a TUN device. Uncomment when this
# unit does not run as root.
# AmbientCapabilities=CAP_NET_ADMIN

# with root
ExecStart=/usr/bin/molehill -c /etc/molehill/%i.toml
# without root
# ExecStart=%h/.local/bin/molehill -c %h/.local/etc/molehill/%i.toml

[Install]
WantedBy=multi-user.target
```

A `[transparent]` client is the same template with `--transparent` (`-t`)
instead of `-c`; `molehillc` is the forwarding client and refuses a
`[transparent]` config. The auto-detect `molehill@.service` also runs it when
the unit omits the mode flag. A transparent client needs `CAP_NET_ADMIN` for
its TUN, exactly like a transparent server.

Both units are templates; two variants cover the rest:

- **Single instance** — drop the `@` from the unit name and the `%i` from
  the paths, and point `ExecStart` at one fixed config: `molehills.service`
  / `molehillc.service`
  (`ExecStart=/usr/bin/molehill -s /etc/molehill/molehill.toml`).
- **Auto-detect mode** — `molehill@.service` omits `-s` / `-c` and lets the
  binary detect the mode from the config content
  (`ExecStart=/usr/bin/molehill /etc/molehill/%i.toml`).

With root (assuming `molehill` in `/usr/bin` and configs under
`/etc/molehill/app1.toml`):

```bash
sudo cp molehills@.service /etc/systemd/system/
sudo mkdir -p /etc/molehill        # then create app1.toml inside
sudo systemctl daemon-reload
sudo systemctl enable molehills@app1 --now
```

Without root (assuming `molehill` in `~/.local/bin` and configs under
`~/.local/etc/molehill/app1.toml`): uncomment the `%h` ExecStart line in
the unit, then:

```bash
mkdir -p ~/.config/systemd/user
cp molehills@.service ~/.config/systemd/user/
mkdir -p ~/.local/etc/molehill    # then create app1.toml inside
systemctl --user daemon-reload
systemctl --user enable molehills@app1 --now
```

Multiple instances: add another config (`app2.toml`) and enable
`molehills@app2` (same for `molehillc@.service` and `molehill@.service`).

## Container

The official image `ghcr.io/niyueee/molehill:latest` is a single static
musl binary on `scratch` (~1.2 MiB), runs as non-root UID 1000 and contains
**no configuration** — mount your own `server.toml` / `client.toml`
read-only at `/app/server.toml` (or `/app/client.toml`) and pass its name
as the command-line argument.

```bash
docker run -v /etc/molehill/server.toml:/app/server.toml:ro \
  ghcr.io/niyueee/molehill:latest server.toml
```

The image carries the full default feature set (`server`, `client`, `noise`,
`hot-reload`, `multiplex`, `kcp`, `transparent`), so neither the data plane's
KCP sessions nor the transparent (L3) services need a different image. Pin a
release tag (`ghcr.io/niyueee/molehill:v0.11.0`) instead of `:latest` when you
want reproducible upgrades.

Two consequences of running as UID 1000:

- The mounted config must be readable by UID 1000 — `chmod 644` it (or
  `chown 1000`), otherwise the container exits with a permission error.
- Under **host** networking the process cannot bind ports below 1024 (the
  host's `ip_unprivileged_port_start`, normally 1024, applies), so every
  `remote_bind_addr` and the control/data listeners need ports ≥ 1024. Under
  bridge networking the container's own namespace usually allows low ports,
  but the portable recipe is the same: keep the container port high and map
  the privileged host port onto it (`-p 80:8080` with
  `remote_bind_addr = "0.0.0.0:8080"`).

Docker / Podman Compose (host networking — simplest on Linux; the server
must expose arbitrary service ports):

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

Bridge-network variant for Docker Desktop (macOS/Windows): keep the same
file, drop `network_mode: host` from both services, add a `ports:` list to
the server — `"2333:2333"` for the control channel and the TCP data plane,
`"2333:2333/udp"` for the KCP data plane (the carrier the data plane tries
first) and one entry per exposed service (e.g. `"5202:5202"`) —
and point the client at the compose DNS name with
`default_remote_addr = "molehill-server:2333"`.

Podman Quadlet — a `.container` file turns the image into a systemd
service (root: copy to `/etc/containers/systemd/`, `daemon-reload`,
`systemctl enable --now molehill-server`; rootless: copy to
`~/.config/containers/systemd/`, use `systemctl --user`, and change
`WantedBy=` to `default.target`):

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

The client variant is the same file with
`Description=Molehill client (container)`,
`Volume=/etc/molehill/client.toml:/app/client.toml:ro` and
`Exec=client.toml`.

## Network requirements

- The **server** must be reachable from the Internet: `server.control.bind_addr`, `server.data.bind_addr` (when set) and every registered `remote_bind_addr` need inbound access (open the ports in the firewall or port-forward them on the public server). Add the matching **UDP** port as well: the data plane tries KCP, so the KCP listener binds `server.data.bind_addr` — the control port by default — and TCP plus UDP coexist on that port number.
- The **client** only needs outbound access to `server.control.bind_addr` (and the data endpoint when it differs; TCP, plus UDP for the KCP data plane); no inbound port is required behind the NAT.
- Running in a container: the image runs as UID 1000 and cannot bind ports below 1024 — see [Container](#container) for the port and config-permission consequences.
- `client.control.default_remote_addr` must use the same port as `server.control.bind_addr` unless the server moved its control listener.

## Security

- The shared token is mandatory. Use long random values.
- `allow_ports` is your authorization boundary: only list what clients genuinely need. Without it, the server exposes nothing regardless of what clients request.
- The config file contains tokens in plain text, so restrict its permissions (e.g. `chmod 600 config.toml`). Tokens are masked (`MASKED`) in logs.
- Use the `noise` transport when traffic traverses untrusted networks; `plain` forwards unencrypted.
- Noise private keys are secrets too.

