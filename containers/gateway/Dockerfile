FROM nginx:alpine@sha256:54f2a904c251d5a34adf545a72d32515a15e08418dae0266e23be2e18c66fefa

# Versions pinned to the base image's public Alpine v3.23/aarch64 index
# observed 2026-10-23 (index SHA-256 recorded in containers/README.md).
RUN apk add --no-cache nftables=1.1.5-r2 dnsmasq=2.91-r1

COPY nftables.conf /etc/nftables.conf
COPY dnsmasq.conf /etc/dnsmasq.conf
COPY gateway-entrypoint /usr/local/sbin/gateway-entrypoint
RUN chmod 0555 /usr/local/sbin/gateway-entrypoint

EXPOSE 53/udp 53/tcp
HEALTHCHECK --interval=2s --timeout=2s --retries=2 CMD test -f /run/gateway-ready && nft list table inet gateway >/dev/null
ENTRYPOINT ["/usr/local/sbin/gateway-entrypoint"]
