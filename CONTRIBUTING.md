# Contributing to Nexus Agent

Thanks for helping improve Nexus Agent. The project is an evolving Rust coding
agent in alpha, with a live TUI, loopback HTTP/SSE server, provider adapters,
and local or opt-in container-backed tools. Real coding tasks have been
evaluated, but neither M0 acceptance nor cross-platform security readiness is
complete. Keep proposals aligned with the documented boundaries and clearly
separate implemented behavior from design-stage work.

## Before opening an issue

- Check the [README](README.md), [documentation index](docs/index.md), and
  [project status](README.md#project-status) for existing behavior and known
  limitations.
- Use the Bug Report, Feature Request, or Performance Regression form so the
  report includes the context maintainers need.
- Do not include API keys, access tokens, private source, or unredacted personal
  data in issues, logs, screenshots, or terminal transcripts. For a suspected
  security vulnerability, do not file a public issue; follow the
  [security policy](SECURITY.md) for private reporting.

## Development setup

Install a current stable Rust toolchain with the `rustfmt` and `clippy`
components. The workspace uses Rust edition 2024 and pins dependency resolution
in `Cargo.lock`. Install `cargo-nextest` for the documented integration-test
workflow.

```sh
git clone https://github.com/immane/nexus-agent.git
cd nexus-agent
cargo build --workspace --locked --offline
```

If the dependency cache is not populated, omit `--offline` for the initial
build. Live-provider runs are opt-in and may incur provider costs; routine
development and tests should use the offline scripted fixtures.

## Validate changes

Follow [`docs/testing.md`](docs/testing.md). Run affected-package Cargo
operations sequentially because they share the target directory. For a TUI
change, the usual checks are:

```sh
cargo fmt --all -- --check
cargo nextest run -p nexus-tui --locked --offline
cargo clippy -p nexus-tui --all-targets --locked --offline -- -D warnings
```

Use the relevant package selection for other changes. Add deterministic tests
for behavior changes, including failure and boundary cases. Do not run paid
live-provider tests as a routine check. The optional low-value suite is not part
of normal validation; see `docs/testing.md` before changing its subjects.

## Implementation and documentation

- Keep the runtime as the authoritative path for policy, authorization,
  cancellation, and resource limits. Frontends and tools must not bypass it.
- Preserve finite resource budgets and the existing project/path confinement
  rules. A suggestion, preview, or partial model response is not authorization.
- Prefer small changes that follow nearby naming, layering, and test patterns.
- Update the relevant contract/design/task documentation when public behavior,
  commands, configuration, or safety properties change.
- TUI, server, tools, and container integration test modules belong in their
  `tests/behavior.rs` entry points; automatic test target discovery is disabled
  for those packages.
- Do not add generated build output, credentials, local configuration, or
  provider responses to a patch.

For benchmark changes, follow [the benchmark workflow](tools/benchmarks/README.md).
Start SWE-bench model runs as background jobs, preserve logs and artifacts, and
record interruptions without automatically retrying. Hosted inference requires
explicit opt-in and may incur uncapped costs. Keep agent completion separate
from official resolution, and do not present benchmark success as security or
isolation validation.

## Pull requests

Keep pull requests focused. Include:

1. A short problem statement and the user-visible behavior change.
2. The affected crates or documentation and any compatibility/safety impact.
3. The exact validation commands run and their results; call out checks that
   were skipped or blocked.
4. Screenshots or a short terminal recording for visible TUI changes, after
   checking that they contain no secrets or private paths.

Use English commit messages; when following Conventional Commits, prefer a
specific scope such as `feat(tui): ...` or `docs: ...`. Do not bundle unrelated
formatting or dependency updates with a behavior change.
