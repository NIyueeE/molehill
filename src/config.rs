pub mod parsing;
pub mod watcher;

#[cfg(any(feature = "client", feature = "notify"))]
pub use parsing::{ClientConfig, ClientServiceConfig, HealthCheckConfig, HealthCheckType};
// Public API re-exports: some names are only consumed by feature-gated
// modules, so client-only builds would otherwise warn about `ServerConfig`.
#[allow(unused_imports)]
pub use parsing::{
    Config, MaskedString, ServerConfig, ServiceType, TcpConfig, TransportConfig, TransportType,
};
// Only used by the TLS transports, which are not compiled in embedded builds
#[cfg(any(feature = "native-tls", feature = "rustls"))]
pub use parsing::TlsConfig;
// Only used by the noise transport
#[cfg(feature = "noise")]
pub use parsing::NoiseConfig;

pub use watcher::{ConfigChange, ConfigWatcherHandle};
// Service-level change events, consumed by the matching run mode
#[cfg(all(feature = "client", feature = "notify"))]
pub use watcher::ClientServiceChange;
