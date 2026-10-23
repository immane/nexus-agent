from ipaddress import IPv4Address, IPv4Network
from pathlib import Path
import re
import unittest


POLICY = (Path(__file__).parent / "gateway/nftables.conf").read_text()


class GatewayPolicyTests(unittest.TestCase):
    def test_default_deny_for_input_and_output(self):
        for chain in ("input", "output"):
            match = re.search(rf"chain {chain} \{{(.*?)\n    \}}", POLICY, re.S)
            self.assertIsNotNone(match, chain)
            self.assertIn("policy drop;", match.group(1))

    def test_non_global_and_docker_private_destinations_are_denied(self):
        match = re.search(r"elements = \{(.*?)\n        \}", POLICY, re.S)
        networks = [IPv4Network(item.strip()) for item in match.group(1).split(",")]
        blocked = [
            "0.1.2.3", "10.1.2.3", "100.64.0.1", "127.0.0.1",
            "169.254.169.254", "172.17.0.1", "192.168.1.1",
            "198.18.0.1", "224.0.0.1", "240.0.0.1",
        ]
        for address in blocked:
            with self.subTest(address=address):
                self.assertTrue(any(IPv4Address(address) in network for network in networks))
        self.assertIn("ip daddr 255.255.255.255 drop", POLICY)

    def test_ipv6_is_explicitly_dropped_and_dns_is_loopback_only(self):
        self.assertEqual(POLICY.count("meta nfproto ipv6 drop"), 2)
        input_chain = re.search(r"chain input \{(.*?)\n    \}", POLICY, re.S).group(1)
        self.assertIn('iifname "lo" accept', input_chain)
        self.assertIn('iifname "lo" ip daddr 127.0.0.1 udp dport 53 accept', input_chain)
        self.assertIn('iifname "lo" ip daddr 127.0.0.1 tcp dport 53 accept', input_chain)
        self.assertLess(input_chain.index("ip daddr 127.0.0.11 udp dport 53 counter drop"), input_chain.index('iifname "lo" accept'))
        dns = (Path(__file__).parent / "gateway/dnsmasq.conf").read_text()
        self.assertIn("listen-address=127.0.0.1", dns)
        self.assertNotIn("0.0.0.0", dns)

    def test_public_transport_and_controlled_dns_are_explicit(self):
        self.assertIn("ip protocol tcp accept", POLICY)
        self.assertIn("ip protocol udp accept", POLICY)
        dns = (Path(__file__).parent / "gateway/dnsmasq.conf").read_text()
        self.assertIn("server=1.1.1.1", dns)
        self.assertIn("server=1.0.0.1", dns)
        self.assertIn("local=/gateway.test/", dns)
        self.assertIn("udp dport 53 drop", POLICY)
        self.assertIn("tcp dport 53 drop", POLICY)
        forward = re.search(r"chain forward \{(.*?)\n    \}", POLICY, re.S).group(1)
        self.assertIn("hook forward priority filter; policy drop;", forward)
        for protocol in ("udp", "tcp"):
            rule = f"ct original ip daddr 127.0.0.11 {protocol} dport 53 counter drop"
            self.assertIn(rule, POLICY)
        self.assertNotIn("flush ruleset", POLICY)
        self.assertIn("destroy table inet gateway", POLICY)
        self.assertIn("ip daddr @blocked_host_v4 counter drop", POLICY)
        self.assertIn("ip daddr @non_global_v4 counter drop", POLICY)
        output = re.search(r"chain output \{(.*?)\n    \}", POLICY, re.S).group(1)
        self.assertLess(output.index("ip daddr @blocked_host_v4 counter drop"), output.index("ip daddr @non_global_v4 counter drop"))

    def test_denies_precede_conntrack_accepts_without_blocking_loopback_dns(self):
        input_chain = re.search(r"chain input \{(.*?)\n    \}", POLICY, re.S).group(1)
        output_chain = re.search(r"chain output \{(.*?)\n    \}", POLICY, re.S).group(1)
        input_established = input_chain.index("ct state established,related accept")
        for rule in ("ip saddr @blocked_host_v4 counter drop", "ip saddr @non_global_v4 drop"):
            self.assertLess(input_chain.index(rule), input_established)
        output_established = output_chain.index("ct state established,related accept")
        for rule in (
            "ip daddr @blocked_host_v4 counter drop",
            "ip daddr @non_global_v4 counter drop",
            "ip daddr 255.255.255.255 drop",
        ):
            self.assertLess(output_chain.index(rule), output_established)
        self.assertLess(input_chain.index('iifname "lo" accept'), input_established)
        self.assertLess(output_chain.index('oifname "lo" accept'), output_established)
        self.assertLess(output_chain.index("ct original ip daddr 127.0.0.11 udp dport 53 counter drop"), output_chain.index('oifname "lo" accept'))

    def test_gateway_requires_explicit_host_lan_cidrs(self):
        entrypoint = (Path(__file__).parent / "gateway/gateway-entrypoint").read_text()
        self.assertIn("GATEWAY_BLOCKED_IPV4_CIDRS must contain host/LAN IPv4 CIDRs", entrypoint)
        self.assertIn("nft add element inet gateway blocked_host_v4", entrypoint)

    def test_dns_readiness_checks_answer_not_server(self):
        entrypoint = (Path(__file__).parent / "gateway/gateway-entrypoint").read_text()
        self.assertIn('$1 == "Name:" && $2 == "gateway.test"', entrypoint)
        self.assertIn('$1 == "Address:" && $2 == "127.0.0.1"', entrypoint)
        self.assertNotIn("grep -q '127.0.0.1'", entrypoint)

    def test_docker_check_hardens_shared_namespace_workload(self):
        check = (Path(__file__).parent / "check-docker.sh").read_text()
        self.assertIn('state == "running healthy"', check)
        self.assertIn('"--cap-drop=ALL"', check)
        self.assertIn('"--security-opt=no-new-privileges"', check)
        self.assertIn('"--network", "bridge"', check)
        self.assertNotIn('"network", "create"', check)
        self.assertIn('dst=/etc/resolv.conf,readonly', check)
        self.assertIn('"127.0.0.11"', check)
        self.assertIn('"gateway.test", "127.0.0.11"', check)
        self.assertIn('"gateway.test", "127.0.0.1"', check)
        self.assertIn('"forward"', check)
        self.assertNotIn('"--cap-add", "NET_RAW"', check)

    def test_docker_check_targets_blocked_host_and_gates_public_https(self):
        check = (Path(__file__).parent / "check-docker.sh").read_text()
        self.assertIn('listener_ip + "/32"', check)
        self.assertIn("blocked_host_v4 counter packets", check)
        self.assertIn("after <= before", check)
        self.assertIn("positive-control container could not reach listener", check)
        self.assertIn('m["RW"] is False', check)
        self.assertIn("finally:", check)
        self.assertIn('if os.environ.get("CHECK_PUBLIC_EGRESS") == "1"', check)
        self.assertIn('"https://example.com/"', check)
        self.assertIn('"No such container" not in result.stderr', check)
        self.assertNotIn('"--network", "none"', check)
        self.assertNotIn('"-p",', check)


if __name__ == "__main__":
    unittest.main()
