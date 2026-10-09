//! The TUN device: attaching to the interface the operator prepared, and
//! reading/writing whole packets.
//!
//! The daemon never creates, addresses or routes the device — that belongs to
//! the operator ([deployment.md](../../docs/deployment.md) owns the recipes) —
//! which is what keeps this crate free of netlink and of shelling out to `ip`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, bail};
use tokio::io::unix::AsyncFd;

const TUN_DEVICE: &str = "/dev/net/tun";

/// `_IOW('T', 202, int)` — the request that binds an fd to a TUN interface.
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
/// A layer-3 tunnel: no Ethernet header, and no 4-byte packet-info prefix, so
/// a read returns exactly one IP packet.
const IFF_TUN: libc::c_short = 0x0001;
const IFF_NO_PI: libc::c_short = 0x1000;

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
        let fd = AsyncFd::new(file).with_context(|| "Failed to register the TUN fd with tokio")?;
        Ok(Self { fd })
    }

    /// Read one packet. The buffer must be at least as large as the largest
    /// packet the interface accepts; a short buffer truncates the packet, so
    /// callers size it to [`super::ip::IPV4_MIN_HEADER`] plus the tunnel MTU
    /// with room to spare.
    pub async fn read_packet(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            match guard.try_io(|inner| {
                let mut file = inner.get_ref();
                file.read(buf)
            }) {
                Ok(result) => return result,
                // Spurious readiness: the kernel woke us for a packet another
                // reader took, or the queue drained. The loop simply waits
                // again.
                Err(_would_block) => {}
            }
        }
    }

    /// Write one packet. A short write would split a packet, so it is an error
    /// rather than something to retry.
    pub async fn write_packet(&self, packet: &[u8]) -> io::Result<()> {
        loop {
            let mut guard = self.fd.writable().await?;
            match guard.try_io(|inner| {
                let mut file = inner.get_ref();
                file.write(packet)
            }) {
                Ok(Ok(written)) if written == packet.len() => return Ok(()),
                Ok(Ok(written)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!(
                            "the TUN device accepted {written} of {} bytes; a packet cannot be \
                             written in pieces",
                            packet.len()
                        ),
                    ));
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => {}
            }
        }
    }
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
        request.ifr_ifru.ifru_flags = IFF_TUN | IFF_NO_PI;
        libc::ioctl(file.as_raw_fd(), TUNSETIFF, &request)
    };

    if result < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| "TUNSETIFF failed (is the process missing CAP_NET_ADMIN?)");
    }
    Ok(())
}
