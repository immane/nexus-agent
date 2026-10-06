# Security Policy

## Supported versions

Nexus Agent has no stable release yet. Security fixes are made against the
latest `main` branch; older commits and experimental builds are not maintained
security branches. The current M0 implementation is test-only and is not
accepted as a production security boundary.

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

The filesystem root restriction is not an OS sandbox, and explicitly enabled
external plugins run with the launching user's OS privileges. Do not test
against systems, accounts, provider endpoints, or data that you do not own or
have explicit permission to use. Avoid destructive actions, denial of service,
and access to other users' data while investigating.

Please report vulnerabilities in third-party dependencies to their respective
maintainers as well; include only the Nexus Agent-specific impact in your
private report here.
