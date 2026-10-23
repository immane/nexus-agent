# Workload image prototype

`Dockerfile` builds the two fixed container executables: `container-worker` for
the bounded file protocol and `nexus-keeper` as the idle container entrypoint.
Both builder and runtime base images must be supplied by digest-pinned
`RUST_BUILDER_IMAGE` and `WORKLOAD_BASE_IMAGE` build arguments. The runtime base
must provide `/bin/sh` and `/etc/ssl/certs/ca-certificates.crt`; the image is
otherwise expected to remain minimal. The root `.dockerignore` excludes build
outputs and common local secret files from the build context.

The keeper healthcheck verifies that the expected `nexus-keeper run` process is
still a direct child of the container init process (the workload uses Docker's
init shim to forward stop signals and reap the keeper). The workload lifecycle also
checks the exact container ID, ownership label, running state, and Docker health
status before constructing its file/exec adapters. This is a process-liveness
gate only; it does not validate network isolation, worker correctness, or
security of an image that has not been reviewed and pinned.

The session composes workload and gateway ownership internally and mounts a
private host-generated resolver file containing only `nameserver 127.0.0.1`.
The resolver file is retained if cleanup becomes uncertain so a possibly-live
workload is not left with its DNS backing removed. Docker Desktop sharing of the
host temporary directory and this file mount are not validated.

The internal owner caps active plus quarantined environments at two. A durable,
owner-only metadata record is written under `XDG_STATE_HOME/nexus-agent/container`
(fallback: `~/.local/state/nexus-agent/container`) before workload creation. It
contains only generated names/IDs and trusted Docker/resolver paths. A process
lock prevents another Nexus process from taking ownership; a stale lock or
leftover record fails closed and is never automatically removed. Operator-facing
inspection and targeted restart reconciliation are not implemented, so this is
still not ready for frontend/session integration.

No base-image digests or release workload image are selected here. Do not build
or publish this prototype as a supported image until its inputs, architecture,
licenses, toolchain, and runtime contents are reviewed. The lifecycle remains
internal and is not wired into TUI/Server or an execution-mode option. Native
Linux network/isolation validation remains a separate required gate.
