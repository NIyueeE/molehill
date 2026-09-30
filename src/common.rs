pub mod constants;
#[cfg(any(feature = "client", feature = "server"))]
pub mod forward;
pub mod helper;
/// The owned-write boundary, used by both transports that need to hand their
/// writer across a channel (`noise`'s record stream and `kcp`'s, which is a mux
/// tunnel over UDP). `kcp` implies `multiplex` but not `noise`, and neither
/// implies a side, so the gate is exactly the two transports that use it — the
/// old `server` gate was a leftover from where it was first introduced and
/// broke `client,kcp`.
#[cfg(any(feature = "noise", feature = "kcp"))]
pub mod owned_write;
