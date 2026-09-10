# Transport

By default, `molehill` forwards traffic as it is (plain TCP). The client's
`[client.transport]` block supports two types — `plain` and `noise` — and
**the client decides**: every connection starts with a v3 transport selector
byte, and the server accepts whatever the client speaks (its
`[server.transport]` block only places the Noise keys — there is no
server-side `type`). The noise-vs-plain benchmark is the price list for
this choice: see README's configuration guide. This page covers `noise`;
`plain` needs no configuration beyond the default.

## Noise Protocol

The [Noise Protocol](http://noiseprotocol.org/noise.html) is a lightweight,
easy-to-configure way to encrypt the connection: one X25519 keypair, no PKI.

`molehill` comes with a reasonable default configuration; see the minimal [noise example](./configuration.md#noise-encrypted-transport). The default pattern `Noise_NK_25519_ChaChaPoly_BLAKE2s` authenticates the server, so MITM is no longer a problem.

> **What does ring-accelerated change?** The default build links `snow`'s
> **ring-accelerated** resolver, so the ChaCha20-Poly1305 data path — the
> hot path for every encrypted byte — runs ring's hardware-dispatched
> implementation: measured ~1.5x the pure-Rust resolver at the transport
> level and ~1.3x end-to-end on x86-64. The pattern's hash (BLAKE2s by
> default, or SHA-256/SHA-512 for other patterns) only runs during the
> one-time handshake, so it does not affect throughput: every pattern gets
> the accelerated cipher. The wire format is unchanged.

To use it, an X25519 keypair is needed.

### Generate a keypair

Run `molehill --genkey`, which generates a keypair using the default X25519 algorithm (pass `x448` for X448):

```sh
$ molehill --genkey
Private Key:
cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q=

Public Key:
GQYTKSbWLBUSZiGfdWPSgek9yoOuaiwGD/GIX8Z1kkE=
```

(WARNING: Don't use the keypair from the Internet, including this one)

The server keeps the private key to identify itself, and the client keeps the public key to verify the server:

```toml
# Client side
[client.transport]
type = "noise"
[client.transport.noise]
remote_public_key = "GQYTKSbWLBUSZiGfdWPSgek9yoOuaiwGD/GIX8Z1kkE="

# Server side (keys only — no `type`; the client's choice decides)
[server.transport.noise]
local_private_key = "cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q="
```

### Per-service encryption

The client-wide `[client.transport].type` is the default for every service,
and each service can override it individually — including its own keys, which
is what a multi-server setup needs (each server holds its own keypair):

```toml
[client.transport]
type = "noise"            # default: every service is encrypted
[client.transport.noise]
remote_public_key = "server-a-pub-key"

[client.services.ssh]     # inherits: encrypted with the global key

[client.services.bulk]
transport = { type = "plain" }   # opt out: plain

[client.services.region-b]       # global plain + this service encrypted
remote_addr = "region-b.example.com:2333"
transport = { type = "noise", noise = { remote_public_key = "server-b-pub-key" } }
```

Rules: `transport.type` unset follows the client-wide `type`; `"noise"`
forces encryption, `"plain"` forces plaintext. A service whose effective transport is
Noise needs keys — its own `transport.noise` if set, else the global
`[client.transport].noise`; configuring effective Noise with no keys
anywhere is a startup error. The data plane follows the service: TCP
tunnels and (with `carrier = "kcp"`) the Noise-over-KCP wrapping both use
the service's effective keys. The server needs nothing beyond placing its
keys (v3 selector: it accepts whatever each connection speaks).

### Specifying the pattern

The default pattern satisfies most use cases, but other patterns can be useful:

**No authentication** (`Noise_XX_...`): encrypts traffic but provides no authentication, so it is vulnerable to MITM attacks while resisting sniffing and replay attacks. Use it when MITM is not a concern:

```toml
[server.transport.noise]
pattern = "Noise_XX_25519_ChaChaPoly_BLAKE2s"

[client.transport.noise]
pattern = "Noise_XX_25519_ChaChaPoly_BLAKE2s"
```

**Bidirectional authentication** (`Noise_KK_...`): both sides authenticate each other:

```toml
[server.transport.noise]
pattern = "Noise_KK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "server-priv-key-here"
remote_public_key = "client-pub-key-here"

[client.transport.noise]
pattern = "Noise_KK_25519_ChaChaPoly_BLAKE2s"
local_private_key = "client-priv-key-here"
remote_public_key = "server-pub-key-here"
```

### Pre-shared keys

`psk` and `psk_location` add a pre-shared key to the handshake. The pattern must include a PSK modifier (e.g. `Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s`), the key must be 32 bytes base64-encoded, and both sides must use the same `psk` and `psk_location`:

```toml
[server.transport.noise]
pattern = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s"
local_private_key = "server-priv-key-here"
remote_public_key = "client-pub-key-here"
psk = "the-same-32-byte-key-in-base64"
psk_location = 0

[client.transport.noise]
pattern = "Noise_KKpsk0_25519_ChaChaPoly_BLAKE2s"
local_private_key = "client-priv-key-here"
remote_public_key = "server-pub-key-here"
psk = "the-same-32-byte-key-in-base64"
psk_location = 0
```

### Other patterns

To find out which pattern to use, refer to:

- [7.5. Interactive handshake patterns (fundamental)](https://noiseprotocol.org/noise.html#interactive-handshake-patterns-fundamental)
- [8. Protocol names and modifiers](https://noiseprotocol.org/noise.html#protocol-names-and-modifiers)
