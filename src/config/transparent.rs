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
    ClientConfig, ClientControlConfig, ClientServiceConfig, ClientServiceTransportConfig,
    MaskedString, NoiseConfig, ServiceType, TransportConfig, TransportType, default_tun_name,
};
#[cfg(feature = "multiplex")]
use crate::config::parsing::{ClientDataConfig, DataCarrierLimits};

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
    /// Override `[transparent.data].tunnels` for this claim only: its lanes.
    #[cfg(feature = "multiplex")]
    pub tunnels: Option<u16>,
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
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct TransparentDataConfig {
    /// Data-plane endpoint; defaults to the claim's control endpoint. Applies
    /// to every claim that does not name its own `remote_addr`.
    pub default_data_addr: Option<String>,
    /// The **lane budget**: how many carrier connections this client's claims
    /// hold together, divided equally among them. Unset means one lane per
    /// claim; a value below the claim count is refused.
    ///
    /// The same key a forwarding client writes, because a lane is the same
    /// physical thing a pool's tunnel is — one carrier connection — and the
    /// carrier itself is chosen at runtime (KCP, falling back to TCP).
    pub tunnels: Option<u16>,
    /// Whether a claim's idle lanes may be lent to a sibling claim carrying more
    /// flows than lanes (`lend = false` keeps the equal split static).
    ///
    /// Default `true`: the mechanism only ever moves a lane that is carrying
    /// nothing, and the evidence says it is what makes a claim's throughput
    /// follow its *flows* rather than its share (HANDOFF.md, "Lending,
    /// measured"). It is a key because the trade is real — lending buys
    /// throughput with connections and CPU, and only where the carrier is the
    /// limit — and because a knob is what lets the model measure the mechanism
    /// as an arm instead of a scratch build.
    #[serde(default = "lend_by_default")]
    pub lend: bool,
}

/// The default of `[transparent.data].lend`, spelled out because `serde`'s
/// `Default` for a `bool` is `false` and this key's default is `true`.
#[cfg(feature = "multiplex")]
const fn lend_by_default() -> bool {
    true
}

/// Written out rather than derived: `bool`'s own default is `false`, so a
/// derived one would turn lending *off* for every config that omits the table
/// — the opposite of the key's documented default, and invisible in the schema.
#[cfg(feature = "multiplex")]
impl Default for TransparentDataConfig {
    fn default() -> Self {
        Self {
            default_data_addr: None,
            tunnels: None,
            lend: lend_by_default(),
        }
    }
}

/// How this client reaches its server (`[transparent.transport]`).
///
/// What an L3 client sends is whole IP packets whose content is the visitor's
/// own, so a **lane** is plain unless the operator turns the crypto on — the
/// visitor's TLS is the end-to-end protection, and per-packet encryption
/// spends CPU the carried flows may not have asked for. The control channel
/// is a different matter and not this table's to turn off: it always speaks
/// Noise, because the session nonce it publishes is the credential every lane
/// authenticates with.
///
/// `type = "noise"` encrypts the lanes with the **control channel's key pair
/// and pattern** by default — one key pair per host, inherited, so a config
/// that asks for encrypted lanes writes no key material of its own. A claim
/// whose keys differ from the control channel's overrides with its own
/// `[transparent.transport.noise]` block instead.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TransparentTransportConfig {
    /// Proxy used to reach the server (`http` / `socks5`).
    pub proxy: Option<Url>,
    /// The lanes' transport. `plain` (the default) sends the visitor's
    /// packets as they are; `noise` wraps every lane in the Noise session the
    /// control channel's keys define. The *control* channel is always Noise
    /// and is not configurable here.
    #[serde(rename = "type")]
    pub transport_type: Option<TransportType>,
    /// Key material for `type = "noise"`. Absent, the control channel's keys
    /// are inherited — the common case, one key pair per host. Present, these
    /// keys define the lanes' Noise session instead.
    pub noise: Option<NoiseConfig>,
}

#[cfg(feature = "multiplex")]
impl TransparentClientConfig {
    /// How many claims share the lane budget.
    pub(crate) fn claims(&self) -> usize {
        self.claims.len()
    }

    /// The lane budget as the operator wrote it, or `None`.
    pub(crate) fn written_budget(&self) -> Option<usize> {
        DataCarrierLimits {
            tunnels: self.data.tunnels,
        }
        .written()
    }

    /// The lane budget: what the operator wrote, else one lane per claim.
    ///
    /// The default is the shape a claim had before the budget existed — one
    /// carrier connection each — so a client that never writes `tunnels` gets
    /// exactly that, and raising the key is what spreads a busy claim's inner
    /// flows over more connections.
    pub(crate) fn lane_budget(&self) -> usize {
        match self.written_budget() {
            Some(written) => written,
            None => self.claims().max(1),
        }
    }

    /// One claim's lane count: an equal share of the lane budget, never
    /// below one.
    ///
    /// Equal share is the whole policy for now, and it is deliberately static:
    /// it is a function of the configuration, so the capacity a deployment
    /// offers does not depend on what it happened to be doing a minute ago.
    /// Lending a lane that an idle claim is not using is the next step, and the
    /// one that needs its own measurement (HANDOFF.md, L15).
    pub(crate) fn claim_lanes(&self) -> usize {
        (self.lane_budget() / self.claims().max(1)).max(1)
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
        // The wire stacks, from `[transparent.transport]`:
        //
        // - the **control** channel is Noise, always — the session nonce it
        //   publishes is the credential every lane authenticates with, so a
        //   plain control leg would hand that credential to the path. The
        //   keys are this table's (`[transparent.transport.noise]`): one key
        //   pair per host, and the mandatory-Noise rule is what makes them
        //   required.
        // - the **lanes** are plain unless the operator turned the crypto on
        //   (`type = "noise"`); on, they inherit the control channel's keys
        //   by default. The resolved pair travels per service, so the
        //   engine's `effective_config` overlays it on the client-wide
        //   transport and a lane dials with exactly what this table said.
        let control_transport = TransportConfig {
            transport_type: TransportType::Noise,
            proxy: self.transport.proxy.clone(),
            noise: self.transport.noise.clone(),
        };
        let lane_transport = TransportConfig {
            transport_type: self
                .transport
                .transport_type
                .unwrap_or(TransportType::Plain),
            proxy: self.transport.proxy.clone(),
            noise: match self.transport.transport_type {
                Some(TransportType::Noise) => self.transport.noise.clone(),
                _ => None,
            },
        };

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
                // The lanes' wire stack travels per service: the engine's
                // `effective_config` overlays this on the client-wide
                // transport, so a claim's lanes dial with exactly what this
                // table said — plain or the inherited/overridden Noise.
                service.transport = Some(ClientServiceTransportConfig {
                    transport_type: Some(lane_transport.transport_type),
                    noise: lane_transport.noise.clone(),
                });
                #[cfg(feature = "multiplex")]
                {
                    // The claim's lane count, resolved here so the engine has one
                    // number to read: an equal share of the lane budget, never
                    // below the one connection every claim has. A claim's own
                    // `tunnels` does not divide the budget — the budget belongs to
                    // the client — so it is a refusal when written on a claim, and
                    // the schema is where that is said.
                    service.transparent_lanes =
                        u16::try_from(self.claim_lanes()).unwrap_or(u16::MAX);
                    service.transparent_lend = self.data.lend;
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
                // A claim never draws from a pool, so pool ownership has nothing
                // to select here, and the lane budget travels per claim
                // (`transparent_lanes`): this block carries only the data
                // endpoint.
                shared_pool: false,
                tunnels: None,
            },
            services,
            status: None,
            transport: control_transport,
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
    /// attaches to. The wire stacks split here: the control channel is Noise
    /// (its keys are this table's), the lanes are plain by default — the
    /// visitor's TLS is the end-to-end protection — and the resolved pair
    /// travels per service so the engine's overlay dials each with its own.
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

[transparent.transport.noise]
local_private_key = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
remote_public_key = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

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
            "a claim has one lane unless the lane budget is raised"
        );
        assert!(
            service.transparent_lend,
            "lending is on unless the client turns it off: `[transparent.data].lend` defaults \
             to true, so a config that never mentions it gets the mechanism"
        );
        // The control leg is Noise, always: its keys are the table's, and the
        // session nonce it publishes is what every lane authenticates with.
        assert_eq!(client.transport.transport_type, TransportType::Noise);
        assert!(client.transport.noise.is_some());
        // The lanes default to plain: the visitor's packets go as they are.
        let lanes = service.transport.as_ref().unwrap();
        assert_eq!(lanes.transport_type, Some(TransportType::Plain));
        assert!(lanes.noise.is_none());
        assert_eq!(
            client.transport.proxy.as_ref().map(Url::as_str),
            Some("socks5://127.0.0.1:1080")
        );
    }

    /// A claim's lanes are an equal share of its lane budget: one each
    /// when the key is unwritten (the shape a claim had before the budget), and
    /// the written budget divided among the claims that draw on that carrier.
    #[cfg(all(feature = "multiplex", feature = "transparent", target_os = "linux"))]
    /// `type = "noise"` on the lanes: the table's keys are the control
    /// channel's and the lanes' at once (one pair per host), and the per-service
    /// overlay carries the resolved pair so a lane dials Noise without the
    /// engine knowing this model exists.
    #[cfg(all(feature = "transparent", target_os = "linux"))]
    #[test]
    fn noise_lanes_inherit_the_table_keys() {
        let config = Config::from_str(
            r#"
[transparent]
default_token = "t"
tun = "l3test0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.transport]
type = "noise"

[transparent.transport.noise]
local_private_key = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
remote_public_key = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#,
        )
        .unwrap();

        let client = config.into_l3_client().unwrap().client.unwrap();
        // Control: Noise with the table's keys (the same pair the lanes use).
        assert_eq!(client.transport.transport_type, TransportType::Noise);
        assert!(client.transport.noise.is_some());
        // Lanes: Noise with the same keys, traveling per service.
        let service = client.services.get("web").unwrap();
        let lanes = service.transport.as_ref().unwrap();
        assert_eq!(lanes.transport_type, Some(TransportType::Noise));
        assert_eq!(lanes.noise.clone(), client.transport.noise.clone());
    }

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

[transparent.transport.noise]
remote_public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="

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
            lanes("tunnels = 4", &two).0,
            2,
            "four lanes over two claims is two each"
        );
        assert_eq!(
            lanes("tunnels = 4", &two).1,
            Some(2),
            "the share is equal, not first-come"
        );
        assert_eq!(
            lanes("tunnels = 5", &two).0,
            2,
            "a budget that does not divide evenly still gives every claim one (integer share)"
        );
        assert_eq!(
            lanes("tunnels = 2", web).0,
            2,
            "a single claim takes the whole budget"
        );
    }

    /// Lending is on unless the client says otherwise, and the switch reaches
    /// the claim the runtime reads it from.
    #[cfg(all(feature = "multiplex", feature = "transparent", target_os = "linux"))]
    #[test]
    fn lending_is_on_unless_the_client_turns_it_off() {
        let base = "[transparent]\ndefault_token = \"t\"\ntun = \"l3test0\"\n\n\
                    [transparent.control]\ndefault_remote_addr = \"example.com:2333\"\n\n\
                    [transparent.transport.noise]\n\
                    remote_public_key = \"{TEST_CLIENT_REMOTE_PUBLIC}\"\n\n";
        let claim = "[transparent.claims.web]\nremote_bind_addr = \"10.99.0.1:8443\"\n";

        let on = Config::from_str(&format!("{base}{claim}")).unwrap();
        let service = on.into_l3_client().unwrap().client.unwrap().services["web"].clone();
        assert!(service.transparent_lend);

        let off = Config::from_str(&format!(
            "{base}[transparent.data]\nlend = false\n\n{claim}"
        ))
        .unwrap();
        let service = off.into_l3_client().unwrap().client.unwrap().services["web"].clone();
        assert!(
            !service.transparent_lend,
            "an operator who turns lending off gets the equal split held static"
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

[transparent.transport.noise]
remote_public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="

[transparent.data]
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
            err.contains("`[transparent.data].tunnels = 1`")
                && err.contains("`[transparent.data].tunnels = 2`")
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

[transparent.transport.noise]
remote_public_key = "{TEST_CLIENT_REMOTE_PUBLIC}"

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

[transparent.transport.noise]
remote_public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="

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

[transparent.transport.noise]
remote_public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#,
        )
        .unwrap();
        let client = config.into_l3_client().unwrap().client.unwrap();
        assert!(!client.services["web"].uses_pool());
    }

    /// A claim never multiplexes: it has no pool to size, so its lanes are
    /// connections of its own and `tunnels` is a budget rather than a width.
    /// The carrier is not a key anywhere, which is what this pins: a claim over
    /// KCP is what the runtime chooses when the path carries it.
    #[test]
    fn a_kcp_carrier_is_not_a_key_a_claim_can_write() {
        let config = r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.transport.noise]
remote_public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="

[transparent.data]
tunnels = 2

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#;
        let client = Config::from_str(config)
            .unwrap()
            .into_l3_client()
            .unwrap()
            .client
            .unwrap();
        assert!(!client.services["web"].uses_pool());
        assert_eq!(
            client.services["web"].lanes(),
            2,
            "two lanes for one claim: the budget is its connection count"
        );

        // The old selector is refused with the reason and the replacement, so a
        // reader who wrote it is told what to do rather than that it is wrong.
        let with_carrier = config.replace(
            "[transparent.data]\ntunnels = 2",
            "[transparent.data]\ndefault_carrier = \"kcp\"",
        );
        let message = format!("{:#}", Config::from_str(&with_carrier).unwrap_err());
        assert!(
            message.contains("default_carrier"),
            "the refusal must name the key the reader wrote, got: {message}"
        );
    }

    /// Lending is on unless the client says otherwise, and the switch reaches
    /// the claim the runtime reads it from.
    #[cfg(all(feature = "multiplex", feature = "transparent", target_os = "linux"))]
    #[test]
    fn lending_is_on_unless_the_client_turns_it_off() {
        let base = "[transparent]\ndefault_token = \"t\"\ntun = \"l3test0\"\n\n\
                    [transparent.control]\ndefault_remote_addr = \"example.com:2333\"\n\n\
                    [transparent.transport.noise]\n\
                    remote_public_key = \"{TEST_CLIENT_REMOTE_PUBLIC}\"\n\n";
        let claim = "[transparent.claims.web]\nremote_bind_addr = \"10.99.0.1:8443\"\n";

        let on = Config::from_str(&format!("{base}{claim}")).unwrap();
        let service = on.into_l3_client().unwrap().client.unwrap().services["web"].clone();
        assert!(service.transparent_lend);

        let off = Config::from_str(&format!(
            "{base}[transparent.data]\nlend = false\n\n{claim}"
        ))
        .unwrap();
        let service = off.into_l3_client().unwrap().client.unwrap().services["web"].clone();
        assert!(
            !service.transparent_lend,
            "an operator who turns lending off gets the equal split held static"
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

[transparent.transport.noise]
remote_public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="

[transparent.data]
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
            err.contains("`[transparent.data].tunnels = 1`")
                && err.contains("`[transparent.data].tunnels = 2`")
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

[transparent.transport.noise]
remote_public_key = "{TEST_CLIENT_REMOTE_PUBLIC}"

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

[transparent.transport.noise]
remote_public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="

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
