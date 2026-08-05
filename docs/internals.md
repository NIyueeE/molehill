# Internals

## Concepts

- **Service**: the entity whose traffic needs forwarding (e.g. an SSH server)
- **Server**: the publicly accessible host running molehill in server mode
- **Client**: the host behind NAT running molehill in client mode; it holds the services to be exposed
- **Visitor**: someone who connects to a service through the server
- **Control channel**: a TCP connection between the server and the client that carries control commands for one service
- **Data channel**: a TCP connection between the server and the client that carries the forwarded data for one service

## Startup

In client mode, molehill creates one control channel per configured service, all connecting to `server.bind_addr`. The server listens on `server.bind_addr` for control channels and binds one listener per service on its `bind_addr`.

## Authentication

When a control channel is established, the server challenges the client with a random nonce. The client responds with `sha256(token || nonce)`. A wrong token or an unknown service name makes the server close the channel; the client logs `Authentication failed` or `No such a service` and retries.

## Forwarding

When a visitor connects to a service's `bind_addr`, the server sends a `CreateDataChannel` command over the corresponding control channel. The client connects back, and the pair forms a data channel over which the visitor's bytes are copied bidirectionally.

To reduce connection latency, the server pre-creates data channels and keeps them in a pool (`TCP_POOL_SIZE = 8` for TCP services, `UDP_POOL_SIZE = 2` for UDP services). New data channels are requested on demand when the pool runs empty.

## UDP

UDP services are forwarded over the same TCP data channels, framed with a small header (source address + length). On the client side, each visitor address is mapped to a local UDP forwarder socket; idle forwarders are cleaned up after 60 seconds. Note that UDP datagrams are limited by the internal buffer size (2048 bytes).

## Heartbeat

The server sends application-layer heartbeats on each control channel every `heartbeat_interval` seconds (`0` disables sending). The client expects some control command within `heartbeat_timeout` seconds; otherwise it treats the channel as dead and reconnects. `heartbeat_timeout` must be greater than `heartbeat_interval`.

## Hot reload

When the config file changes, the watcher compares the old and new configs: general changes (transport, addresses, tokens) trigger a full restart of the instance; service-level changes (add, remove, or modify a service) are applied without restarting.
