pub mod parsing;
pub mod watcher;

#[cfg(any(feature = "client", feature = "notify"))]
pub use parsing::{ClientConfig, ClientServiceConfig};
pub use parsing::{Config, ServiceType, TcpConfig, TransportConfig, TransportType};
#[cfg(any(feature = "server", feature = "notify"))]
pub use parsing::{ServerConfig, ServerServiceConfig};
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
#[cfg(all(feature = "server", feature = "notify"))]
pub use watcher::ServerServiceChange;
