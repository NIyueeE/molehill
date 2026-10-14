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
the `multiplex` feature, which is part of the default build, and a client
can mix modes because every service may override `mode`/`carrier` on its own
`[client.services.<name>]` block.

### Minimal

A minimal client and server pair:

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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"

[server.transport.noise]
local_private_key = "QLYMByBnjgM254zT6YKaBVvuAA61swyZfFxoA/SKZHM="
```

### UDP service

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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
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

[client.services.foo1]
local_addr = "127.0.0.1:80"
remote_bind_addr = "0.0.0.0:5202"

[server]
default_token = "123"
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

### Connect through a proxy

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

### iperf3 test services

Forward a local iperf3 server over both TCP and UDP:

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
# Master switch for dynamic registration: empty/missing = all registrations rejected
allow_ports = ["5202"]

[server.control]
bind_addr = "0.0.0.0:2333"
```

## Transparent services

A transparent (L3) client makes the **client** the owner of a public
`ip:port`: the server binds nothing, routes whole IP packets into the tunnel,
and the client's kernel answers the visitor. The keys, the prerequisites and
the exact refusals belong to
[Configuration](./configuration.md#transparent-l3-services); this section is the
network the operator has to build. It is Linux only, needs `CAP_NET_ADMIN` on
both ends (each side opens a TUN device), and configures nothing itself.

The server serves L3 only if **its own** config asks for it: the
`[server.transparent]` table in `server.toml` below is that switch, and without
it the registration is refused before any device is touched. Run the L3 server
under a unit that has the capability, and an L4-only server under one that does
not (see [systemd](#systemd)).

Both recipes start from the same two facts:

- **The claimed address is not a listener.** Nothing may bind it on the server;
  the server needs a route that hands its packets to the TUN device.
- **The client owns the address.** Its TUN device carries the claimed address,
  and a source rule sends the replies the local application emits back into the
  tunnel.
- **The TUN MTU is the packet-size lever.** A userspace data path pays per
  packet, so the MTU decides how many packets a given byte rate costs: the same
  bulk transfer at an 8000-byte TUN MTU (with a link MTU to match) measured
  1.8× the throughput of 1400-byte packets at half the CPU per byte
  ([Benchmarks](./benchmarks.md#the-transparent-l3-wire-question-the-acceptance-harness)).
  The recipes below use 1400, which fits any path that carries the tunnel; raise
  it to what the path between the two ends actually allows.

Both recipes end with the same configuration:

```toml
# server.toml - the last table is the switch: without it this server refuses
# transparent registrations by policy, and never opens a TUN device at all.
[server]
default_token = "change-me"
allow_ports = ["8443"]

[server.control]
bind_addr = "0.0.0.0:2333"

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

[transparent.claims.web]
remote_bind_addr = "<public-ip>:8443"
```

The device names are per host, so the two `tun` values need not match.

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

The client side is recipe A's, unchanged: it does not matter whose address the
client claims — it carries it, and its application binds it. Afterwards,
`ip route get <public-ip> mark 0x1` must name the TUN device, and `ip rule
show` must list the mark rule above the local lookup.

### Operational notes

- The `ip` and `sysctl` commands are runtime state. Make them persistent with
  your distribution's mechanism (a boot unit, a networkd/NetworkManager
  dispatcher, `sysctl.d` for the `rp_filter` keys); the daemon only verifies
  what it finds when the service starts.
- `mtu 1400` is the value the daemon's own refusal prints, and the value these
  recipes use. Keep it at or below the path MTU: the client's kernel derives the
  MSS it advertises from the TUN device's MTU, and a tunnel MTU smaller than the
  path is what keeps a segment from being too large for it.
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
`hot-reload`, `multiplex`, `kcp`, `transparent`), so `default_carrier = "kcp"`
needs no different image. Pin a release tag (`ghcr.io/niyueee/molehill:v0.9.0`)
instead of `:latest` when you want reproducible upgrades.

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
`"2333:2333/udp"` for the KCP data plane (only when a service uses
`carrier = "kcp"`) and one entry per exposed service (e.g. `"5202:5202"`) —
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

- The **server** must be reachable from the Internet: `server.control.bind_addr`, `server.data.bind_addr` (when set) and every registered `remote_bind_addr` need inbound access (open the ports in the firewall or port-forward them on the public server). Add the matching **UDP** port whenever a service uses `carrier = "kcp"` — the KCP listener binds `server.data.bind_addr`, i.e. the control port by default, and TCP plus UDP coexist on that port number.
- The **client** only needs outbound access to `server.control.bind_addr` (and the data endpoint when it differs; TCP, plus UDP for `carrier = "kcp"`); no inbound port is required behind the NAT.
- Running in a container: the image runs as UID 1000 and cannot bind ports below 1024 — see [Container](#container) for the port and config-permission consequences.
- `client.control.default_remote_addr` must use the same port as `server.control.bind_addr` unless the server moved its control listener.

## Security

- The shared token is mandatory. Use long random values.
- `allow_ports` is your authorization boundary: only list what clients genuinely need. Without it, the server exposes nothing regardless of what clients request.
- The config file contains tokens in plain text, so restrict its permissions (e.g. `chmod 600 config.toml`). Tokens are masked (`MASKED`) in logs.
- Use the `noise` transport when traffic traverses untrusted networks; `plain` forwards unencrypted.
- Noise private keys are secrets too.

