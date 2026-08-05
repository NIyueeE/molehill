# Container Deployment Examples

This directory shows how to deploy `molehill` in containers. The official
image `ghcr.io/niyueee/molehill:latest` is a single static musl binary on
`scratch`: no shell, no package manager, and it runs as the non-root UID
`1000`. A CA certificate bundle is baked in for TLS verification against
public certificates.

The image contains **no configuration** — the `server.toml` / `client.toml`
in this directory are only examples. Mount your own config file read-only
at `/app/server.toml` (or `/app/client.toml`) and pass its name as the
command-line argument.

## Docker / Podman Compose

```bash
docker compose up -d
# or with Podman:
podman compose up -d
```

`compose.yaml` uses host networking, which is the simplest way to expose the
server's service ports on Linux. If host networking is unavailable (e.g.
Docker Desktop on macOS/Windows), use `compose.bridge.yaml` instead:

```bash
docker compose -f compose.bridge.yaml up -d
```

In bridge mode the client reaches the server through the compose network, so
set `remote_addr = "molehill-server:2333"` in `client.toml`.

## Podman Quadlet (systemd)

Quadlet turns a `.container` file into a systemd service managed by Podman.

As root:

```bash
sudo cp molehill-server.container molehill-client.container /etc/containers/systemd/
sudo mkdir -p /etc/molehill
sudo cp server.toml /etc/molehill/ && sudo cp client.toml /etc/molehill/
sudo systemctl daemon-reload
sudo systemctl enable --now molehill-server molehill-client
```

Rootless:

```bash
mkdir -p ~/.config/containers/systemd
cp molehill-server.container molehill-client.container ~/.config/containers/systemd/
mkdir -p ~/.config/molehill
cp server.toml ~/.config/molehill/ && cp client.toml ~/.config/molehill/
# change WantedBy=multi-user.target to WantedBy=default.target in both files
systemctl --user daemon-reload
systemctl --user enable --now molehill-server molehill-client
```

## Notes

- The container runs as UID `1000`; make sure the mounted config file is
  readable by that user.
- For security, keep the token in the config file private (e.g. file
  permission `600`).
- For TLS transport, adapt the configs in `examples/tls/` and mount the
  certificate files in addition to the config, e.g.
  `-v /etc/molehill/identity.pfx:/app/identity.pfx:ro`.
- The image is built with rustls; see `.github/workflows/release.yml` for
  how the multi-arch image is assembled from the release build artifacts.
