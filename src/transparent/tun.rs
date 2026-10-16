//! The TUN device: attaching to the interface the operator prepared, and
//! reading/writing whole packets.
//!
//! The daemon never creates, addresses or routes the device — that belongs to
//! the operator ([deployment.md](../../docs/deployment.md) owns the recipes) —
//! which is what keeps this crate free of netlink and of shelling out to `ip`.
//!
//! ## The virtio-net header and the offload declaration
//!
//! The device is attached with `IFF_VNET_HDR` and then told, via
//! `TUNSETOFFLOAD`, which segmentation offloads this reader can take (TCP and
//! UDP, IPv4 and IPv6, checksum-agnostic). That pair is what lets the kernel
//! hand over a *super-frame* — one up-to-64 KiB unit whose virtio-net header
//! names a `gso_size`, carrying many MTU-sized packets — instead of segmenting
//! at the device boundary. Measured on this host (kernel 6.12, a routed veth
//! path, `tcp_tso_census2.py`): with the declaration, a bulk TCP flow arrives
//! as ~65 212-byte reads with `gso_type = TCPV4` (59 155 super-frames for 3.85
//! GB); without it, the same flow arrives as 1500-byte reads (576 071 of
//! them). The declaration is the difference between one read and ~44 reads
//! per 64 KiB, and it is what puts the transparent path's batch granularity
//! on the same footing the multiplexed path gets for free from the kernel's
//! TCP stack.
//!
//! Every frame on the wire then carries the 10-byte virtio-net header in
//! front of the packet (or the super-frame). The header is relayed **whole**:
//! a frame the reader received with `NEEDS_CSUM` is injected the same way, and
//! the kernel completes the checksum — this process never checksums anything.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, bail};
use tokio::io::unix::AsyncFd;

const TUN_DEVICE: &str = "/dev/net/tun";

/// `_IOW('T', 202, int)` — the request that binds an fd to a TUN interface.
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
/// `_IOW('T', 208, unsigned int)` — declares the offloads this reader takes.
const TUNSETOFFLOAD: libc::c_ulong = 0x4004_54d0;
/// A layer-3 tunnel: no Ethernet header, and no 4-byte packet-info prefix, so
/// a read returns exactly one IP packet (plus its virtio-net header).
const IFF_TUN: libc::c_short = 0x0001;
const IFF_NO_PI: libc::c_short = 0x1000;
/// Every read/write carries the 10-byte virtio-net header; the offload
/// declaration below is refused without it.
const IFF_VNET_HDR: libc::c_short = 0x4000;

/// `TUN_F_CSUM`: this reader can take packets whose checksum is not computed
/// (`NEEDS_CSUM`), which is the flag every other offload bit requires.
const TUN_F_CSUM: u32 = 0x01;
const TUN_F_TSO4: u32 = 0x02;
const TUN_F_TSO6: u32 = 0x04;
const TUN_F_TSO_ECN: u32 = 0x08;
const TUN_F_USO4: u32 = 0x20;
const TUN_F_USO6: u32 = 0x40;

/// The 10-byte virtio-net header every frame carries. Declared on the
/// framing side ([`crate::protocol::VNET_HDR_LEN`], which counts it inside
/// its length prefix) and re-exported here: this module is where frames are
/// read and written, the framing is where the number means something on the
/// wire.
pub use crate::protocol::VNET_HDR_LEN;

/// The largest frame a TUN read is willing to see: the virtio-net header plus
/// a GSO super-frame. The ceiling is the kernel's own 64 KiB budget
/// (`MAX_SKB_FRAGS`; 65 550 observed), with slack for growth.
pub const MAX_PACKET: usize = VNET_HDR_LEN + 66_000;

/// One attached TUN interface.
pub struct Tun {
    fd: AsyncFd<File>,
}

impl Tun {
    /// Attach to the existing TUN interface `name`.
    ///
    /// The interface itself is the operator's; see [`super::check`] for the
    /// checks that run before this one.
    pub fn attach(name: &str) -> Result<Self> {
        if name.is_empty() || name.len() >= libc::IFNAMSIZ {
            bail!(
                "TUN interface name {name:?} must be 1 to {} bytes",
                libc::IFNAMSIZ - 1
            );
        }
        if name.as_bytes().contains(&0) {
            bail!("TUN interface name {name:?} contains a NUL byte");
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(TUN_DEVICE)
            .with_context(|| {
                format!(
                    "Failed to open {TUN_DEVICE}. A transparent service needs the tun module \
                     (`modprobe tun`) and CAP_NET_ADMIN in the process's capability set"
                )
            })?;
        attach_interface(&file, name)
            .with_context(|| format!("Failed to attach to TUN interface {name}"))?;
        declare_offloads(&file).with_context(|| {
            format!(
                "The kernel refused the segmentation offloads for TUN interface {name}; \
                 this build requires a tun driver with TUNSETOFFLOAD support"
            )
        })?;
        // `AsyncFd::new` is deprecated (its `I/O-safety` hole: nothing pinned
        // the fd open across registration). The replacement is `unsafe` and
        // its contract is exactly what this function already guarantees by
        // construction: `file` is open, and it moves into the `AsyncFd` here
        // — owned by it for the struct's whole life — so the fd cannot close
        // or change under the registration.
        #[expect(
            unsafe_code,
            reason = "audited FFI-adjacent: `AsyncFd::register` demands an open fd that stays \
                      open and unchanged for the AsyncFd's life; `file` is the open TUN fd \
                      this function built and it moves into the guard below, which owns it \
                      exclusively until drop"
        )]
        // SAFETY: see the expect above — the fd is open, owned, and moves.
        let fd = unsafe { AsyncFd::register(file) }
            .with_context(|| "Failed to register the TUN fd with tokio")?;
        Ok(Self { fd })
    }

    /// Wait until at least one frame is ready, then hand **every** frame the
    /// device has to `visit`, and return once it runs dry.
    ///
    /// This is the batching window. It ends the moment the device has nothing
    /// left to give, so a batch never waits on a timer and an idle path is
    /// handed over (and written) as promptly as a single-packet read would
    /// have been: what changes is how many packets share one hand-over, not
    /// when the first of them moves. A reader that returned one packet at a
    /// time made the carrier pay a header, an acknowledgement and a syscall
    /// per packet — the cost this exists to amortise.
    ///
    /// `visit` sees each frame whole — virtio-net header first, then the
    /// packet or super-frame it describes. It cannot refuse a frame; one it
    /// cannot use is the caller's to drop and count.
    pub async fn drain(&self, buf: &mut [u8], visit: &mut impl FnMut(&[u8])) -> io::Result<()> {
        let mut guard = self.fd.readable().await?;
        loop {
            match guard.try_io(|inner| {
                let mut file = inner.get_ref();
                file.read(buf)
            }) {
                Ok(Ok(read)) => visit(&buf[..read]),
                // The queue is empty (or another reader took the last packet):
                // `try_io` has already cleared the readiness, so the next call
                // waits for the kernel again instead of spinning.
                Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => return Ok(()),
            }
        }
    }

    /// Write one frame: the caller supplies the **whole** frame — the
    /// virtio-net header first, exactly as it was read off the peer's device.
    /// The device is attached with `IFF_VNET_HDR`, so it expects the header in
    /// front of every write, and relaying the received header is what makes a
    /// `NEEDS_CSUM` frame checksum and a super-frame re-segment here.
    ///
    /// A short write would split a frame, so it is an error rather than
    /// something to retry.
    pub async fn write_packet(&self, frame: &[u8]) -> io::Result<()> {
        let mut guard = self.fd.writable().await?;
        loop {
            match guard.try_io(|inner| {
                let mut file = inner.get_ref();
                file.write(frame)
            }) {
                Ok(Ok(written)) if written == frame.len() => return Ok(()),
                Ok(Ok(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!(
                            "the TUN device accepted part of a {}-byte frame; a frame cannot \
                             be written in pieces",
                            frame.len()
                        ),
                    ));
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => {}
            }
        }
    }
}

/// Tell the kernel which offloads this reader accepts. With `IFF_VNET_HDR`
/// attached but no declaration, the tun driver's egress feature set has no
/// TSO/UFO and the stack segments before the reader sees anything — measured:
/// a bulk TCP flow arrived as 1500-byte reads until this was added, then as
/// ~65 KiB super-frames.
///
/// The USO bits are newer than some kernels; a refusal retries with the TCP
/// set, which every `VNET_HDR` kernel accepts.
const USO_SET: u32 = TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_TSO_ECN | TUN_F_USO4 | TUN_F_USO6;
const TCP_SET: u32 = TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_TSO_ECN;

fn declare_offloads(file: &File) -> io::Result<()> {
    #[expect(
        unsafe_code,
        reason = "audited FFI: TUNSETOFFLOAD is the kernel's only request for declaring the \
                  offloads a TUN reader takes. The argument is a plain u32 the header \
                  defines; the fd was bound to the device one call earlier"
    )]
    // SAFETY: the fd is an open TUN device fd (bound by `attach_interface`
    // before this runs), the request is the one `linux/if_tun.h` defines for
    // it, and the argument is a value the kernel reads by value.
    let accepted = |flags: u32| unsafe { libc::ioctl(file.as_raw_fd(), TUNSETOFFLOAD as _, flags) };
    if accepted(USO_SET) >= 0 || accepted(TCP_SET) >= 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

/// Bind `file` to the TUN interface `name`, creating it for this process's
/// lifetime when it does not exist.
fn attach_interface(file: &File, name: &str) -> Result<()> {
    #[expect(
        unsafe_code,
        reason = "audited FFI: TUNSETIFF is the only way to bind an fd to a TUN interface. \
                  The request is a plain C `ifreq` whose layout libc owns; the name is \
                  length-checked and NUL-checked by the caller, and the only flag written is \
                  one the kernel reads"
    )]
    // SAFETY: `ifreq` is a C struct for which all-zero is a valid value, the
    // name copy is bounded by IFNAMSIZ (and the buffer it copies into is
    // zeroed, so it stays NUL-terminated), and `ioctl` is called with the
    // request the kernel defines for exactly this struct.
    let result = unsafe {
        let mut request: libc::ifreq = std::mem::zeroed();
        for (slot, byte) in request.ifr_name.iter_mut().zip(name.bytes()) {
            *slot = byte.cast_signed();
        }
        request.ifr_ifru.ifru_flags = IFF_TUN | IFF_NO_PI | IFF_VNET_HDR;
        // The cast is to whatever this libc declares for `ioctl`'s request
        // parameter: `c_ulong` on glibc, `c_int` on musl. It is an identity
        // cast on glibc, and the constant is the same 32-bit value on both
        // (it fits `c_int` positively), so the request the kernel sees is
        // identical either way.
        libc::ioctl(file.as_raw_fd(), TUNSETIFF as _, &request)
    };

    if result < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| "TUNSETIFF failed (is the process missing CAP_NET_ADMIN?)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape this module promises the rest of the crate: a 10-byte
    /// header in front of every frame, and a read buffer that can hold the
    /// largest super-frame the kernel hands over.
    #[test]
    fn the_vnet_ceiling_covers_the_kernel_budget() {
        // 65550 is the largest frame observed on this host (64 KiB of
        // segments plus headers); the constant carries slack above it.
        const { assert!(MAX_PACKET >= VNET_HDR_LEN + 65_550) };
    }

    /// The all-zero virtio-net header is a valid `NONE` descriptor: flags 0,
    /// `gso_type NONE`, and the rest zero — what a relayed plain frame
    /// carries.
    #[test]
    fn the_plain_header_is_the_zero_header() {
        let header = [0u8; VNET_HDR_LEN];
        let (flags, gso_type, _hdr_len, gso_size, _cs, _co) = (
            header[0],
            header[1],
            u16::from_le_bytes([header[2], header[3]]),
            u16::from_le_bytes([header[4], header[5]]),
            u16::from_le_bytes([header[6], header[7]]),
            u16::from_le_bytes([header[8], header[9]]),
        );
        assert_eq!(flags, 0);
        assert_eq!(gso_type, 0); // VIRTIO_NET_HDR_GSO_NONE
        assert_eq!(gso_size, 0);
    }
}
