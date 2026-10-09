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
use crate::config::parsing::{ClientDataConfig, DataCarrier, DataMode};

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
    pub data: ClientDataConfig,
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
    /// Override `[transparent.data].default_mode` for this claim only.
    #[cfg(feature = "multiplex")]
    pub mode: Option<DataMode>,
    /// Override `[transparent.data].default_carrier` for this claim only;
    /// valid only with `mode = "multiplex"`.
    #[cfg(feature = "multiplex")]
    pub carrier: Option<DataCarrier>,
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
                    service.mode = claim.mode;
                    service.carrier = claim.carrier;
                }
                (name.clone(), service)
            })
            .collect();

        ClientConfig {
            default_token: self.default_token.clone(),
            control: self.control.clone(),
            #[cfg(feature = "multiplex")]
            data: self.data.clone(),
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
        assert_eq!(client.transport.transport_type, TransportType::Plain);
        assert!(client.transport.noise.is_none());
        assert_eq!(
            client.transport.proxy.as_ref().map(Url::as_str),
            Some("socks5://127.0.0.1:1080")
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
