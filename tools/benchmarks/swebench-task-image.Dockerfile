# Build from the repository root:
# docker buildx build --platform linux/amd64 \
#   --build-arg TASK_IMAGE='swebench/...@sha256:...' \
#   --build-arg TASK_BASE_COMMIT='<canonical base_commit>' \
#   -f tools/benchmarks/swebench-task-image.Dockerfile .
#
# Adds the statically linked Nexus worker/keeper to an already-pulled official
# SWE-bench task image. This is an adapter image, not a validated security boundary.
ARG RUST_BUILDER_IMAGE=rust:1.99.0-alpine3.23@sha256:6daa5ad789a3023ceb2f1b3d1307d5e8c38326c8c95aa8d58ec4d112f69653c8
ARG TASK_IMAGE=nexus.invalid/required-swebench-task-image:missing

FROM ${RUST_BUILDER_IMAGE} AS build
ENV RUSTUP_TOOLCHAIN=1.99.0-x86_64-unknown-linux-musl
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --locked \
    -p nexus-container --features container-worker,container-keeper \
    --bin container-worker --bin container-keeper

FROM ${TASK_IMAGE}
ARG TASK_BASE_COMMIT
COPY tools/benchmarks/swebench_image_support.py /usr/local/lib/nexus-swe-support.py
RUN test -n "${TASK_BASE_COMMIT}" \
    && git -C /testbed reset --hard "${TASK_BASE_COMMIT}" \
    && /opt/miniconda3/envs/testbed/bin/python /usr/local/lib/nexus-swe-support.py capture /testbed /opt/nexus-task-support "${TASK_BASE_COMMIT}" \
    && /opt/miniconda3/envs/testbed/bin/python -c "import pathlib,site; pathlib.Path(site.getsitepackages()[0], '00_nexus_workspace.pth').write_text(\"import sys; sys.path[:0] = ['/workspace/src', '/workspace/lib', '/workspace']\\n\")" \
    && rm -rf /testbed
WORKDIR /workspace
COPY --from=build /src/target/release/container-worker /usr/local/bin/container-worker
COPY --from=build /src/target/release/container-keeper /usr/local/bin/nexus-keeper
ENV PATH="/opt/miniconda3/envs/testbed/bin:${PATH}" \
    CONDA_PREFIX=/opt/miniconda3/envs/testbed \
    CONDA_DEFAULT_ENV=testbed
HEALTHCHECK --interval=2s --timeout=1s --retries=3 --start-period=1s \
    CMD ["/usr/local/bin/nexus-keeper", "healthcheck"]
