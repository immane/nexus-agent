#!/bin/sh
set -eu
cd "$(dirname "$0")"
if [ "${CHECK_LINUX_HOST_ISOLATION:-0}" = 1 ]; then
    python3 -c 'import os, platform, shutil, subprocess, sys
if platform.system() != "Linux": sys.exit("CHECK_LINUX_HOST_ISOLATION=1 requires a Linux host; Docker Desktop is not a native Linux host")
if not shutil.which("ip"): sys.exit("CHECK_LINUX_HOST_ISOLATION=1 requires the ip utility")
if "DOCKER_HOST" in __import__("os").environ or "DOCKER_CONTEXT" in __import__("os").environ: sys.exit("CHECK_LINUX_HOST_ISOLATION=1 does not allow DOCKER_HOST or DOCKER_CONTEXT overrides")
try:
 context = subprocess.check_output(["docker", "context", "show"], text=True, timeout=5).strip()
 if context != "default": sys.exit("CHECK_LINUX_HOST_ISOLATION=1 requires Docker context default; found " + context)
 endpoint = subprocess.check_output(["docker", "context", "inspect", "default", "--format", "{{.Endpoints.docker.Host}}"], text=True, timeout=5).strip()
 if not endpoint.startswith("unix://") or not endpoint[7:].startswith("/"): sys.exit("CHECK_LINUX_HOST_ISOLATION=1 requires a nonempty local Unix socket endpoint; found " + endpoint)
 info = subprocess.check_output(["docker", "info", "--format", "{{.OSType}}|{{.OperatingSystem}}"], text=True, timeout=5).strip()
 os_type, operating_system = info.split("|", 1)
 if os_type != "linux" or "docker desktop" in operating_system.lower() or "linuxkit" in operating_system.lower(): sys.exit("CHECK_LINUX_HOST_ISOLATION=1 requires a native Linux Docker Engine; found " + info)
except Exception as e: sys.exit("cannot verify local Docker endpoint: " + str(e))'
fi
BASE='nginx:alpine@sha256:54f2a904c251d5a34adf545a72d32515a15e08418dae0266e23be2e18c66fefa'
IMAGE='nexus-public-egress-gateway:test'
docker image inspect "$BASE" >/dev/null || { echo "Required cached base image missing: $BASE" >&2; exit 1; }
docker build --pull=false -t "$IMAGE" gateway
docker run --rm --entrypoint /bin/sh "$IMAGE" -c 'nft --version'
docker run --rm --cap-add NET_ADMIN --entrypoint /bin/sh "$IMAGE" -c 'nft -c -f /etc/nftables.conf'
python3 -c 'import json, os, re, subprocess, sys, tempfile, time, uuid, ipaddress, platform, shutil, socket, unittest
image = sys.argv[1]
suffix = uuid.uuid4().hex[:12]
gateway = "nexus-egress-gateway-" + suffix
workload = "nexus-egress-workload-" + suffix
listener = "nexus-egress-listener-" + suffix
control = "nexus-egress-control-" + suffix
resolver_name = None
resolver_contents = "nameserver 127.0.0.1\noptions ndots:0\n"
def run(args, **kwargs): subprocess.run(args, timeout=20, check=True, **kwargs)
def parse_host_cidrs(addresses, routes):
    if not isinstance(addresses, list) or not isinstance(routes, list): raise RuntimeError("ip JSON output must be arrays")
    found = set()
    def add(address, prefix):
        try:
            network = ipaddress.IPv4Network("%s/%s" % (address, prefix), strict=False)
            if network.prefixlen == 0: raise ValueError("zero-length network is not a usable host/LAN block")
            found.add(str(network))
        except Exception as e: raise RuntimeError("malformed IPv4 address/prefix: %r/%r" % (address, prefix)) from e
    for interface in addresses:
        if not isinstance(interface, dict) or not isinstance(interface.get("ifname"), str) or not isinstance(interface.get("addr_info"), list): raise RuntimeError("malformed ip address entry")
        if interface.get("ifname") == "lo": continue
        for info in interface["addr_info"]:
            if not isinstance(info, dict): raise RuntimeError("malformed address record")
            if info.get("family") == "inet":
                add(info.get("local"), info.get("prefixlen"))
                if "peer" in info:
                    peer = info["peer"]
                    if not isinstance(peer, str): raise RuntimeError("malformed point-to-point peer")
                    add(peer.split("/", 1)[0], 32)
    for route in routes:
        if not isinstance(route, dict) or not isinstance(route.get("dst"), str): raise RuntimeError("malformed or incomplete route entry")
        if "nhid" in route: raise RuntimeError("unsupported nexthop ID (nhid)")
        if route["dst"] != "default":
            if "/" in route["dst"]: destination, prefix = route["dst"].split("/", 1)
            else: destination, prefix = route["dst"], 32
            add(destination, prefix)
        if "gateway" in route: add(route["gateway"], 32)
        hops = route.get("nexthops", route.get("multipath", []))
        if not isinstance(hops, list): raise RuntimeError("malformed multipath route")
        for hop in hops:
            if not isinstance(hop, dict) or "nhid" in hop: raise RuntimeError("unsupported or malformed multipath nexthop")
            if "gateway" in hop: add(hop["gateway"], 32)
            if "dev" not in hop and "gateway" not in hop: raise RuntimeError("incomplete multipath nexthop")
    collapsed = sorted(ipaddress.collapse_addresses(ipaddress.IPv4Network(item) for item in found), key=lambda n: (int(n.network_address), n.prefixlen))
    if not collapsed or len(collapsed) > 64: raise RuntimeError("host CIDR count must be between 1 and 64 after collapsing (got %d)" % len(collapsed))
    return [str(network) for network in collapsed]
def discover_host_cidrs():
    def read(args):
        try: value = json.loads(subprocess.check_output(args, text=True, timeout=5))
        except Exception as e: raise RuntimeError("invalid output from %s: %s" % (" ".join(args), e))
        return value
    return parse_host_cidrs(read(["ip", "-j", "-4", "addr", "show"]), read(["ip", "-j", "-4", "route", "show", "table", "all"]))
class ParserTests(unittest.TestCase):
    def test_fixture_covers_interfaces_routes_and_multipath(self):
        got = parse_host_cidrs([{"ifname":"lo","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8}]},{"ifname":"eth0","addr_info":[{"family":"inet","local":"192.0.2.10","prefixlen":24}]}],[{"dst":"default","gateway":"203.0.114.1"},{"dst":"203.0.113.0/24","nexthops":[{"id":1,"gateway":"198.51.100.1","dev":"eth0","weight":1},{"id":2,"gateway":"198.51.100.2","dev":"eth1","weight":1}]}])
        self.assertEqual(set(got), {"192.0.2.0/24","198.51.100.1/32","198.51.100.2/32","203.0.113.0/24","203.0.114.1/32"})
    def test_routes_subnets_peers_and_collapse(self):
        addresses = [{"ifname":"eth0","addr_info":[{"family":"inet","local":"192.0.2.10","prefixlen":24}]},{"ifname":"tun0","addr_info":[{"family":"inet","local":"10.0.0.1","prefixlen":32,"peer":"10.0.0.2"}]}]
        routes = [{"dst":"default","gateway":"192.0.2.1","dev":"eth0"},{"dst":"198.51.100.7/32","dev":"tun0"},{"dst":"192.0.2.11/32","dev":"eth0"}]
        self.assertEqual(parse_host_cidrs(addresses, routes), ["10.0.0.1/32", "10.0.0.2/32", "192.0.2.0/24", "198.51.100.7/32"])
    def test_malformed_and_nhid_rejected(self):
        with self.assertRaises(RuntimeError): parse_host_cidrs([], [{"dst":"default","gateway":"bad"}])
        with self.assertRaises(RuntimeError): parse_host_cidrs([], [{"gateway":"192.0.2.1"}])
        with self.assertRaises(RuntimeError): parse_host_cidrs([], [{"dst":"0.0.0.0/0"}])
        with self.assertRaises(RuntimeError): parse_host_cidrs([], [{"dst":"default","nexthops":[{"nhid":7}]}])
        with self.assertRaises(RuntimeError): parse_host_cidrs([], [{"nhid":3}])
    def test_empty_and_over_limit_rejected(self):
        with self.assertRaises(RuntimeError): parse_host_cidrs([], [])
        addresses = [{"ifname":"e","addr_info":[{"family":"inet","local":"10.%d.0.1" % i,"prefixlen":32} for i in range(1,66)]}]
        with self.assertRaises(RuntimeError): parse_host_cidrs(addresses, [])
    def test_opt_in_platform_guard(self):
        if os.environ.get("CHECK_LINUX_HOST_ISOLATION") == "1": self.assertEqual(platform.system(), "Linux")
test_result = unittest.TextTestRunner(verbosity=0).run(unittest.defaultTestLoader.loadTestsFromTestCase(ParserTests))
if not test_result.wasSuccessful(): raise RuntimeError("host CIDR parser tests failed")
opt_in = os.environ.get("CHECK_LINUX_HOST_ISOLATION") == "1"
host_listener = None
stop_listener = None
listener_thread = None
blocked_cidrs = None
if opt_in:
    if platform.system() != "Linux": raise RuntimeError("CHECK_LINUX_HOST_ISOLATION requires native Linux; Docker Desktop is not native Linux")
    if not shutil.which("ip"): raise RuntimeError("CHECK_LINUX_HOST_ISOLATION requires ip")
    blocked_cidrs = discover_host_cidrs()
    context = subprocess.check_output(["docker", "context", "show"], text=True, timeout=5).strip()
    endpoint = subprocess.check_output(["docker", "context", "inspect", "default", "--format", "{{.Endpoints.docker.Host}}"], text=True, timeout=5).strip()
    if context != "default" or not endpoint.startswith("unix://") or not endpoint[7:].startswith("/"): raise RuntimeError("CHECK_LINUX_HOST_ISOLATION requires default context and a local Unix socket")
try:
    resolver = tempfile.NamedTemporaryFile(mode="w", delete=False)
    resolver_name = resolver.name
    resolver.write(resolver_contents)
    resolver.close()
    if opt_in:
        bridge = json.loads(subprocess.check_output(["docker", "network", "inspect", "bridge"], text=True, timeout=5))[0]
        bridge_ip = bridge["IPAM"]["Config"][0]["Gateway"]
        host_listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        host_listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        host_listener.bind((bridge_ip, 0))
        host_listener.listen(8)
        listener_port = host_listener.getsockname()[1]
        listener_ip = bridge_ip
        from threading import Event, Thread
        stop_listener = Event()
        def serve():
            while not stop_listener.is_set():
                try: conn, _ = host_listener.accept(); conn.close()
                except socket.timeout: continue
                except OSError: return
        host_listener.settimeout(0.25)
        listener_thread = Thread(target=serve, daemon=True)
        listener_thread.start()
        blocked_cidrs = [str(network) for network in ipaddress.collapse_addresses([ipaddress.IPv4Network(cidr) for cidr in blocked_cidrs] + [ipaddress.IPv4Network(bridge_ip + "/32")])]
        if len(blocked_cidrs) > 64 or not any(ipaddress.IPv4Address(bridge_ip) in ipaddress.IPv4Network(cidr) for cidr in blocked_cidrs): raise RuntimeError("bridge gateway missing from final collapsed host CIDRs")
    else:
        run(["docker", "run", "-d", "--name", listener, "--network", "bridge", "--entrypoint", "/bin/sh", image, "-c", "while :; do nc -l -p 8080; done"], stdout=subprocess.DEVNULL)
        listener_ip = subprocess.check_output(["docker", "inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", listener], text=True, timeout=5).strip()
        listener_port = 8080
    run(["docker", "run", "-d", "--name", gateway, "--cap-add", "NET_ADMIN", "--network", "bridge", "-e", "GATEWAY_BLOCKED_IPV4_CIDRS=" + ",".join(blocked_cidrs or [listener_ip + "/32"]), image], stdout=subprocess.DEVNULL)
    for _ in range(20):
        state = subprocess.check_output(["docker", "inspect", "-f", "{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}", gateway], text=True, timeout=5).strip()
        if state == "running healthy": break
        if state.startswith("exited") or state.startswith("dead"): raise RuntimeError("gateway exited before becoming healthy: " + state)
        time.sleep(0.5)
    else: raise TimeoutError("gateway did not become healthy")
    run(["docker", "exec", gateway, "test", "-f", "/run/gateway-ready"])
    forward = subprocess.check_output(["docker", "exec", gateway, "nft", "list", "chain", "inet", "gateway", "forward"], text=True, timeout=5)
    if "hook forward" not in forward or "policy drop" not in forward: raise RuntimeError("forward chain is not explicitly drop-policy: " + forward)
    run(["docker", "run", "-d", "--name", workload, "--entrypoint", "/bin/sh", "--network", "container:" + gateway, "--mount", "type=bind,src=" + resolver_name + ",dst=/etc/resolv.conf,readonly", "--cap-drop=ALL", "--security-opt=no-new-privileges", image, "-c", "sleep 60"], stdout=subprocess.DEVNULL)
    config = subprocess.check_output(["docker", "inspect", "-f", "{{json .HostConfig.CapDrop}} {{json .HostConfig.SecurityOpt}}", workload], text=True, timeout=5).strip()
    caps, security = config.split(" ", 1)
    if json.loads(caps) != ["ALL"] or json.loads(security) != ["no-new-privileges"]: raise RuntimeError("workload hardening options missing: " + config)
    mounted_resolver = subprocess.check_output(["docker", "exec", workload, "cat", "/etc/resolv.conf"], text=True, timeout=5)
    if mounted_resolver != resolver_contents: raise RuntimeError("workload resolver contents differ: " + repr(mounted_resolver))
    mounts = json.loads(subprocess.check_output(["docker", "inspect", "-f", "{{json .Mounts}}", workload], text=True, timeout=5))
    if not any(m["Destination"] == "/etc/resolv.conf" and m["RW"] is False for m in mounts): raise RuntimeError("effective resolver mount is missing or writable: " + repr(mounts))
    result = subprocess.run(["docker", "exec", workload, "nslookup", "gateway.test"], capture_output=True, text=True, timeout=10)
    if result.returncode != 0 or not re.search(r"Name:\s*gateway\.test\s*\nAddress:\s*127\.0\.0\.1(?:\s|$)", result.stdout): raise RuntimeError("workload resolver did not return gateway.test A answer 127.0.0.1: " + result.stdout + result.stderr)
    run(["docker", "run", "-d", "--name", control, "--network", "bridge", "--entrypoint", "/bin/sh", image, "-c", "sleep 20"], stdout=subprocess.DEVNULL)
    positive = subprocess.run(["docker", "exec", control, "nc", "-z", "-w", "3", listener_ip, str(listener_port)], capture_output=True, text=True, timeout=5)
    if positive.returncode != 0: raise RuntimeError("positive-control container could not reach listener: " + positive.stdout + positive.stderr)
    for client in (gateway, workload):
        direct = subprocess.run(["docker", "exec", client, "nslookup", "-timeout=1", "gateway.test", "127.0.0.11"], capture_output=True, text=True, timeout=5)
        if direct.returncode == 0: raise RuntimeError(client + " unexpectedly resolved through 127.0.0.11: " + direct.stdout + direct.stderr)
    local_dns = subprocess.run(["docker", "exec", workload, "nslookup", "-timeout=2", "gateway.test", "127.0.0.1"], capture_output=True, text=True, timeout=5)
    if local_dns.returncode != 0 or not re.search(r"Address:\s*127\.0\.0\.1(?:\s|$)", local_dns.stdout): raise RuntimeError("workload local dnsmasq lookup failed: " + local_dns.stdout + local_dns.stderr)
    def blocked_counter():
        chain = subprocess.check_output(["docker", "exec", gateway, "nft", "list", "chain", "inet", "gateway", "output"], text=True, timeout=5)
        matches = re.findall(r"ip daddr @blocked_host_v4 counter packets (\d+)", chain)
        if len(matches) != 1: raise RuntimeError("could not identify exact blocked_host_v4 rule counter: " + chain)
        return int(matches[0])
    before = blocked_counter()
    denied = subprocess.run(["docker", "exec", workload, "nc", "-z", "-w", "3", listener_ip, str(listener_port)], capture_output=True, text=True, timeout=5)
    if denied.returncode == 0: raise RuntimeError("workload reached listener despite blocked_host_v4")
    after = blocked_counter()
    if after <= before: raise RuntimeError("blocked_host_v4 counter did not increase: before=%d after=%d" % (before, after))
    if os.environ.get("CHECK_PUBLIC_EGRESS") == "1":
        probe = subprocess.run(["docker", "exec", workload, "wget", "-S", "-T", "10", "-O", "/dev/null", "https://example.com/"], capture_output=True, text=True, timeout=15)
        if probe.returncode:
            print("public HTTPS probe failed: " + probe.stdout + probe.stderr, file=sys.stderr)
            for args in (("exec", gateway, "ip", "route"), ("exec", workload, "nslookup", "example.com"), ("exec", gateway, "nft", "list", "ruleset")):
                diagnostic = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=5)
                print("$ docker " + " ".join(args) + "\n" + diagnostic.stdout + diagnostic.stderr, file=sys.stderr)
            raise RuntimeError("public HTTPS egress probe failed")
finally:
    if host_listener is not None:
        if stop_listener is not None: stop_listener.set()
        host_listener.close()
        if listener_thread is not None: listener_thread.join(timeout=2)
    cleanup_failures = []
    for container in (workload, control, listener, gateway):
        try:
            result = subprocess.run(["docker", "rm", "-f", container], capture_output=True, text=True, timeout=20)
            if result.returncode and "No such container" not in result.stderr:
                cleanup_failures.append("container %s: %s" % (container, result.stderr.strip() or result.stdout.strip()))
        except Exception as error: cleanup_failures.append("container %s: %s" % (container, error))
    if resolver_name is not None:
        try: os.unlink(resolver_name)
        except FileNotFoundError: pass
        except Exception as error: cleanup_failures.append("resolver file %s: %s" % (resolver_name, error))
    if cleanup_failures:
        for failure in cleanup_failures: print("cleanup failed: " + failure, file=sys.stderr)
        raise RuntimeError("one or more cleanup operations failed")' "$IMAGE"
python3 -m unittest -v test_policy.py
