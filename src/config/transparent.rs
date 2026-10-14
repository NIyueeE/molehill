//! The transparent (L3) client's configuration model: `[transparent]`.
//!
//! An L3 client owns public addresses instead of forwarding to local ones, so
//! its block is not a `[client]` with different service types — it is a
//! different model, chosen by its own top-level table exactly as `[server]`
//! and `[client]` are. Every service it has is a **claim**, there is no
//! `protocol` key to switch a block's meaning, and the keys a forwarding
//! service would use (`local_addr`, `nodelay`, the UDP-only ones) have no home
//! in the schema at all rather than being refused one by one.
//!
//! What it does *not* do is fork the engine. [`TransparentClientConfig::lower`]
//! produces the very [`ClientConfig`] the forwarding path already runs on —
//! one service per claim, `ServiceType::Transparent`, the claimed address
//! standing in for `local_addr` — so the client, the data path and the wire
//! see one shape, and this module stays the only place that knows about a
//! second way to write a client down.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

use crate::config::parsing::{
    ClientConfig, ClientControlConfig, ClientServiceConfig, MaskedString, ServiceType,
    TransportConfig, TransportType, default_tun_name,
};
#[cfg(feature = "multiplex")]
use crate::config::parsing::{ClientDataConfig, DataCarrier, DataCarrierLimits};

/// `[transparent]`: the whole configuration of an L3 client.
///
/// Its presence is what makes a run an L3 one, so the table carries the
/// client-wide half of the model (token, device, control and data defaults)
/// and [`Self::claims`] carries the per-address half.
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct TransparentClientConfig {
    /// Shared secret, must match `[server].default_token`. Required.
    pub default_token: MaskedString,
    /// The TUN device this client attaches to; the device, its addresses and
    /// its routes are the operator's, as on the forwarding side.
    #[serde(default = "default_tun_name")]
    pub tun: String,
    #[serde(default)]
    pub control: ClientControlConfig,
    #[cfg(feature = "multiplex")]
    #[serde(default)]
    pub data: TransparentDataConfig,
    #[serde(default)]
    pub transport: TransparentTransportConfig,
    /// The public addresses this client claims, one per named entry.
    pub claims: HashMap<String, TransparentClaimConfig>,
    /// The forwarding-shaped config this block lowers to, filled by
    /// [`crate::config::Config::from_str`] after it has been validated. A
    /// claim's runtime fields (`local_addr`, `transparent_tun`) are the
    /// lowering's work, so the validated form is what the client runs on.
    #[serde(skip)]
    pub(crate) lowered: Option<Box<ClientConfig>>,
}

/// One claimed public address (`[transparent.claims.<name>]`).
///
/// The same per-service overrides a forwarding service has, minus everything
/// that describes dialing a local application: there is nothing to dial.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TransparentClaimConfig {
    /// The public address this client **claims**, e.g. `"10.99.0.1:8443"`.
    /// Required. The server binds nothing for it; its port must still be
    /// covered by the server's `allow_ports`, because a claim is a
    /// registration like any other.
    pub remote_bind_addr: String,
    /// Override `[transparent].default_token` for this claim only — e.g. to
    /// authenticate against a server that has its own token.
    pub token: Option<MaskedString>,
    /// Override `[transparent.control].default_remote_addr` for this claim
    /// only: the claim's control channel (and, by default, its data plane)
    /// dials this server instead of the client-wide one.
    pub remote_addr: Option<String>,
    /// Override `[transparent.control].default_retry_interval` for this claim.
    pub retry_interval: Option<u64>,
    /// Override `[transparent.data].default_carrier` for this claim only.
    #[cfg(feature = "multiplex")]
    pub carrier: Option<DataCarrier>,
}

/// Data-plane knobs for a claim (`[transparent.data]`).
///
/// The same keys as `[client.data]`, minus the pool: a claim **never**
/// multiplexes, because its channels *are* its carrier connections — its
/// throughput is the sum of them, not of streams. A multiplexer over such a
/// channel would be framing for nothing: on the same host and workload, a claim
/// on its own connection moved 6 % fewer wire bytes, took 33 % less CPU per
/// packet and carried 65 % more round trips per second than the same claim as
/// one stream of a pool ([benchmarks.md](../../docs/benchmarks.md), "The
/// transparent-L3 wire question").
///
/// What is left for `tunnels` to mean is therefore a **lane budget**: the
/// carrier connections this client holds for all of its claims together, shared
/// out by [`TransparentClientConfig::claim_lanes`]. One lane is one connection,
/// and every claim always has at least one.
#[cfg(feature = "multiplex")]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TransparentDataConfig {
    /// Data-plane endpoint; defaults to the claim's control endpoint. Applies
    /// to every claim that does not name its own `remote_addr`.
    pub default_data_addr: Option<String>,
    /// `tcp` (default) or `kcp`. Either one carries a claim's connections: a
    /// `tcp` channel is a TCP connection, a `kcp` one is a KCP session of its
    /// own.
    #[serde(default)]
    pub default_carrier: DataCarrier,
    /// `[transparent.data.tcp]`: the TCP carrier's lane budget.
    #[serde(default)]
    pub tcp: DataCarrierLimits,
    /// `[transparent.data.kcp]`: the KCP carrier's lane budget.
    #[serde(default)]
    pub kcp: DataCarrierLimits,
}

/// How this client reaches its server (`[transparent.transport]`).
///
/// One key, on purpose: what an L3 client sends is whole IP packets whose
/// content is the visitor's own, so it is a plain link by design and there are
/// no encryption keys for the schema to hold. What is left is the *dialing*
/// half, which is a property of the path to the server and not of the payload.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TransparentTransportConfig {
    /// Proxy used to reach the server (`http` / `socks5`).
    pub proxy: Option<Url>,
}

#[cfg(feature = "multiplex")]
impl TransparentClientConfig {
    /// How many claims draw on one carrier's lane budget.
    pub(crate) fn claims_on(&self, carrier: DataCarrier) -> usize {
        self.claims
            .values()
            .filter(|claim| claim.carrier.unwrap_or(self.data.default_carrier) == carrier)
            .count()
    }

    /// The carrier's lane budget as the operator wrote it, or `None`.
    pub(crate) fn written_budget(&self, carrier: DataCarrier) -> Option<usize> {
        match carrier {
            DataCarrier::Tcp => self.data.tcp.written(),
            DataCarrier::Kcp => self.data.kcp.written(),
        }
    }

    /// The carrier's lane budget: what the operator wrote, else one lane per
    /// claim that draws on it.
    ///
    /// The default is the shape a claim had before the budget existed — one
    /// carrier connection each — so a client that never writes `tunnels` gets
    /// exactly that, and raising the key is what spreads a busy claim's inner
    /// flows over more connections.
    pub(crate) fn lane_budget(&self, carrier: DataCarrier) -> usize {
        match self.written_budget(carrier) {
            Some(written) => written,
            None => self.claims_on(carrier).max(1),
        }
    }

    /// One claim's lane count: an equal share of the carrier's budget, never
    /// below one.
    ///
    /// Equal share is the whole policy for now, and it is deliberately static:
    /// it is a function of the configuration, so the capacity a deployment
    /// offers does not depend on what it happened to be doing a minute ago.
    /// Lending a lane that an idle claim is not using is the next step, and the
    /// one that needs its own measurement (HANDOFF.md, L15).
    pub(crate) fn claim_lanes(&self, carrier: DataCarrier) -> usize {
        (self.lane_budget(carrier) / self.claims_on(carrier).max(1)).max(1)
    }
}

impl TransparentClientConfig {
    /// The forwarding-shaped config this block means.
    ///
    /// The mapping is the whole of the model difference: one service per
    /// claim, the claimed address standing in for `local_addr` (nothing is
    /// dialed — the local application binds the claimed address itself), and
    /// the device carried on every service so the data path knows what to
    /// attach to. The transport is `plain` with no keys, which is not a
    /// default but the only value this model has.
    #[must_use]
    pub fn lower(&self) -> ClientConfig {
        let services = self
            .claims
            .iter()
            .map(|(name, claim)| {
                let mut service = ClientServiceConfig::with_name(name);
                service.service_type = ServiceType::Transparent;
                service.remote_bind_addr.clone_from(&claim.remote_bind_addr);
                service.local_addr.clone_from(&claim.remote_bind_addr);
                service.transparent_tun.clone_from(&self.tun);
                service.token.clone_from(&claim.token);
                service.remote_addr.clone_from(&claim.remote_addr);
                service.retry_interval = claim.retry_interval;
                #[cfg(feature = "multiplex")]
                {
                    let carrier = claim.carrier.unwrap_or(self.data.default_carrier);
                    service.carrier = claim.carrier;
                    // The claim's lane count, resolved here so the engine has one
                    // number to read: an equal share of the carrier's budget,
                    // never below the one connection every claim has.
                    service.transparent_lanes =
                        u16::try_from(self.claim_lanes(carrier)).unwrap_or(u16::MAX);
                }
                (name.clone(), service)
            })
            .collect();

        ClientConfig {
            default_token: self.default_token.clone(),
            control: self.control.clone(),
            #[cfg(feature = "multiplex")]
            data: ClientDataConfig {
                default_data_addr: self.data.default_data_addr.clone(),
                default_carrier: self.data.default_carrier,
                // A claim never draws from a pool, so pool ownership has nothing
                // to select here: the lowered block carries the defaults the
                // engine reads for the data endpoint and the carrier only.
                shared_pool: false,
                tcp: self.data.tcp.clone(),
                kcp: self.data.kcp.clone(),
            },
            services,
            transport: TransportConfig {
                transport_type: TransportType::Plain,
                proxy: self.transport.proxy.clone(),
                noise: None,
            },
        }
    }

    /// The validated lowering, or an error naming what is missing.
    ///
    /// # Errors
    ///
    /// Fails when the block never went through
    /// [`crate::config::Config::from_str`], which is the only thing that
    /// validates a config and therefore the only thing that fills this in.
    pub fn client_config(&self) -> Result<&ClientConfig> {
        self.lowered.as_deref().ok_or_else(|| {
            anyhow::anyhow!("the `[transparent]` block was not validated before it was used")
        })
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use crate::config::parsing::Config;
    #[cfg(all(feature = "transparent", target_os = "linux"))]
    use crate::config::parsing::{ServiceType, TransportType};
    #[cfg(all(feature = "transparent", target_os = "linux"))]
    use url::Url;

    /// The lowering is the whole of the model difference, so it is pinned key
    /// by key: a claim becomes a service whose *claimed* address stands in for
    /// `local_addr` (nothing is dialed), carrying the device the data path
    /// attaches to, and the transport is plain because this model has nowhere
    /// to say otherwise.
    #[cfg(all(feature = "transparent", target_os = "linux"))]
    #[test]
    fn a_claim_lowers_to_a_transparent_service() {
        let config = Config::from_str(
            r#"
[transparent]
default_token = "t"
tun = "l3test0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.transport]
proxy = "socks5://127.0.0.1:1080"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
token = "claim-token"
"#,
        )
        .unwrap();

        let client = config.into_l3_client().unwrap().client.unwrap();
        assert_eq!(client.services.len(), 1);
        let service = client.services.get("web").unwrap();
        assert_eq!(service.name, "web", "the table key names the claim");
        assert_eq!(service.service_type, ServiceType::Transparent);
        assert_eq!(service.remote_bind_addr, "10.99.0.1:8443");
        assert_eq!(
            service.local_addr, "10.99.0.1:8443",
            "the client owns the address it claims: that is what stands where a forwarding \
             service would name the application to dial"
        );
        assert_eq!(service.transparent_tun, "l3test0");
        assert_eq!(&**service.token.as_ref().unwrap(), "claim-token");
        assert_eq!(
            service.transparent_lanes, 1,
            "a claim has one lane unless the carrier's budget is raised"
        );
        assert_eq!(client.transport.transport_type, TransportType::Plain);
        assert!(client.transport.noise.is_none());
        assert_eq!(
            client.transport.proxy.as_ref().map(Url::as_str),
            Some("socks5://127.0.0.1:1080")
        );
    }

    /// A claim's lanes are an equal share of its carrier's budget: one each
    /// when the key is unwritten (the shape a claim had before the budget), and
    /// the written budget divided among the claims that draw on that carrier.
    #[cfg(all(feature = "multiplex", feature = "transparent", target_os = "linux"))]
    #[test]
    fn a_claims_lanes_are_an_equal_share_of_the_carriers_budget() {
        let lanes = |budget: &str, claims: &str| {
            let config = Config::from_str(&format!(
                r#"
[transparent]
default_token = "t"
tun = "l3test0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.data]
{budget}

{claims}
"#
            ))
            .unwrap()
            .into_l3_client()
            .unwrap()
            .client
            .unwrap()
            .services;
            (
                config["web"].transparent_lanes,
                config.get("ssh").map(|s| s.transparent_lanes),
            )
        };
        let web = "[transparent.claims.web]\nremote_bind_addr = \"10.99.0.1:8443\"\n";
        let two =
            format!("{web}\n[transparent.claims.ssh]\nremote_bind_addr = \"10.99.0.1:2222\"\n");

        assert_eq!(
            lanes("", web).0,
            1,
            "one connection per claim is the default: the budget is the claim count"
        );
        assert_eq!(
            lanes("[transparent.data.tcp]\ntunnels = 4\n", &two).0,
            2,
            "four lanes over two claims is two each"
        );
        assert_eq!(
            lanes("[transparent.data.tcp]\ntunnels = 4\n", &two).1,
            Some(2),
            "the share is equal, not first-come"
        );
        assert_eq!(
            lanes("[transparent.data.tcp]\ntunnels = 5\n", &two).0,
            2,
            "a budget that does not divide evenly still gives every claim one (integer share)"
        );
        assert_eq!(
            lanes("[transparent.data.tcp]\ntunnels = 2\n", web).0,
            2,
            "a single claim takes the whole budget"
        );
    }

    /// A budget below the claim count is refused with the number to write: a
    /// claim's lane is a connection of its own, so some claim would otherwise
    /// have no carrier at all.
    #[cfg(all(feature = "multiplex", feature = "transparent", target_os = "linux"))]
    #[test]
    fn a_lane_budget_below_the_claim_count_is_refused() {
        let err = format!(
            "{:#}",
            Config::from_str(
                r#"
[transparent]
default_token = "t"
tun = "l3test0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.data.tcp]
tunnels = 1

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"

[transparent.claims.ssh]
remote_bind_addr = "10.99.0.1:2222"
"#
            )
            .unwrap_err()
        );
        assert!(
            err.contains("`[transparent.data.tcp].tunnels = 1`")
                && err.contains("`[transparent.data.tcp].tunnels = 2`")
                && err.contains("2 claim(s)"),
            "the refusal must name the key, the count and the count to write: {err}"
        );
    }

    /// The `[transparent.control]` block is where the server is named, so a
    /// block without one is refused — and the message names the block the
    /// reader actually wrote, not `[client.control]`.
    #[test]
    fn a_claim_needs_a_server_to_dial() {
        let err = Config::from_str(
            r#"
[transparent]
default_token = "t"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#,
        )
        .unwrap_err();

        let message = format!("{err:#}");
        assert!(
            message.contains("[transparent.control].default_remote_addr"),
            "the refusal must name the block the reader wrote, got: {message}"
        );
    }

    /// An L3 client has claims and nothing else: a forwarding key has no home
    /// in the model, and the parse says so rather than ignoring it.
    #[test]
    fn a_claim_has_no_home_for_forwarding_keys() {
        let err = Config::from_str(
            r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
local_addr = "127.0.0.1:8080"
"#,
        )
        .unwrap_err();

        let message = format!("{err:#}");
        assert!(
            message.contains("unknown field `local_addr`"),
            "a key the model has no home for must be an unknown field, got: {message}"
        );
    }
}

#[cfg(all(test, feature = "multiplex"))]
mod data_tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use crate::config::DataCarrier;
    use crate::config::parsing::Config;

    /// A claim never multiplexes: its data channels are carrier connections of
    /// its own, which is what `ClientServiceConfig::uses_pool` derives from the
    /// service type — the shape is a property of the model, not a key.
    #[test]
    fn a_claim_never_multiplexes() {
        let config = Config::from_str(
            r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#,
        )
        .unwrap();
        let client = config.into_l3_client().unwrap().client.unwrap();
        assert!(!client.services["web"].uses_pool());
    }

    /// A claim's carrier is a key of its own: a claim over KCP is a KCP session
    /// per channel, and the pair needs no mode to be meaningful.
    #[test]
    fn a_kcp_carrier_is_allowed_for_a_claim() {
        let config = r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.data]
default_carrier = "kcp"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#;
        if cfg!(feature = "kcp") {
            let client = Config::from_str(config)
                .unwrap()
                .into_l3_client()
                .unwrap()
                .client
                .unwrap();
            assert_eq!(client.data.default_carrier, DataCarrier::Kcp);
            assert!(!client.services["web"].uses_pool());
        } else {
            let message = format!("{:#}", Config::from_str(config).unwrap_err());
            assert!(
                message.contains("[transparent.data].default_carrier") && message.contains("kcp"),
                "the refusal must name the block the reader wrote, got: {message}"
            );
        }
    }
}
