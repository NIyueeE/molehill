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

use crate::common::constants::DEFAULT_CLAIM_LANES;
#[cfg(feature = "multiplex")]
use crate::config::parsing::ClientDataConfig;
use crate::config::parsing::{
    ClientConfig, ClientControlConfig, ClientNoiseConfig, ClientServiceConfig, MaskedString,
    ServiceNoiseConfig, ServiceType, default_tun_name,
};

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
    /// The TUN device this client attaches to. On Linux the daemon creates a
    /// missing device and assigns each claimed address that is not already on
    /// it; an existing device is used as it is, so pre-create it to choose its
    /// MTU. The operator owns the policy rule and routes. On macOS the daemon
    /// creates the utun and assigns the claimed addresses, while the operator
    /// owns the pf rule and routes. `auto` (the macOS default) picks a free
    /// `utunN`.
    #[serde(default = "default_tun_name")]
    pub tun: String,
    #[serde(default)]
    pub control: ClientControlConfig,
    #[serde(default)]
    pub data: TransparentDataConfig,
    /// The KK key pair the control channel always uses, and the default for
    /// the lanes: `[transparent.noise].data_enable` defaults to `false`, so
    /// a lane carries the visitor's packets as they are unless asked otherwise.
    pub noise: ClientNoiseConfig,
    /// The optional read-only status API (`[transparent.status]`), the same
    /// shape as `[client.status]`: absent means off, present needs a bind
    /// address and a non-empty bearer token.
    #[serde(default)]
    pub status: Option<crate::config::parsing::StatusConfig>,
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
    /// The public address this client **claims**, e.g. `"10.99.0.1:8443"` or
    /// `"[2001:db8::1]:8443"`.
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
    /// Override `[transparent.control].retry_backoff_max` for this claim.
    pub retry_backoff_max: Option<u64>,
    /// Per-claim Noise overrides: a claim may dial another server with its own
    /// key pair, or flip its lanes' encryption without touching its siblings.
    pub noise: Option<ServiceNoiseConfig>,
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
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct TransparentDataConfig {
    /// Data-plane endpoint; defaults to the claim's control endpoint. Applies
    /// to every claim that does not name its own `remote_addr`.
    pub default_data_addr: Option<String>,
    /// The **lane budget**: how many carrier connections this client's claims
    /// hold together, divided equally among them. Unset means
    /// [`DEFAULT_CLAIM_LANES`] per claim (two, the measured knee of the
    /// throughput curve — see that constant for the numbers and the cost); a
    /// value below the claim count is refused.
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
const fn lend_by_default() -> bool {
    true
}

/// Written out rather than derived: `bool`'s own default is `false`, so a
/// derived one would turn lending *off* for every config that omits the table
/// — the opposite of the key's documented default, and invisible in the schema.
impl Default for TransparentDataConfig {
    fn default() -> Self {
        Self {
            default_data_addr: None,
            tunnels: None,
            lend: lend_by_default(),
        }
    }
}

impl TransparentClientConfig {
    /// How many claims share the lane budget.
    pub(crate) fn claims(&self) -> usize {
        self.claims.len()
    }

    /// The lane budget as the operator wrote it, or `None`.
    pub(crate) fn written_budget(&self) -> Option<usize> {
        self.data.tunnels.map(usize::from)
    }

    /// The lane budget: what the operator wrote, else
    /// [`DEFAULT_CLAIM_LANES`] per claim.
    ///
    /// The key is what spreads a busy claim's inner flows over more
    /// connections, and the default is the measured knee of that curve rather
    /// than the smallest value that starts ([`DEFAULT_CLAIM_LANES`] carries the
    /// numbers and the cost). A client that serves many claims and would rather
    /// not pay for lanes it cannot use writes `tunnels` and gets exactly the
    /// split it asks for.
    pub(crate) fn lane_budget(&self) -> usize {
        match self.written_budget() {
            Some(written) => written,
            None => self.claims().max(1) * usize::from(DEFAULT_CLAIM_LANES),
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
        // The control channel is always Noise with this table's KK pair; a
        // lane defaults to plain because what it carries is the visitor's own
        // packets (the visitor's TLS is the end-to-end protection). A claim's
        // own noise block travels as an override on its service, so the engine
        // resolves one pair and one switch per service without knowing this
        // model exists.
        let data_enable = self.noise.data_enable.unwrap_or(false);

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
                service.retry_backoff_max = claim.retry_backoff_max;
                // The claim's own keys and encryption switch travel as an
                // override; unset fields inherit the transparent client's.
                service.noise.clone_from(&claim.noise);
                // The claim's lane count, resolved here so the engine has one
                // number to read: an equal share of the lane budget, never
                // below the one connection every claim has. A claim's own
                // `tunnels` does not divide the budget — the budget belongs to
                // the client — so it is a refusal when written on a claim, and
                // the schema is where that is said.
                service.transparent_lanes = u16::try_from(self.claim_lanes()).unwrap_or(u16::MAX);
                service.transparent_lend = self.data.lend;
                (name.clone(), service)
            })
            .collect();

        ClientConfig {
            default_token: self.default_token.clone(),
            control: self.control.clone(),
            #[cfg(feature = "multiplex")]
            data: ClientDataConfig {
                default_data_addr: self.data.default_data_addr.clone(),
                // A claim never draws from a pool, so pool width has nothing
                // to select here, and the lane budget travels per claim
                // (`transparent_lanes`): this block carries only the data
                // endpoint.
                tunnels: None,
            },
            services,
            // The L3 client runs the same status listener as a forwarding one:
            // the answer already carries its claims and lanes.
            status: self.status.clone(),
            noise: ClientNoiseConfig {
                keys: self.noise.keys.clone(),
                data_enable: Some(data_enable),
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

#[cfg(all(test, mh_l3_client))]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::TransparentClientConfig;
    use crate::common::constants::DEFAULT_CLAIM_LANES;
    use crate::config::parsing::Config;
    use crate::config::parsing::{ServiceType, TEST_CLIENT_PRIVATE, TEST_SERVER_PUBLIC};

    /// The lowering is the whole of the model difference, so it is pinned key
    /// by key: a claim becomes a service whose *claimed* address stands in for
    /// `local_addr` (nothing is dialed), carrying the device the data path
    /// attaches to. The wire stacks split here: the control channel is Noise
    /// (its keys are this table's), the lanes are plain by default — the
    /// visitor's TLS is the end-to-end protection — and the resolved pair
    /// travels per service so the engine's overlay dials each with its own.
    #[test]
    fn a_claim_lowers_to_a_transparent_service() {
        let config = Config::from_str(
            r#"
[transparent]
default_token = "t"
tun = "l3test0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

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
            service.transparent_lanes, DEFAULT_CLAIM_LANES,
            "a claim starts at the default lane count unless the budget is raised"
        );
        assert!(
            service.transparent_lend,
            "lending is on unless the client turns it off: `[transparent.data].lend` defaults \
             to true, so a config that never mentions it gets the mechanism"
        );
        // The control leg is KK, always: its keys are the table's, and the
        // session nonce it publishes is what every lane authenticates with.
        assert_eq!(client.noise.keys.public_key, TEST_SERVER_PUBLIC);
        assert_eq!(&*client.noise.keys.private_key, TEST_CLIENT_PRIVATE);
        // The lanes default to plain: the visitor's packets go as they are.
        assert_eq!(
            client.noise.data_enable,
            Some(false),
            "a transparent client's lanes are plain unless asked otherwise"
        );
        assert!(
            service.noise.is_none(),
            "a claim with no override inherits the client-wide pair and switch"
        );
        assert!(!service.data_enabled_with(false));
    }

    /// `type = "noise"` on the lanes: the table's keys are the control
    /// channel's and the lanes' at once (one pair per host), and the per-service
    /// overlay carries the resolved pair so a lane dials Noise without the
    /// engine knowing this model exists.
    #[test]
    fn noise_lanes_inherit_the_table_keys() {
        let config = Config::from_str(
            r#"
[transparent]
default_token = "t"
tun = "l3test0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.noise]
data_enable = true
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#,
        )
        .unwrap();

        let client = config.into_l3_client().unwrap().client.unwrap();
        // Control: KK with the table's keys (the same pair the lanes use).
        assert_eq!(client.noise.keys.public_key, TEST_SERVER_PUBLIC);
        assert_eq!(&*client.noise.keys.private_key, TEST_CLIENT_PRIVATE);
        // Lanes: encryption is on for this client, inherited by the claim.
        assert_eq!(client.noise.data_enable, Some(true));
        let service = client.services.get("web").unwrap();
        assert!(service.data_enabled_with(true));
    }

    /// A claim's lanes are an equal share of its lane budget: two each when
    /// the key is unwritten, and the written budget divided among the claims
    /// that draw on that carrier.
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

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

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
            DEFAULT_CLAIM_LANES,
            "an unwritten budget is the default per claim: the budget is claims x that"
        );
        assert_eq!(
            lanes("", &two).0,
            DEFAULT_CLAIM_LANES,
            "and an unwritten budget does not divide a client's spare lanes onto it: every \
             claim starts with the same share, whatever its siblings are doing"
        );
        assert_eq!(
            lanes("", &two).1,
            Some(DEFAULT_CLAIM_LANES),
            "the default share is per claim, not a pool divided among them"
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
    #[test]
    fn lending_is_on_unless_the_client_turns_it_off() {
        let base = r#"
[transparent]
default_token = "t"
tun = "l3test0"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

"#;
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

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

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

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

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

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

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

    /// A claim never multiplexes: its data channels are carrier connections of
    /// its own, which is what `ClientServiceConfig::uses_pool` derives from the
    /// service type — the shape is a property of the model, not a key.
    /// A block that never went through `Config::from_str` has no lowering;
    /// the runtime reads `client_config()` and must get a named error rather
    /// than an empty client.
    #[test]
    fn an_unvalidated_block_has_no_lowered_client() {
        let block = TransparentClientConfig::default();
        let err = block.client_config().unwrap_err();
        assert!(format!("{err:#}").contains("not validated"), "{err:#}");
    }

    #[test]
    fn a_claim_never_multiplexes() {
        let config = Config::from_str(
            r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

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

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

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

    /// A claim's lane count is its share of the client's budget, never its own
    /// key: the budget belongs to the carrier the claims share. The key is
    /// refused with the budget key to write instead.
    #[test]
    fn a_claim_cannot_size_its_own_lanes() {
        let err = format!(
            "{:#}",
            Config::from_str(
                r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
tunnels = 2
"#
            )
            .unwrap_err()
        );
        assert!(
            err.contains("transparent.claims.*.tunnels")
                && err.contains("[transparent.data].tunnels"),
            "the refusal must name the removed key and the budget: {err}"
        );
    }

    /// Both retry keys are named for what they are — a cap — so an old
    /// `retry_interval` is refused with the key to write instead, at both
    /// levels the L3 model has.
    #[test]
    fn retry_caps_are_named_for_what_they_do() {
        let err = format!(
            "{:#}",
            Config::from_str(
                r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"
default_retry_interval = 3

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
retry_interval = 2
"#
            )
            .unwrap_err()
        );
        assert!(
            err.contains("transparent.control.default_retry_interval")
                && err.contains("transparent.claims.*.retry_interval")
                && err.contains("retry_backoff_max"),
            "the refusal must name both removed keys and the replacement: {err}"
        );
    }

    /// The status API is part of the L3 model too: the same two keys under
    /// `[transparent.status]`, lowered onto the client the L3 run executes.
    #[test]
    fn a_transparent_client_can_serve_the_status_api() {
        let config = Config::from_str(
            r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

[transparent.status]
bind_addr = "127.0.0.1:9091"
token = "status-token"

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#,
        )
        .unwrap();
        let client = config.into_l3_client().unwrap().client.unwrap();
        let status = client.status.unwrap();
        assert_eq!(status.bind_addr, "127.0.0.1:9091");
        assert_eq!(&*status.token, "status-token");

        // Half a credential is refused, and the message names the block the
        // reader wrote.
        let err = format!(
            "{:#}",
            Config::from_str(
                r#"
[transparent]
default_token = "t"

[transparent.control]
default_remote_addr = "example.com:2333"

[transparent.noise]
public_key = "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
private_key = "8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="

[transparent.status]
bind_addr = "127.0.0.1:9091"
token = ""

[transparent.claims.web]
remote_bind_addr = "10.99.0.1:8443"
"#
            )
            .unwrap_err()
        );
        assert!(
            err.contains("[transparent].status.token"),
            "the refusal must name the block the reader wrote: {err}"
        );
    }
}
