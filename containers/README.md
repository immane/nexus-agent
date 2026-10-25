# Container execution prototypes and gateway checks

This directory contains gateway and workload image prototypes plus opt-in
gateway checks. Container execution is selectable only in container-enabled
TUI/server builds with explicit startup options; it remains an unvalidated
prototype and is not an approved isolation boundary. The gateway runs in its own network namespace with
`NET_ADMIN` confined to that container. A workload shares that namespace with
`--cap-drop=ALL` and `no-new-privileges`; never grant `NET_RAW`, `NET_ADMIN`, or a
Docker socket. It must not start until gateway health/readiness succeeds. Docker
rejects `--dns 127.0.0.1` with `--network container:<id>`, so the workload instead
receives a read-only `/etc/resolv.conf` bind mount containing
`nameserver 127.0.0.1`.

The workload image recipe and pinned **linux/arm64** builder/runtime inputs are
documented in [`workload/README.md`](workload/README.md). A keeper-only smoke
test passed on Docker Desktop with `--network none`, a read-only rootfs, no
capabilities, and no workspace mount; it verified only keeper process
liveness. The worker was not dispatched. This is not workload or network
isolation evidence. The workload Dockerfile currently rejects non-arm64 targets.

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

Set `CHECK_LINUX_HOST_ISOLATION=1` to additionally exercise a real Linux Docker
host gateway. Before building or changing containers, this mode requires host
Linux, Docker context `default`, a nonempty local Unix-socket endpoint, and a
daemon not identified as Docker Desktop/LinuxKit; it also requires `ip`. A local
Unix socket can still front a proxy, so these checks cannot prove daemon
locality. It gathers all non-loopback IPv4 interface subnets and point-to-point
peers, route destinations and gateways (including multipath), and collapses
overlapping/adjacent CIDRs before blocking them along with the default bridge
gateway. Unsupported `nhid` routes and malformed/incomplete IP JSON fail closed.
A temporary Python listener binds only to that bridge gateway on an ephemeral
port: a regular bridge control must connect, while the shared-namespace workload
must be denied and increment the exact `blocked_host_v4` counter. The listener
has a bounded accept timeout and is stopped/joined during cleanup. This does
not modify host firewall state, publish ports, pull images, or make public
requests; public egress remains independently opt-in via
`CHECK_PUBLIC_EGRESS=1`. Native Linux host-isolation validation remains deferred
until this opt-in check is run on a supported native Linux Docker host.

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

On macOS, broad routes (`/8` or broader) attached to `utun` interfaces are
treated as VPN/exit-node routing rather than host/LAN subnets and are omitted
from the blocked-host set. Specific routes through a tunnel and all connected
interface networks remain blocked. This is necessary when an exit node installs
split-default routes; otherwise those routes can cover every public IPv4 address
and silently turn a network-capable task container into a networkless one. It
does not prove isolation against every tunnel implementation or publicly
numbered network behind a tunnel; validate the effective CIDRs and egress on the
actual host before relying on this policy.

Docker Desktop may synthesize public DNS answers in `198.18.0.0/15` and route
TCP through its host-side network proxy. The host detects Docker Desktop from
`docker info` and enables a narrow exception for that range on TCP ports 80 and
443 only. The configured host/LAN deny set is checked first; other RFC 2544
destinations and all non-TCP traffic to the synthetic range remain denied. The
exception is off for ordinary Docker Engine. This adapts the firewall to Docker
Desktop's synthetic routing and does not pass host proxy variables or credentials
into the gateway/workload.

The workload bind-mounts the project source but overlays `/workspace/node_modules`
and `/workspace/.next` with private tmpfs mounts. Dependency installs therefore
use Linux binaries and Linux filesystem rename semantics without modifying or
reusing host `node_modules`; generated dependencies/build output are ephemeral
when the workload is removed. Source and lockfile changes remain on the workspace
bind mount.

## Build and checks

The base must already be cached exactly as the pinned digest in
`check-docker.sh`. The build uses `--pull=false`; it downloads only the pinned
nftables and dnsmasq packages from the configured public Alpine repository. No
project code is uploaded.

```sh
python3 -m unittest -v containers/test_policy.py
containers/check-docker.sh
```

The separate workload image is a development prototype and is built from the
repository root (default inputs are digest-pinned and arm64-only):

```sh
docker buildx build --platform linux/arm64 --load \
  -f containers/workload/Dockerfile \
  -t nexus-workload-prototype:dev .
```

Building or smoke-testing this image does not approve it for secure agent
sessions. Do not treat workspace mounts, worker dispatch, or successful runtime
startup as evidence of isolation; native Linux validation remains outstanding.

On a native Linux host only, an additional opt-in probe discovers interface and
route CIDRs, binds a temporary listener exclusively to the Docker bridge
gateway address, checks that an ordinary bridge container can reach it, and
checks that the shared-namespace workload is blocked by the exact host-CIDR
rule:

```sh
CHECK_LINUX_HOST_ISOLATION=1 containers/check-docker.sh
```

The opt-in preflight rejects Docker Desktop/LinuxKit, non-default contexts,
remote endpoints, and explicit Docker endpoint overrides before building or
creating test containers. A local Unix socket can still proxy another daemon,
so the operator must trust the selected socket. This probe exercises the
Docker bridge host address, not every upstream router or LAN destination.

Runtime checks require a Linux Docker Engine with usable nftables kernel
support. They test startup/readiness, shared-namespace workload hardening,
read-only resolver mount, local DNS answer, embedded resolver unreachability,
default-bridge listener connectivity and policy denial, and the drop-policy
forward hook. The default check makes no public HTTPS request. Set
`CHECK_PUBLIC_EGRESS=1` to add a request to `https://example.com/`; this is the
only public HTTPS probe (the image build may access the pinned public Alpine
package repository if its layer is uncached). No image pull is enabled. The listener test verifies
only the configured deny rule, not general host/LAN isolation or DNS rebinding
resistance.
Set `CHECK_NPM_REGISTRY=1` to additionally install the small public
`is-number@7.0.0` package into a temporary, read-only-rootfs `node:22-alpine`
container sharing the gateway namespace. npm lifecycle scripts are disabled;
the test uses no project or host credentials and requires the Node image to be
cached locally.

## Validation status

**Native Linux validation is outstanding.** Container execution can be selected
explicitly before the validation checklist passes, but do not treat the gateway
as an approved isolation boundary or claim Linux host/LAN isolation. Use only a
disposable workspace with no secrets or valuable data.
When a supported native Linux Docker Engine is available, run the accumulated
checklist as one validation pass and record engine/kernel versions, host/LAN
CIDRs, commands, and results here.

The current development host is macOS with Docker Desktop (`desktop-linux`,
LinuxKit 6.12.76, arm64), not a native Linux Docker host. Docker Desktop's
resolver was observed returning `198.18.7.38` for `registry.npmjs.org`; a plain
bridge container could reach the registry through that synthetic route, while
the original gateway policy rejected the reserved range. The opt-in probes now
exercise this narrow exception. Passing them on Docker Desktop still does not
validate the Linux host's own LAN interfaces or routes; native Linux validation
remains outstanding.

### Deferred native-Linux validation checklist

Run these checks together on the supported native Linux Docker Engine; do not
count Docker Desktop or a missing daemon as a pass:

- [ ] Record Docker Engine, kernel, architecture, selected context/socket, and
  actual host/LAN interface and route CIDRs.
- [ ] Run the default gateway checks for startup/readiness, workload capability
  drops, resolver mount, DNS behavior, bridge listener denial, and forwarding
  policy.
- [ ] With controlled public fixtures, verify successful public HTTPS egress
  and record the exact DNS answers and egress route.
- [ ] Verify denial of every host/LAN CIDR (including publicly numbered LANs),
  host gateway/interface addresses, sibling networks, and controlled metadata
  destinations; verify the expected deny counters.
- [ ] Verify IPv6 is denied and test IPv4-mapped IPv6 and relevant transition
  paths.
- [ ] Exercise DNS behavior, including attempts to use Docker embedded DNS,
  controlled rebinding answers, and access to blocked destinations by resolved
  address; record what the gateway does and does not prevent.
- [ ] Repeat the opt-in native host-gateway probe and public-egress probe; retain
  command output and distinguish direct egress failures from environment proxy
  requirements.
