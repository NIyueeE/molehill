# Transport

The control channel always speaks Noise, and it speaks one handshake:
**Noise_KK_25519_ChaChaPoly_BLAKE2s**. KK authenticates both ends — the
client knows the server's static key and the server knows the client's — so a
man-in-the-middle cannot impersonate either, and a config cannot select an
anonymous suite. This page covers the keys, the one switch that controls data
encryption, and the per-service and L3 overrides. The removed tables are on
[Configuration](configuration.md#migrating-to-0112-one-pool-per-session).

## Noise Protocol

The [Noise Protocol](http://noiseprotocol.org/noise.html) is a lightweight,
easy-to-configure way to encrypt the connection: X25519 keys and no PKI. Every
handshake is fresh, so each connection derives its own keys.

### Generate a keypair

Run `molehill --genkey` on each host. It prints an X25519 private
key and its public key:

`sh
$ molehill --genkey
Private Key:
cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q=

Public Key:
GQYTKSbWLBUSZiGfdWPSgek9yoOuaiwGD/GIX8Z1kkE=
`

(Do not use a keypair from the Internet, including this one.) Each host keeps
its own private key and writes the **other** host's public key.

`toml
# Server side
[server.noise]
private_key = "the server's private key"
public_key = "the client's public key"

# Client side
[client.noise]
private_key = "the client's private key"
public_key = "the server's public key"
data_enable = true
`

Both keys are required in both blocks and must decode to 32 bytes, or the
daemon refuses to start and names the block and the key. The same pair also
seeds the data plane, so one keypair per host is the normal deployment.

### The fixed pattern

There is no `pattern` key, no `psk` / `psk_location`, no
`resume` and no dialing proxy. The handshake is KK with
ChaCha20-Poly1305 and BLAKE2s in every build; a config that still writes any of
those keys is refused with the replacement (or the note that there is none).
The BLAKE2s hash runs only during the one-time handshake; the ChaCha20-Poly1305
data path is ring-accelerated and is the same for every session.

### Control and data channels

The **control channel is always encrypted** with the client's pair. It
publishes the session nonce the data channels authenticate with, so a plaintext
control leg would hand that credential to everyone on the path; a server
refuses a plaintext control channel on the connection it arrives on.

The **data channels** follow `data_enable`:

- `true` (the client default) wraps every TCP tunnel, direct channel and
  KCP session in the same KK record stream. Both ends pay the cipher's CPU per
  byte, and the path learns nothing about the forwarded payload.
- `false` sends the data channels plain. The visitor's own TLS/SSH is the
  end-to-end protection in that deployment, and this hop spends no CPU on a
  second encryption. The control channel stays encrypted either way.

`toml
[client.noise]
private_key = "the client's private key"
public_key = "the server's public key"
data_enable = false # data channels are plain; the control channel is still KK
`

The server needs no switch: it accepts a plain data channel from a client that
chose `data_enable = false` and a Noise one from everyone else, on the same
listener.

### Per-service overrides

A forwarding service may override the pair (to dial another server with its own
keypair) and the data switch. Unset fields inherit `[client.noise]`:

`toml
[client.services.ssh]              # inherits the client pair and data_enable

[client.services.bulk]
[client.services.bulk.noise]
data_enable = false                # this service's data channels are plain

[client.services.region-b]
remote_addr = "region-b.example.com:2333"
[client.services.region-b.noise]
private_key = "this client's other private key"
public_key = "region-b's public key"
`

The pair is the session's identity: services with the same endpoint and the
same pair share one control connection, and a different pair gets a session of
its own. `data_enable` alone never splits a session.

### Transparent (L3) clients

An L3 client writes the same three keys under `[transparent.noise]`. Its
control channel is KK; its lanes default to **plain**
(`data_enable = false`), because what a lane carries is the visitor's own IP
packets and the visitor's TLS is the end-to-end protection:

`toml
[transparent.noise]
private_key = "the client's private key"
public_key = "the server's public key"
data_enable = false

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"

[transparent.claims.web.noise]
data_enable = true                 # only this claim's lanes are encrypted
`

A claim may also override the keypair to reach a second server; unset fields
inherit `[transparent.noise]`.

### Migrating

The old `[client.transport]`, `[server.transport]` and
`[transparent.transport]` tables move to `[client.noise]`,
`[server.noise]` and `[transparent.noise]`;
`local_private_key` becomes `private_key` and
`remote_public_key` becomes `public_key`. The handshake now needs both
sides' keys, so add the client's public key to `[server.noise]` and the
client's private key to `[client.noise]`. `type = "noise"` becomes the
absence of `data_enable` (default true); `type = "plain"` becomes
`data_enable = false`; and `[client.services.<name>.transport]` becomes
`[client.services.<name>.noise]`.
