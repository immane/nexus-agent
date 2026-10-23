# Public-egress gateway prototype

This directory contains only a gateway prototype; it does not wire container
execution into Nexus. The gateway runs in its own network namespace with
`NET_ADMIN` confined to that container. A workload shares that namespace with
`--cap-drop=ALL` and `no-new-privileges`; never grant `NET_RAW`, `NET_ADMIN`, or a
Docker socket. It must not start until gateway health/readiness succeeds. Docker
rejects `--dns 127.0.0.1` with `--network container:<id>`, so the workload instead
receives a read-only `/etc/resolv.conf` bind mount containing
`nameserver 127.0.0.1`.

The gateway uses Docker's default `bridge`, not a per-session user-defined
bridge. In the observed Docker Engine configuration, cached-image containers on
default bridge receive the host resolver (`192.168.65.7`), while explicit
queries to `127.0.0.11` are refused. Containers on a user-defined bridge instead
get Docker embedded DNS at `127.0.0.11`, which the shared-namespace workload
could reach despite firewall rules. Default-bridge selection avoids that
observed bypass; it is engine-dependent and not a portable resolver isolation
guarantee. No ports are published. The Docker check assigns the dynamically
inspected bridge listener IP as an explicit blocked host/LAN CIDR and requires
that the control can reach it while workload access fails and the exact
`blocked_host_v4` counter increments.

The nftables input, output, and forward hooks default to drop; forwarding is
explicitly denied. Output allows established replies and TCP/UDP egress, while
blocking caller-supplied host/LAN IPv4 CIDRs before common non-global ranges.
Startup fails unless `GATEWAY_BLOCKED_IPV4_CIDRS` contains IPv4 CIDRs covering
every host and LAN network to protect. Generic RFC1918 ranges do not cover
publicly numbered LANs. All RFC1918 destinations are denied by the generic
non-global set. DNS listens only on `127.0.0.1`, and shared workloads use that
local dnsmasq. Docker-created NAT rules remain intact. IPv6 input and output are
explicitly dropped. DNS upstreams are Cloudflare public resolvers; DNS remains a
data-exfiltration channel. Allowed public endpoints can themselves proxy
traffic. This is not proof against every special-purpose registry allocation
or indirect relay.

## Build and checks

The base must already be cached exactly as the pinned digest in
`check-docker.sh`. The build uses `--pull=false`; it downloads only the pinned
nftables and dnsmasq packages from the configured public Alpine repository. No
project code is uploaded.

```sh
python3 -m unittest -v containers/test_policy.py
containers/check-docker.sh
```

Runtime checks require a Linux Docker Engine with usable nftables kernel
support. They test startup/readiness, shared-namespace workload hardening,
read-only resolver mount, local DNS answer, embedded resolver unreachability,
default-bridge listener connectivity and policy denial, and the drop-policy
forward hook. The default check makes no public HTTPS request. Set
`CHECK_PUBLIC_EGRESS=1` to add a request to `https://example.com/`; this is the
only public endpoint probe. No image pull is enabled. The listener test verifies
only the configured deny rule, not general host/LAN isolation or DNS rebinding
resistance.
