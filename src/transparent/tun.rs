//! The TUN device: creating or attaching to the interface, and reading/writing
//! whole frames.
//!
//! The daemon never creates routes. On Linux it creates a missing persistent
//! TUN and otherwise attaches to the operator's device; on macOS and Windows
//! the device is created by the daemon because those platforms have no
//! persistent operator-created equivalent. The operator owns the routes and
//! forwarding rules around it.
//!
//! The device layer is `tun-rs`, which gives one interface across Linux,
//! macOS and Windows and keeps the platform's unsafe syscalls in the
//! dependency rather than in this crate. The local frame shape is reported as
//! a [`FrameFormat`](crate::protocol::FrameFormat): a Linux device with
//! `IFF_VNET_HDR`/segmentation offload produces vnet frames (the 10-byte
//! virtio-net header plus packet or GSO super-frame); macOS, Windows and a
//! Linux device without offload produce bare IPv4 or IPv6 packets.
//!
//! The daemon never checksums, segments or routes anything at this layer: a
//! frame is read and written whole. The one exception is the wire-format
//! conversion in [`frame`](super::frame), which lives above this module.

use std::io;
#[cfg(mh_macos)]
use std::net::IpAddr;

use anyhow::{Context, Result, bail};
use tun_rs::{AsyncDevice, DeviceBuilder};

use crate::protocol::FrameFormat;

/// The largest frame a TUN read is willing to see: the virtio-net header plus
/// a GSO super-frame. The ceiling is the kernel's own ~64 KiB budget, with
/// slack for growth.
pub use crate::transparent::frame::MAX_FRAME as MAX_PACKET;

/// One attached TUN interface.
pub struct Tun {
    device: AsyncDevice,
    format: FrameFormat,
    name: String,
}

impl Tun {
    /// Attach to (or, on macOS/Windows, create) the interface `name`.
    ///
    /// Linux expects the interface to exist: `net::prepare_device` creates a
    /// missing one before this constructor. macOS accepts `"auto"` (a free
    /// `utunN`) or an explicit `utunN`; Windows uses the adapter `name`.
    pub fn attach(name: &str) -> Result<Self> {
        let (device, format) = build(name)?;
        let actual = device
            .name()
            .with_context(|| format!("Failed to read the name of TUN device {name}"))?;
        Ok(Self {
            device,
            format,
            name: actual,
        })
    }

    /// The local frame shape this device produces and consumes.
    pub fn format(&self) -> FrameFormat {
        self.format
    }

    /// The actual interface name, which can differ from the configured one on
    /// macOS (`auto` resolves to a free `utunN`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Wait until at least one frame is ready, then hand **every** frame the
    /// device has to `visit`, and return once it runs dry.
    ///
    /// This is the batching window. It ends the moment the device has nothing
    /// left to give, so a batch never waits on a timer and an idle path is
    /// handed over (and written) as promptly as a single-packet read would
    /// have been: what changes is how many packets share one hand-over, not
    /// when the first of them moves.
    pub async fn drain(&self, buf: &mut [u8], visit: &mut impl FnMut(&[u8])) -> io::Result<()> {
        self.device.readable().await?;
        loop {
            match self.device.try_recv(buf) {
                Ok(read) => visit(&buf[..read]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    /// Write one frame: the caller supplies the **whole** frame in this
    /// device's own [`format`](Self::format).
    ///
    /// A short write would split a frame, so it is an error rather than
    /// something to retry.
    pub async fn write_packet(&self, frame: &[u8]) -> io::Result<()> {
        loop {
            match self.device.send(frame).await {
                Ok(written) if written == frame.len() => return Ok(()),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!(
                            "the TUN device accepted part of a {}-byte frame; a frame cannot \
                             be written in pieces",
                            frame.len()
                        ),
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Assign a claimed IPv4 or IPv6 address to the daemon-created device.
    ///
    /// Only macOS needs this: its utun exists only while the process holds it,
    /// so the address cannot be prepared before the device exists. The Linux
    /// client assigns a missing claim address in
    /// [`crate::transparent::net::claim_client`]; Windows servers route the
    /// claim rather than owning it. Re-adding an address already present is
    /// not an error, which makes a config reload idempotent.
    #[cfg(mh_macos)]
    pub fn ensure_claimed(&self, ip: IpAddr) -> Result<()> {
        let result = match ip {
            IpAddr::V4(ip) => self
                .device
                .add_address_v4(ip, 32)
                .map(|()| format!("{ip}/32")),
            IpAddr::V6(ip) => self
                .device
                .add_address_v6(ip, 128)
                .map(|()| format!("{ip}/128")),
        };
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Failed to assign {ip} to {}", self.name)),
        }
    }
}

#[cfg(mh_linux)]
fn build(name: &str) -> Result<(AsyncDevice, FrameFormat)> {
    if name.is_empty() || name.len() >= libc::IFNAMSIZ {
        bail!(
            "TUN interface name {name:?} must be 1 to {} bytes",
            libc::IFNAMSIZ - 1
        );
    }
    if name.as_bytes().contains(&0) {
        bail!("TUN interface name {name:?} contains a NUL byte");
    }

    match DeviceBuilder::new().name(name).offload(true).build_async() {
        Ok(device) => Ok((device, FrameFormat::Vnet)),
        Err(offload_error) => {
            // A kernel without TUNSETOFFLOAD support (or a TUN that refuses the
            // declaration) still carries packets; it just hands over MTU-sized
            // reads. Fall back to plain IP rather than making L3 unavailable.
            tracing::info!(
                tun = name,
                "TUN offload unavailable ({offload_error}); using plain IP frames"
            );
            let device = DeviceBuilder::new()
                .name(name)
                .offload(false)
                .build_async()
                .with_context(|| {
                    format!(
                        "Failed to attach to TUN interface {name}. A transparent service needs \
                         the tun module (`modprobe tun`) and CAP_NET_ADMIN in the process's \
                         capability set"
                    )
                })?;
            Ok((device, FrameFormat::Ip))
        }
    }
}

#[cfg(mh_macos)]
fn build(name: &str) -> Result<(AsyncDevice, FrameFormat)> {
    let candidates: Vec<String> = if name == "auto" {
        (0..16).map(|i| format!("utun{i}")).collect()
    } else if name.starts_with("utun") && name[4..].bytes().all(|b| b.is_ascii_digit()) {
        vec![name.to_owned()]
    } else {
        bail!(
            "TUN interface name {name:?} is not valid on macOS: use `tun = \"auto\"` or an \
             explicit `utunN` (for example `utun7`); macOS creates the device when the daemon \
             starts"
        );
    };

    let mut last = None;
    for candidate in candidates {
        match DeviceBuilder::new()
            .name(&candidate)
            .associate_route(false)
            .build_async()
        {
            Ok(device) => return Ok((device, FrameFormat::Ip)),
            Err(e) => last = Some((candidate, e)),
        }
    }
    let (candidate, error) = last.expect("macOS candidate list is never empty");
    Err(error).with_context(|| {
        format!(
            "Failed to create a utun device (tried through {candidate}). macOS needs root \
             privileges for a utun device; choose another explicit `utunN` if the names are busy"
        )
    })
}

#[cfg(mh_windows)]
fn build(name: &str) -> Result<(AsyncDevice, FrameFormat)> {
    if name.is_empty() {
        bail!("TUN adapter name must not be empty");
    }
    let device = DeviceBuilder::new()
        .name(name)
        .build_async()
        .with_context(|| {
            format!(
                "Failed to create the Wintun adapter {name:?}. A transparent server on Windows \
                 needs Administrator privileges and `wintun.dll` next to the binary (or on PATH)"
            )
        })?;
    Ok((device, FrameFormat::Ip))
}

#[cfg(not(any(mh_linux, mh_macos, mh_windows)))]
fn build(_name: &str) -> Result<(AsyncDevice, FrameFormat)> {
    bail!("the transparent data path is not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vnet_ceiling_covers_the_kernel_budget() {
        const { assert!(MAX_PACKET >= crate::protocol::VNET_HDR_LEN + 65_550) };
    }
}
