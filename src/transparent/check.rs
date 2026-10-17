//! Startup prerequisites: what the operator must have prepared, checked here
//! so a misconfiguration is one precise message instead of silent packet loss.
//!
//! The daemon deliberately does not configure the network (no netlink, no
//! `ip` calls); it verifies what it depends on and says exactly what to run.

#[cfg(target_os = "linux")]
use std::net::IpAddr;
#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(not(target_os = "linux"))]
use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::{Context, Result, bail};
#[cfg(not(target_os = "linux"))]
use std::net::IpAddr;

/// The TUN interface must already exist: the operator's addresses and routes
/// live on it, and a device this process creates would be missing both.
#[cfg(target_os = "linux")]
pub fn require_interface(name: &str) -> Result<()> {
    if !Path::new("/sys/class/net").join(name).exists() {
        bail!(
            "Interface {name} does not exist. Prepare it first (docs/deployment.md, \
             \"Transparent services\"):\n  \
             sudo ip tuntap add dev {name} mode tun\n  \
             sudo ip link set {name} up mtu 1400"
        );
    }
    Ok(())
}

/// The server's contract for serving claims: it must be willing to *forward*.
///
/// The visitor's packets reach this host addressed to the claimed address and
/// are routed into the TUN device — that is forwarding, and the kernel does it
/// only when the family's `forwarding` sysctl is on. With it off, every packet
/// for every claim is dropped by the kernel before this process reads a byte:
/// the daemon sees a healthy TUN and a silent device, which is exactly the
/// "one precise message instead of silent packet loss" failure this module
/// exists to prevent. Checked at the first transparent registration, where
/// the interface check already runs.
#[cfg(target_os = "linux")]
pub fn check_server(claimed: IpAddr) -> Result<()> {
    let (path, key, sysctl) = match claimed {
        IpAddr::V4(_) => (
            "/proc/sys/net/ipv4/ip_forward",
            "net.ipv4.ip_forward",
            "net.ipv4.ip_forward=1",
        ),
        IpAddr::V6(_) => (
            "/proc/sys/net/ipv6/conf/all/forwarding",
            "net.ipv6.conf.all.forwarding",
            "net.ipv6.conf.all.forwarding=1",
        ),
    };
    let Ok(value) = std::fs::read_to_string(path) else {
        // Unreadable (a container without /proc, a sandbox): not this check's
        // to fail — the interface check above is the hard prerequisite, and a
        // host that runs a TUN server has networking already.
        return Ok(());
    };
    if value.trim() != "1" {
        bail!(
            "IP forwarding is off ({key} = {}), so the kernel drops every packet addressed \
             to a claim before it reaches the TUN device. Turn it on:\n  sudo sysctl -w {sysctl}",
            value.trim()
        );
    }
    Ok(())
}

/// The client's contract: it owns the claimed addresses, and the kernel is
/// willing to accept packets whose source is not on the local network.
#[cfg(target_os = "linux")]
pub fn check_client(tun: &str, claimed: &[IpAddr]) -> Result<()> {
    // IPv4 reverse-path filtering drops the injected packets; IPv6 has no
    // rp_filter knob, so only a v4 claim makes this check meaningful.
    if claimed.iter().any(IpAddr::is_ipv4) {
        check_rp_filter(tun)?;
    }
    let local = local_addresses()?;
    for ip in claimed {
        if local.contains(ip) {
            continue;
        }
        let ip_cmd = if ip.is_ipv6() { "ip -6" } else { "ip" };
        let prefix = if ip.is_ipv6() { 128 } else { 32 };
        bail!(
            "Transparent service claims {ip}, but no local interface carries it. The client \
             must own the address it claims — assign it to the tunnel:\n  \
             sudo {ip_cmd} addr add {ip}/{prefix} dev {tun}\n  \
             sudo {ip_cmd} rule add from {ip} lookup 100\n  \
             sudo {ip_cmd} route add default dev {tun} table 100"
        );
    }
    Ok(())
}

/// Reverse-path filtering drops exactly the packets this feature injects: ones
/// whose source address has no route back out of the interface they arrived
/// on (the visitor's address, arriving on the tunnel).
#[cfg(target_os = "linux")]
fn check_rp_filter(tun: &str) -> Result<()> {
    for path in [
        format!("/proc/sys/net/ipv4/conf/{tun}/rp_filter"),
        "/proc/sys/net/ipv4/conf/all/rp_filter".to_string(),
    ] {
        let Ok(value) = std::fs::read_to_string(&path) else {
            // A missing key is not a failure: this check exists to catch the
            // one that is set to 1.
            continue;
        };
        if value.trim() != "0" {
            let key = path.trim_start_matches("/proc/sys/").replace('/', ".");
            bail!(
                "Reverse-path filtering is on ({key} = {}), so injected packets would be \
                 dropped. Turn it off:\n  sudo sysctl -w {key}=0",
                value.trim()
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn local_addresses() -> Result<Vec<IpAddr>> {
    let addrs = nix::ifaddrs::getifaddrs().with_context(|| "Failed to list local addresses")?;
    Ok(addrs
        .filter_map(|interface| interface.address)
        .filter_map(|address| {
            if let Some(v4) = address.as_sockaddr_in() {
                Some(IpAddr::V4(v4.ip()))
            } else {
                address.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip()))
            }
        })
        .collect())
}

/// macOS and Windows create the device themselves; there is no pre-existing
/// interface for the daemon to require.
#[cfg(not(target_os = "linux"))]
pub fn require_interface(_name: &str) -> Result<()> {
    Ok(())
}

/// macOS and Windows own forwarding at the operator level (a system sysctl or
/// Windows routing/firewall configuration), and neither exposes a safe
/// in-process equivalent of the Linux `/proc` check. The deployment page owns
/// those recipes.
#[cfg(not(target_os = "linux"))]
pub fn check_server(_claimed: IpAddr) -> Result<()> {
    Ok(())
}
