ARG RUST_BUILDER_IMAGE
FROM ${RUST_BUILDER_IMAGE} AS build

WORKDIR /src
COPY . .
RUN cargo build --release --locked \
    -p nexus-tools --features container-worker,container-keeper \
    --bin container-worker --bin container-keeper

ARG WORKLOAD_BASE_IMAGE
FROM ${WORKLOAD_BASE_IMAGE}

ARG RUST_BUILDER_IMAGE
ARG WORKLOAD_BASE_IMAGE
RUN case "$RUST_BUILDER_IMAGE" in *@sha256:????????????????????????????????????????????????????????????????) ;; *) exit 1 ;; esac \
    && case "$WORKLOAD_BASE_IMAGE" in *@sha256:????????????????????????????????????????????????????????????????) ;; *) exit 1 ;; esac \
    && test -x /bin/sh \
    && test -f /etc/ssl/certs/ca-certificates.crt \
    && addgroup -S -g 1000 worker \
    && adduser -S -D -H -u 1000 -G worker -s /bin/sh worker \
    && mkdir -p /workspace /home/worker /tmp \
    && chown 1000:1000 /home/worker /tmp

COPY --from=build /src/target/release/container-worker /usr/local/bin/container-worker
COPY --from=build /src/target/release/container-keeper /usr/local/bin/nexus-keeper

ENV PATH=/usr/local/bin:/usr/bin:/bin \
    HOME=/home/worker \
    TMPDIR=/tmp \
    LC_ALL=C.UTF-8

USER 1000:1000
WORKDIR /workspace
HEALTHCHECK --interval=2s --timeout=1s --retries=3 --start-period=1s CMD ["/usr/local/bin/nexus-keeper", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/nexus-keeper", "run"]
