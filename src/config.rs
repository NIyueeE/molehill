pub mod parsing;
pub mod transparent;
pub mod watcher;

#[cfg(any(feature = "client", feature = "hot-reload"))]
pub use parsing::{ClientConfig, ClientServiceConfig};
// Server-side names are only consumed by the server run mode
// (ServerControlConfig/ServerTransportConfig stay internal to parsing).
#[cfg(feature = "server")]
pub use parsing::ServerConfig;
pub use parsing::{Config, MaskedString, ServiceType, TransportConfig, TransportType};
// Only used by the noise transport: it wraps the data-plane stream of any
// build that has it, which is why this is gated on `noise` alone — the noise
// transport itself compiles without `multiplex` (the `embedded` feature set
// depends on that).
#[cfg(feature = "noise")]
pub use parsing::NoiseConfig;

pub use watcher::{ConfigChange, ConfigWatcherHandle};
// Service-level change events, consumed by the matching run mode
#[cfg(all(feature = "client", feature = "hot-reload"))]
pub use watcher::ClientServiceChange;
