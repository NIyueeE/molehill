pub mod constants;
pub mod helper;
/// The owned-write boundary, used by both transports that need to hand their
/// writer across a channel (`noise`'s record stream and `mhd`'s, which is a mux
/// tunnel over UDP). `mhd` implies `multiplex` but not `noise`, and neither
/// implies a side, so the gate is exactly the two transports that use it — the
/// old `server` gate was a leftover from where it was first introduced and
/// broke `client,mhd`.
#[cfg(any(feature = "noise", feature = "mhd"))]
pub mod owned_write;
