#!/bin/sh
set -eu
cd "$(dirname "$0")"
BASE='nginx:alpine@sha256:54f2a904c251d5a34adf545a72d32515a15e08418dae0266e23be2e18c66fefa'
IMAGE='nexus-public-egress-gateway:test'
docker image inspect "$BASE" >/dev/null || { echo "Required cached base image missing: $BASE" >&2; exit 1; }
docker build --pull=false -t "$IMAGE" gateway
docker run --rm --entrypoint /bin/sh "$IMAGE" -c 'nft --version'
docker run --rm --cap-add NET_ADMIN --entrypoint /bin/sh "$IMAGE" -c 'nft -c -f /etc/nftables.conf'
python3 -c 'import json, os, re, subprocess, sys, tempfile, time, uuid
image = sys.argv[1]
suffix = uuid.uuid4().hex[:12]
gateway = "nexus-egress-gateway-" + suffix
workload = "nexus-egress-workload-" + suffix
listener = "nexus-egress-listener-" + suffix
control = "nexus-egress-control-" + suffix
resolver = tempfile.NamedTemporaryFile(mode="w", delete=False)
resolver_name = resolver.name
resolver_contents = "nameserver 127.0.0.1\noptions ndots:0\n"
resolver.write(resolver_contents)
resolver.close()
def run(args, **kwargs): subprocess.run(args, timeout=20, check=True, **kwargs)
try:
    run(["docker", "run", "-d", "--name", listener, "--network", "bridge", "--entrypoint", "/bin/sh", image, "-c", "while :; do nc -l -p 8080; done"], stdout=subprocess.DEVNULL)
    listener_ip = subprocess.check_output(["docker", "inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", listener], text=True, timeout=5).strip()
    run(["docker", "run", "-d", "--name", gateway, "--cap-add", "NET_ADMIN", "--network", "bridge", "-e", "GATEWAY_BLOCKED_IPV4_CIDRS=" + listener_ip + "/32", image], stdout=subprocess.DEVNULL)
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
    positive = subprocess.run(["docker", "exec", control, "nc", "-z", "-w", "3", listener_ip, "8080"], capture_output=True, text=True, timeout=5)
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
    denied = subprocess.run(["docker", "exec", workload, "nc", "-z", "-w", "3", listener_ip, "8080"], capture_output=True, text=True, timeout=5)
    if denied.returncode == 0: raise RuntimeError("workload reached listener despite blocked_host_v4")
    after = blocked_counter()
    if after <= before: raise RuntimeError("blocked_host_v4 counter did not increase: before=%d after=%d" % (before, after))
    if os.environ.get("CHECK_PUBLIC_EGRESS") == "1": run(["docker", "exec", workload, "wget", "-q", "-T", "10", "-O", "/dev/null", "https://example.com/"])
finally:
    cleanup_failures = []
    for container in (workload, control, listener, gateway):
        try:
            result = subprocess.run(["docker", "rm", "-f", container], capture_output=True, text=True, timeout=20)
            if result.returncode and "No such container" not in result.stderr:
                cleanup_failures.append("container %s: %s" % (container, result.stderr.strip() or result.stdout.strip()))
        except Exception as error: cleanup_failures.append("container %s: %s" % (container, error))
    try: os.unlink(resolver_name)
    except FileNotFoundError: pass
    except Exception as error: cleanup_failures.append("resolver file %s: %s" % (resolver_name, error))
    if cleanup_failures:
        for failure in cleanup_failures: print("cleanup failed: " + failure, file=sys.stderr)
        raise RuntimeError("one or more cleanup operations failed")' "$IMAGE"
python3 -m unittest -v test_policy.py
