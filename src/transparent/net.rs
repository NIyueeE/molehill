//! Linux device preparation for transparent services.
//!
//! The daemon owns the device step on every platform: Linux creates a missing
//! TUN device and, on the client, assigns the claimed addresses; an existing
//! device and an address already on it are left untouched. Every route, policy
//! rule and forwarding switch stays with the operator, exactly as it does on
//! macOS (pf, route, forwarding) and Windows (route, forwarding, firewall),
//! which is what keeps the three symmetric.
//!
//! Only Linux needs this module: macOS creates its `utunN` and assigns the
//! claimed addresses already, and Windows creates the Wintun adapter.
//!
//! Both steps are idempotent — the device is created only when missing and an
//! address already on it is left alone — so a restart, a reload or a second
//! claim converges without bouncing a live interface. A device the daemon
//! creates gets a conservative MTU; an operator who needs another one creates
//! the device before starting, and the daemon uses it as it is.

use std::net::SocketAddr;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Create the device when it is missing; use an existing one as it is.
///
/// The device is created persistent (what `ip tuntap add` leaves behind), so
/// the operator's routes survive a daemon restart. An existing device keeps
/// its MTU and up state: pre-create it to choose either.
pub(crate) fn prepare_device(tun: &str) -> Result<()> {
    let _gate = lock_gate();
    if crate::transparent::check::interface_exists(tun) {
        return Ok(());
    }
    tracing::info!(tun, mtu = DEFAULT_TUN_MTU, "Creating the TUN device");
    apply_locked(&device_plan(tun))
}

/// The MTU of a device this module creates. It fits the paths a tunnel
/// crosses; another value means the operator creates the device first.
const DEFAULT_TUN_MTU: u16 = 1400;

/// Assign a claimed address to the device.
///
/// The client owns the address the server never binds; assigning it here is
/// the same step macOS' daemon takes for its utun. The replies' route back
/// into the tunnel is the operator's policy rule, untouched here.
pub(crate) fn claim_client(tun: &str, addr: SocketAddr) -> Result<()> {
    let _gate = lock_gate();
    // A second claim on the same address, or a restart over the persistent
    // device, already has it; replacing it would bounce the address while an
    // earlier claim's lanes are live.
    if crate::transparent::check::interface_has_address(tun, addr.ip())? {
        return Ok(());
    }
    apply_locked(&client_claim_plan(tun, addr))
}

/// The device step: create and configure it only when it is missing, so an
/// existing device is never reconfigured.
fn device_plan(tun: &str) -> Vec<String> {
    vec![format!(
        "ip link show dev {tun} >/dev/null 2>&1 || \
         {{ ip tuntap add dev {tun} mode tun && ip link set dev {tun} up mtu {DEFAULT_TUN_MTU}; }}"
    )]
}

/// One claimed address on the device, for an address that is not there yet.
fn client_claim_plan(tun: &str, addr: SocketAddr) -> Vec<String> {
    let (ip_cmd, prefix) = if addr.is_ipv4() {
        ("ip", 32)
    } else {
        ("ip -6", 128)
    };
    vec![format!(
        "{ip_cmd} addr replace {}/{} dev {tun}",
        addr.ip(),
        prefix
    )]
}

/// Serializes the steps: two claims racing on `ip tuntap add` would both see
/// the device missing and one of them would fail.
static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The shared lock, with a poisoned mutex treated as available: the guarded
/// work is a sequence of idempotent host commands, so a panic in one claim
/// must not wedge the next.
fn lock_gate() -> std::sync::MutexGuard<'static, ()> {
    GATE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run the steps in order, stopping at the first failure. The caller holds
/// `GATE`.
fn apply_locked(steps: &[String]) -> Result<()> {
    for step in steps {
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(step)
            .output()
            .with_context(|| format!("Failed to run the host step: {step}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            if stderr.is_empty() {
                bail!("Host step failed: {step}");
            }
            bail!("Host step failed: {step}\n{stderr}");
        }
        tracing::debug!(step = %step, "host step applied");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;

    fn sip(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    /// Create and configure only when missing: an operator's persistent device
    /// is reused as it is, including its MTU.
    #[test]
    fn the_device_is_created_once_and_brought_up() {
        assert_eq!(
            device_plan("molehill0"),
            [
                "ip link show dev molehill0 >/dev/null 2>&1 || { ip tuntap add dev molehill0 mode tun && ip link set dev molehill0 up mtu 1400; }"
            ]
        );
    }

    /// The client's claimed addresses are the only address state this mode
    /// owns; each is a host prefix on the device, assigned when it is absent.
    #[test]
    fn a_claimed_address_is_assigned_to_the_device() {
        assert_eq!(
            client_claim_plan("molehill0", sip("203.0.113.7:8443")),
            ["ip addr replace 203.0.113.7/32 dev molehill0"]
        );
        assert_eq!(
            client_claim_plan("molehill0", sip("[2001:db8::7]:8443")),
            ["ip -6 addr replace 2001:db8::7/128 dev molehill0"]
        );
    }
}
