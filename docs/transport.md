# Transport

By default, `molehill` forwards traffic as it is (plain TCP). Different `transport` configurations can be enabled to secure the traffic. The `type` of `[client.transport]` and `[server.transport]` must match on both sides.

## TLS

TLS is the drop-in choice when you already have certificates, e.g. from a public CA or Let's Encrypt. See the [example](../examples/tls).

### Client

Normally a self-signed certificate is used, in which case the client needs to trust the CA. `trusted_root` is the path to the root CA's certificate PEM file. `hostname` is the hostname that the client uses to validate against the certificate that the server presents; it does not have to be the same as `client.remote_addr`.

```toml
[client.transport]
type = "tls"

[client.transport.tls]
trusted_root = "examples/tls/rootCA.crt"
hostname = "localhost"
```

If `trusted_root` is omitted, the system certificate store is used, which works for publicly trusted certificates.

### Server

A PKCS#12 archive is needed on the server side. It can be created with openssl:

```sh
openssl pkcs12 -export -out identity.pfx -inkey server.key -in server.crt -certfile ca_chain_certs.crt
```

Arguments:

- `-inkey`: Server private key
- `-in`: Server certificate
- `-certfile`: CA certificate

Creating a self-signed certificate with one's own CA is a non-trivial task; a script is provided in the [tls example folder](../examples/tls) for reference.

```toml
[server.transport]
type = "tls"

[server.transport.tls]
pkcs12 = "identity.pfx"
pkcs12_password = "password"
```

### Rustls support

`molehill` provides optional `rustls` support; see the [build guide](build-guide.md). One difference is that the crate used for loading PKCS#12 archives only handles limited types of PBE algorithms, so the archive must be created in the legacy (openssl 1.x) format. With openssl 3, add `-legacy`:

```sh
openssl pkcs12 -export -out identity.pfx -inkey server.key -in server.crt -certfile ca_chain_certs.crt -legacy
```

## Noise Protocol

The [Noise Protocol](http://noiseprotocol.org/noise.html) is a lightweight, easy-to-configure drop-in replacement of TLS: no self-signed certificates are needed to secure the connection.

`molehill` comes with a reasonable default configuration; see the minimal [example](../examples/noise_nk). The default pattern `Noise_NK_25519_ChaChaPoly_BLAKE2s` authenticates the server (like TLS with properly configured certificates), so MITM is no longer a problem.

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

# Server side
[server.transport]
type = "noise"
[server.transport.noise]
local_private_key = "cQ/vwIqNPJZmuM/OikglzBo/+jlYGrOt9i0k5h5vn1Q="
```

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

## WebSocket

The `websocket` transport tunnels the molehill protocol over WebSocket, which can help when only HTTP(S) traffic is allowed. Set `type = "websocket"` on both sides and configure the block:

```toml
[client.transport.websocket] # or [server.transport.websocket]
tls = true # Necessary. TLS on the WebSocket connection (uses the TLS settings above); set to false for plain WebSocket
```
