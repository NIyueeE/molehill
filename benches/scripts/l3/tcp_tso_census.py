#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Does a TCP bulk flow reach a TUN reader as GSO super-frames? The census.

The question behind the transparent path's bulk throughput: L4's "packer" (the
kernel TCP stack assembling a byte stream) is free — is the same packing free
to an L3 path, or does the kernel segment every packet at the TUN boundary?

Measured 2026-10-16 on kernel 6.12 (the numbers in benchmarks.md): without the
offload declaration a bulk TCP flow arrived as 576 071 reads of 1 500 bytes;
with `IFF_VNET_HDR` + `TUNSETOFFLOAD`, as 59 160 reads averaging 65 212 bytes
(`gso_type = TCPV4`). Bare `IFF_VNET_HDR` without the declaration still
segments — the declaration is the lever. UDP GSO segments before the reader on
this kernel even with it, so the win is TCP-shaped.

Run as root; builds its own namespaces and tears them down:

    sudo -n python3 benches/scripts/l3/tcp_tso_census.py

Topology (root namespace = pure router, ip_forward=1):

    client ns (census_a, 10.9.0.1) --veth c_va(10.9.0.2)-- root
        --veth c_vb(10.8.0.2)-- server ns (census_b, 10.8.0.1)

Forward path: policy rule `iif c_va` -> table 200 -> dev `census_tun` (the TUN
under test). The shuttle reads `census_tun` and relays every frame whole (the
10-byte virtio-net header included: a `NEEDS_CSUM` frame only checksums with
its header present) into `census_tun2`, whose connected route hands the packet
to the stack, which forwards it out `c_vb`. Return path never touches a TUN.

Verdict: super-packets delivered when any read exceeds 1 600 B or a non-NONE
`gso_type` is seen; otherwise the kernel segmented before the reader.
"""

import collections
import contextlib
import fcntl
import os
import struct
import subprocess
import threading
import time

TUNSETIFF = 0x400454CA
TUNSETOFFLOAD = 0x400454D0  # _IOW('T', 208, unsigned int)
IFF_TUN = 0x0001
IFF_NO_PI = 0x1000
IFF_VNET_HDR = 0x4000
VNET_LEN = 10

TUN_F_CSUM = 0x01
TUN_F_TSO4 = 0x02
TUN_F_TSO6 = 0x04
TUN_F_TSO_ECN = 0x08
TUN_F_USO4 = 0x20
TUN_F_USO6 = 0x40

SERVER_PORT = 5203
RECV_BATCH = 262144
#: A read above this many bytes carries more than one MTU-sized packet.
SUPER_PACKET_FLOOR = 1600


def sh(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    r = subprocess.run(args, check=False, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"{args} -> rc={r.returncode} {r.stderr.strip()[:120]}")
    return r


def attach(name: str, vnet: bool) -> int:
    """Attach to `name`, creating it for this process's lifetime."""
    fd = os.open("/dev/net/tun", os.O_RDWR | os.O_NONBLOCK)
    flags = IFF_TUN | IFF_NO_PI | (IFF_VNET_HDR if vnet else 0)
    ifr = struct.pack("16sH22s", name.encode(), flags, b"")
    fcntl.ioctl(fd, TUNSETIFF, ifr)
    if vnet:
        # The declaration is the whole experiment: without it the tun driver's
        # egress feature set has no TSO and the stack segments before the
        # reader sees anything.
        want = (
            TUN_F_CSUM
            | TUN_F_TSO4
            | TUN_F_TSO6
            | TUN_F_TSO_ECN
            | TUN_F_USO4
            | TUN_F_USO6
        )
        fcntl.ioctl(fd, TUNSETOFFLOAD, want)
    sh("ip", "link", "set", name, "up")
    return fd


def setup() -> None:
    for ns in ("census_a", "census_b"):
        sh("ip", "netns", "del", ns, check=False)
    for link in ("c_va", "c_vb", "census_tun", "census_tun2"):
        sh("ip", "link", "del", link, check=False)
    sh("ip", "netns", "add", "census_a")
    sh("ip", "netns", "add", "census_b")
    sh("ip", "link", "add", "c_va", "type", "veth", "peer", "name", "c_var")
    sh("ip", "link", "set", "c_var", "netns", "census_a")
    sh("ip", "link", "add", "c_vb", "type", "veth", "peer", "name", "c_vbr")
    sh("ip", "link", "set", "c_vbr", "netns", "census_b")
    sh("ip", "addr", "add", "10.9.0.2/24", "dev", "c_va")
    sh("ip", "addr", "add", "10.8.0.2/24", "dev", "c_vb")
    sh("ip", "link", "set", "c_va", "up")
    sh("ip", "link", "set", "c_vb", "up")
    sh(
        "ip",
        "netns",
        "exec",
        "census_a",
        "ip",
        "addr",
        "add",
        "10.9.0.1/24",
        "dev",
        "c_var",
    )
    sh("ip", "netns", "exec", "census_a", "ip", "link", "set", "c_var", "up")
    sh("ip", "netns", "exec", "census_a", "ip", "link", "set", "lo", "up")
    sh(
        "ip",
        "netns",
        "exec",
        "census_a",
        "ip",
        "route",
        "add",
        "default",
        "via",
        "10.9.0.2",
    )
    sh(
        "ip",
        "netns",
        "exec",
        "census_b",
        "ip",
        "addr",
        "add",
        "10.8.0.1/24",
        "dev",
        "c_vbr",
    )
    sh("ip", "netns", "exec", "census_b", "ip", "link", "set", "c_vbr", "up")
    sh("ip", "netns", "exec", "census_b", "ip", "link", "set", "lo", "up")
    sh(
        "ip",
        "netns",
        "exec",
        "census_b",
        "ip",
        "route",
        "add",
        "default",
        "via",
        "10.8.0.2",
    )
    sh("sysctl", "-wq", "net.ipv4.ip_forward=1")
    sh("sysctl", "-wq", "net.ipv4.conf.all.rp_filter=0")


def policy_rules() -> None:
    sh("ip", "rule", "add", "pref", "50", "iif", "c_va", "lookup", "200", check=False)
    sh(
        "ip",
        "route",
        "add",
        "10.8.0.0/24",
        "dev",
        "census_tun",
        "table",
        "200",
        check=False,
    )
    sh("ip", "route", "add", "10.8.0.0/24", "dev", "c_vb", check=False)


def policy_cleanup() -> None:
    sh("ip", "rule", "del", "pref", "50", check=False)
    sh(
        "ip",
        "route",
        "del",
        "10.8.0.0/24",
        "dev",
        "census_tun",
        "table",
        "200",
        check=False,
    )
    sh("ip", "route", "del", "10.8.0.0/24", "dev", "c_vb", check=False)


class Shuttle(threading.Thread):
    """census_tun -> census_tun2, frames relayed whole."""

    def __init__(self, fd_in: int, fd_out: int):
        super().__init__(daemon=True)
        self.fd_in = fd_in
        self.fd_out = fd_out
        self.hist: collections.Counter = collections.Counter()
        self.types: collections.Counter = collections.Counter()
        self.reads = 0
        self.relayed = 0
        self.running = True

    def run(self) -> None:
        while self.running:
            try:
                raw = os.read(self.fd_in, RECV_BATCH)
            except BlockingIOError:
                time.sleep(0.0002)
                continue
            except OSError:
                break
            self.reads += 1
            _flags, gso_type, _hdr_len, _gso_size, _cs, _co = struct.unpack(
                "<BBHHHH", raw[:VNET_LEN]
            )
            self.types[gso_type] += 1
            self.hist[len(raw) - VNET_LEN] += 1
            try:
                os.write(self.fd_out, raw)
                self.relayed += 1
            except OSError:
                pass


SERVER_CODE = f"""
import socket
s = socket.socket(2, 1)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('10.8.0.1', {SERVER_PORT}))
s.listen(1)
s.settimeout(12)
try:
    c, _ = s.accept()
    got = 0
    c.settimeout(8)
    try:
        while True:
            b = c.recv({RECV_BATCH})
            if not b:
                break
            got += len(b)
    except socket.timeout:
        pass
    print('GOT', got)
    c.close()
except socket.timeout:
    print('GOT -1 (no connection)')
s.close()
"""

CLIENT_CODE = f"""
import socket, time
s = socket.socket(2, 1)
s.settimeout(6)
s.connect(('10.8.0.1', {SERVER_PORT}))
s.settimeout(3)
buf = bytes({RECV_BATCH})
end = time.monotonic() + 3.0
try:
    while time.monotonic() < end:
        s.sendall(buf)
except Exception as e:
    print('SEND_STOP', type(e).__name__, e)
s.close()
"""


def main() -> int:
    setup()
    fd_tun = attach("census_tun", vnet=True)
    fd_in = attach("census_tun2", vnet=True)
    policy_rules()
    shuttle = Shuttle(fd_tun, fd_in)
    shuttle.start()
    server = subprocess.Popen(
        ["ip", "netns", "exec", "census_b", "python3", "-c", SERVER_CODE],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    time.sleep(0.4)
    try:
        client = subprocess.run(
            ["ip", "netns", "exec", "census_a", "python3", "-c", CLIENT_CODE],
            check=False,
            capture_output=True,
            text=True,
            timeout=40,
        )
        time.sleep(1.0)
        out, _ = server.communicate(timeout=10)
        print("client:", client.stdout.strip()[:120])
        print("server:", out.strip())
        tot = sum(shuttle.hist.values())
        big = sum(v for k, v in shuttle.hist.items() if k > SUPER_PACKET_FLOOR)
        print(f"reads={shuttle.reads} relayed={shuttle.relayed}")
        print(f"gso_types={dict(shuttle.types)}")
        print(f"hist_top={shuttle.hist.most_common(5)}")
        print(f"reads_over_1600B={big}/{tot}")
        if out.startswith("GOT -1"):
            print("INCONCLUSIVE: no bytes reached the server")
            return 2
        if big:
            print("VERDICT: SUPER-PACKETS DELIVERED — the TUN reader sees TSO units")
            return 0
        print("VERDICT: NO super-packets — the kernel segmented before the reader")
        return 1
    finally:
        shuttle.running = False
        time.sleep(0.2)
        server.kill()
        for fd in (fd_tun, fd_in):
            with contextlib.suppress(OSError):
                os.close(fd)
        policy_cleanup()
        for link in ("census_tun", "census_tun2", "c_va", "c_vb"):
            sh("ip", "link", "del", link, check=False)
        sh("ip", "netns", "del", "census_a", check=False)
        sh("ip", "netns", "del", "census_b", check=False)


if __name__ == "__main__":
    raise SystemExit(main())
