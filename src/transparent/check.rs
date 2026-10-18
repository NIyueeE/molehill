//! Startup prerequisites: what the operator must have prepared, checked here
//! so a misconfiguration is one precise message instead of silent packet loss.
//!
//! The daemon deliberately does not configure the network (no netlink, no
//! `ip` calls); it verifies what it depends on and says exactly what to run.

#[cfg(mh_linux)]
use std::net::IpAddr;
#[cfg(mh_linux)]
use std::path::Path;

#[cfg(not(mh_linux))]
use anyhow::Result;
#[cfg(mh_linux)]
use anyhow::{Context, Result, bail};
#[cfg(not(mh_linux))]
use std::net::IpAddr;

/// The TUN interface must already exist: the operator's addresses and routes
/// live on it, and a device this process creates would be missing both.
#[cfg(mh_linux)]
pub fn require_interface(name: &str) -> Result<()> {
    require_interface_at(Path::new("/sys/class/net"), name)
}

/// The filesystem-taking half of [`require_interface`], so tests can use a
/// temporary directory instead of the host's `/sys`.
#[cfg(mh_linux)]
fn require_interface_at(root: &Path, name: &str) -> Result<()> {
    if !root.join(name).exists() {
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
#[cfg(mh_linux)]
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
    check_forwarding(Path::new(path), key, sysctl)
}

/// Read one forwarding sysctl. A missing path is not a failure: the check is
/// only meaningful where `/proc` is readable, and the interface check remains
/// the hard prerequisite.
#[cfg(mh_linux)]
fn check_forwarding(path: &Path, key: &str, sysctl: &str) -> Result<()> {
    let Ok(value) = std::fs::read_to_string(path) else {
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
#[cfg(mh_linux)]
pub fn check_client(tun: &str, claimed: &[IpAddr]) -> Result<()> {
    // IPv4 reverse-path filtering drops the injected packets; IPv6 has no
    // rp_filter knob, so only a v4 claim makes this check meaningful.
    if claimed.iter().any(IpAddr::is_ipv4) {
        check_rp_filter(tun)?;
    }
    let local = local_addresses()?;
    check_claimed_addresses(tun, claimed, &local)
}

/// Compare the claims against the addresses the host actually carries. Pure
/// apart from the error message, so both address families and the exact
/// commands can be tested without touching the host's interfaces.
#[cfg(mh_linux)]
fn check_claimed_addresses(tun: &str, claimed: &[IpAddr], local: &[IpAddr]) -> Result<()> {
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
#[cfg(mh_linux)]
fn check_rp_filter(tun: &str) -> Result<()> {
    for path in [
        format!("/proc/sys/net/ipv4/conf/{tun}/rp_filter"),
        "/proc/sys/net/ipv4/conf/all/rp_filter".to_string(),
    ] {
        let key = path.trim_start_matches("/proc/sys/").replace('/', ".");
        check_rp_filter_at(Path::new(&path), &key)?;
    }
    Ok(())
}

/// Read one `rp_filter` knob by path. Missing is fine; `1` is the failure
/// this check exists to catch.
#[cfg(mh_linux)]
fn check_rp_filter_at(path: &Path, key: &str) -> Result<()> {
    let Ok(value) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    if value.trim() != "0" {
        bail!(
            "Reverse-path filtering is on ({key} = {}), so injected packets would be \
             dropped. Turn it off:\n  sudo sysctl -w {key}=0",
            value.trim()
        );
    }
    Ok(())
}

#[cfg(mh_linux)]
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
#[cfg(not(mh_linux))]
pub fn require_interface(_name: &str) -> Result<()> {
    Ok(())
}

/// macOS and Windows own forwarding at the operator level (a system sysctl or
/// Windows routing/firewall configuration), and neither exposes a safe
/// in-process equivalent of the Linux `/proc` check. The deployment page owns
/// those recipes.
#[cfg(not(mh_linux))]
pub fn check_server(_claimed: IpAddr) -> Result<()> {
    Ok(())
}

#[cfg(all(test, mh_linux))]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A private scratch directory per test (the suite is serial, but unique
    /// names keep a rerun from reusing a stale file).
    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("molehill-check-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn an_interface_must_exist_and_the_message_names_the_recipe() {
        let dir = scratch("interface");
        let err = require_interface_at(&dir, "l3test0").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("does not exist"), "{message}");
        assert!(message.contains("ip tuntap add dev l3test0"), "{message}");

        fs::write(dir.join("l3test0"), b"").unwrap();
        require_interface_at(&dir, "l3test0").unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn forwarding_must_be_on_and_a_missing_proc_file_is_tolerated() {
        let dir = scratch("forwarding");
        let path = dir.join("ip_forward");

        // The practical answer on a host without /proc: not this check's
        // failure.
        check_forwarding(&path, "net.ipv4.ip_forward", "net.ipv4.ip_forward=1").unwrap();

        fs::write(&path, b"1\n").unwrap();
        check_forwarding(&path, "net.ipv4.ip_forward", "net.ipv4.ip_forward=1").unwrap();

        fs::write(&path, b"0\n").unwrap();
        let err =
            check_forwarding(&path, "net.ipv4.ip_forward", "net.ipv4.ip_forward=1").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("net.ipv4.ip_forward = 0"), "{message}");
        assert!(
            message.contains("sudo sysctl -w net.ipv4.ip_forward=1"),
            "{message}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rp_filter_must_be_zero_and_a_missing_key_is_tolerated() {
        let dir = scratch("rp_filter");
        let path = dir.join("rp_filter");
        let key = "net.ipv4.conf.l3test0.rp_filter";

        check_rp_filter_at(&path, key).unwrap();
        fs::write(&path, b"0\n").unwrap();
        check_rp_filter_at(&path, key).unwrap();

        fs::write(&path, b"1\n").unwrap();
        let err = check_rp_filter_at(&path, key).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains(key), "{message}");
        assert!(
            message.contains(&format!("sudo sysctl -w {key}=0")),
            "{message}"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_claim_must_be_an_address_the_host_carries() {
        let v4: IpAddr = "10.99.0.1".parse().unwrap();
        let v6: IpAddr = "2001:db8::1".parse().unwrap();

        check_claimed_addresses("l3test0", &[v4, v6], &[v4, v6]).unwrap();

        let err = check_claimed_addresses("l3test0", &[v4], &[]).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("sudo ip addr add 10.99.0.1/32 dev l3test0"),
            "{message}"
        );
        assert!(
            message.contains("sudo ip rule add from 10.99.0.1 lookup 100"),
            "{message}"
        );
        assert!(
            message.contains("sudo ip route add default dev l3test0 table 100"),
            "{message}"
        );

        let err = check_claimed_addresses("l3test0", &[v6], &[]).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("sudo ip -6 addr add 2001:db8::1/128 dev l3test0"),
            "{message}"
        );
        assert!(
            message.contains("sudo ip -6 rule add from 2001:db8::1 lookup 100"),
            "{message}"
        );
    }
}
