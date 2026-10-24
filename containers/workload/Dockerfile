ARG RUST_BUILDER_IMAGE=rust:1.99.0-alpine3.23@sha256:58d81a4631c273a704b725e6e3f2b0f207301c82bcb50523f082eaed6d21e768
ARG WORKLOAD_BASE_IMAGE=alpine:3.23@sha256:8793abc479381bafd16c9d538b01c5a8dd7eef6aa10a737f2ad828ac73cb80e9
FROM ${RUST_BUILDER_IMAGE} AS build

ENV RUSTUP_TOOLCHAIN=1.99.0-aarch64-unknown-linux-musl

WORKDIR /src
COPY . .
RUN cargo build --release --locked \
    -p nexus-tools --features container-worker,container-keeper \
    --bin container-worker --bin container-keeper

FROM ${WORKLOAD_BASE_IMAGE}

ARG RUST_BUILDER_IMAGE
ARG WORKLOAD_BASE_IMAGE
ARG TARGETARCH
RUN case "$RUST_BUILDER_IMAGE" in *@sha256:????????????????????????????????????????????????????????????????) ;; *) exit 1 ;; esac \
    && case "$WORKLOAD_BASE_IMAGE" in *@sha256:????????????????????????????????????????????????????????????????) ;; *) exit 1 ;; esac \
    && test "$TARGETARCH" = arm64 \
    && apk add --no-cache ca-certificates=20261020-r0 \
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
