pub mod parsing;
pub mod watcher;

#[cfg(any(feature = "client", feature = "notify"))]
pub use parsing::{ClientConfig, ClientServiceConfig, HealthCheckConfig, HealthCheckType};
// Server-side names are only consumed by the server run mode
// (ServerControlConfig/ServerTransportConfig stay internal to parsing).
#[cfg(feature = "server")]
pub use parsing::ServerConfig;
pub use parsing::{Config, MaskedString, ServiceType, TransportConfig, TransportType};
// Only meaningful together with the data-plane fields they select between
#[cfg(feature = "multiplex")]
pub use parsing::{DataCarrier, DataMode};
// Only used by the noise transport
#[cfg(feature = "noise")]
pub use parsing::NoiseConfig;

pub use watcher::{ConfigChange, ConfigWatcherHandle};
// Service-level change events, consumed by the matching run mode
#[cfg(all(feature = "client", feature = "notify"))]
pub use watcher::ClientServiceChange;
