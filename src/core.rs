#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub use client::run_client;
#[cfg(feature = "server")]
pub use server::{control_sessions_accepted, run_server};
