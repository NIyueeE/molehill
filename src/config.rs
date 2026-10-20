pub mod parsing;
pub mod transparent;
pub mod watcher;

#[cfg(any(feature = "client", feature = "hot-reload"))]
pub use parsing::{ClientConfig, ClientServiceConfig, StatusConfig};
// Server-side names are only consumed by the server run mode
// (ServerControlConfig/ServerTransportConfig stay internal to parsing).
#[cfg(feature = "server")]
pub use parsing::ServerConfig;
pub use parsing::{Config, MaskedString, ServiceType, TransportConfig, TransportType};
// The key material is part of the configuration schema even in a build
// without the `noise` feature (the client still has to parse a config that
// names a pair); only the handshake itself is feature-gated.
pub use parsing::NoiseConfig;

pub use watcher::{ConfigChange, ConfigWatcherHandle};
// Service-level change events, consumed by the matching run mode
#[cfg(all(feature = "client", feature = "hot-reload"))]
pub use watcher::ClientServiceChange;
