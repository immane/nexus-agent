# Security Policy

## Supported versions

Nexus Agent has no stable release yet. Security fixes are made against the
latest `main` branch; older commits and experimental builds are not maintained
security branches. The current alpha includes live provider access and real
coding tools, but neither M0 acceptance nor cross-platform security readiness
is complete. Successful coding benchmarks do not establish a production
security boundary.

## Reporting a vulnerability

Please do not report suspected vulnerabilities in a public issue, discussion,
or pull request. Use GitHub's **Report a vulnerability** option on this
repository's Security page to send a private report. If that option is
unavailable, contact the repository maintainer through their GitHub profile
and request a private channel before sharing technical details.

Include, where possible:

- the affected commit, version, platform, and configuration;
- the impact and security boundary affected;
- minimal reproduction steps or a proof of concept; and
- any relevant logs with credentials, tokens, and personal data removed.

Please allow the maintainer a reasonable opportunity to investigate and
coordinate a fix before publicly disclosing the issue. There is currently no
published response-time guarantee or bug-bounty program. The maintainer will
acknowledge and follow up as availability permits.

## Scope and safe testing

Reports are most useful when they demonstrate an impact on the implementation
that exists in this repository, such as approval enforcement, filesystem
scoping, credential handling, provider requests, or unsafe handling of
untrusted model/tool output. Features described only as planned or draft are
not implemented security guarantees.

The optional container backend is a feature-gated, explicitly selectable
prototype. Docker Desktop checks and offline SWE-bench runs are not complete
isolation validation. Do not rely on it as an approved security boundary;
native Linux host/LAN, metadata, DNS/IPv6, and public egress validation remains
outstanding. Its `host_exec.shell_command` interface runs inside the workload
container, not on the host. Use only disposable workspaces without secrets or
valuable data. See the [container validation notes](containers/README.md).

The filesystem root restriction is not an OS sandbox. Local `host_exec` requires
an OS sandbox and fails closed when it is unavailable, but Linux hardening is
not yet verified and process-group cleanup cannot prove that all escaped
descendants have terminated. Project/temp operations are automatic by default;
use `--strict-tools` for stricter confirmation. Planned external plugins are
not implemented and must not be assumed to provide untrusted-code containment.

The HTTP server binds to loopback, but loopback is not isolation from other
local processes. Its bearer token authorizes approval and control operations;
protect it like a provider credential. Provider requests can send workspace
content to the configured service, so use only providers authorized to receive
that data. Optional persisted session records and benchmark artifacts may
contain source code and tool output; treat them as sensitive and redact them
before sharing. See [Safety and Trust](README.md#safety-and-trust).

Do not test against systems, accounts, provider endpoints, or data that you do
not own or have explicit permission to use. Avoid destructive actions, denial
of service, and access to other users' data while investigating.

Please report vulnerabilities in third-party dependencies to their respective
maintainers as well; include only the Nexus Agent-specific impact in your
private report here.
