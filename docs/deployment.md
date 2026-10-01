# Deployment & examples

These are ready-to-run starting points for putting molehill on a host: a
configuration per common scenario, then the systemd, container and network
material around them. Every key they use is specified in
[Configuration](./configuration.md), which owns what each setting means, its
default and its allowed values.

- [Worked examples](#worked-examples)
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

## systemd

Run molehill as a systemd service, with root or rootless, including
multiple instances. In the unit names, `molehills` stands for
`molehill --server`, `molehillc` for `molehill --client`, and `molehill`
for the auto-detect mode. The `@` in a unit name instantiates it per config
file. Store config files with permission `600` (they contain the shared
token).

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
`hot-reload`, `multiplex`, `kcp`), so `default_carrier = "kcp"` needs no
different image. Pin a release tag (`ghcr.io/niyueee/molehill:v0.9.0`)
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

