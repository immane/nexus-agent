# Workload image prototype

`Dockerfile` builds the two fixed container executables: `container-worker` for
the bounded file protocol and `nexus-keeper` as the idle container entrypoint.
The Dockerfile defaults pin the `linux/arm64` manifests for
`rust:1.99.0-alpine3.23` (`sha256:58d81a4631c273a704b725e6e3f2b0f207301c82bcb50523f082eaed6d21e768`)
and `alpine:3.23` (`sha256:8793abc479381bafd16c9d538b01c5a8dd7eef6aa10a737f2ad828ac73cb80e9`).
The Dockerfile rejects other target architectures and explicitly selects the
builder's installed `1.99.0-aarch64-unknown-linux-musl` toolchain, overriding
the workspace's floating `stable` channel. The Rust builder and runtime both
use musl/Alpine; the runtime installs `ca-certificates=20261020-r0` from the
Alpine 3.23 repository. Base manifests and the package version are pinned, but
package availability still depends on the repository retaining that version.
Build arguments can override the defaults and must remain digest-pinned and
reviewed. The root `.dockerignore` excludes build outputs and common local
secret files from the build context.

The keeper healthcheck verifies that the expected `nexus-keeper run` process is
still a direct child of the container init process (the workload uses Docker's
init shim to forward stop signals and reap the keeper). The workload lifecycle also
checks the exact container ID, ownership label, running state, and Docker health
status before constructing its file/exec adapters. This is a process-liveness
gate only; it does not validate network isolation, worker correctness, or
security of an image that has not been reviewed and pinned.

The lifecycle explicitly passes `--entrypoint /usr/local/bin/nexus-keeper`
and the `run` argument; overriding the image entrypoint without that argument
makes the keeper exit with status 2. Both local `sha256:<image-id>` references
and `repository@sha256:<digest>` references use the gateway's immutable-image
validation. Mutable tags are not accepted.

If server session creation returns `503 container environment is unavailable`,
check the host server's stderr for `container environment preparation failed`.
Workload failures name the preparation stage (configuration, creation,
readiness, or transport) without forwarding Docker output or credentials to
the client.

The session composes workload and gateway ownership internally and mounts a
private host-generated resolver file containing only `nameserver 127.0.0.1`.
The resolver file is retained if cleanup becomes uncertain so a possibly-live
workload is not left with its DNS backing removed. A local Docker Desktop arm64
lifecycle smoke check passed with the resolver mount; other host sharing
configurations and network isolation remain unvalidated.

The internal owner caps active plus quarantined environments at two. A durable,
owner-only metadata record is written under `XDG_STATE_HOME/nexus-agent/container`
(fallback: `~/.local/state/nexus-agent/container`) before workload creation. It
contains only generated names/IDs and trusted Docker/resolver paths. A process
lock prevents another Nexus process from taking ownership; a stale lock or
leftover record fails closed and is never automatically removed. Operator-facing
inspection and targeted restart reconciliation are not implemented.

These are development prototype inputs, not approved release image inputs. Do
not publish this prototype as a supported image until its architecture,
licenses, toolchain, dependency provenance, and runtime contents are reviewed.
The lifecycle is available only through opt-in container-enabled TUI/server
builds and explicit execution-mode flags. Native Linux network/isolation
validation remains outstanding; successful startup is not security approval.
