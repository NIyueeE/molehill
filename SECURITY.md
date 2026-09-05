# Security Policy

## Supported versions

Only the latest `main` branch receives security fixes.

## Reporting a vulnerability

Please do **not** open a public issue for security reports. Use GitHub's
private vulnerability reporting instead:

**Security → Report a vulnerability** on this repository.

You can expect a first response within a few days. If the report is accepted,
fixes are developed privately and released as soon as possible.

## Scope notes for molehill

- The shared `default_token` authenticates control channels; the server's
  `allow_ports` whitelist bounds what any client can register. Reports about
  bypassing either are in scope.
- Transport-layer concerns (TLS validation, Noise handshake, PSK handling)
  are in scope. Transport docs: [docs/transport.md](docs/transport.md).
- Supply-chain reports (a compromised dependency or CI action) also go here;
  actions are pinned to commit SHAs and audited by `cargo audit` /
  `cargo deny`.
