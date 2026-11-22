# Security Policy

## Supported Versions

Only the latest minor release receives security fixes:

| Version | Supported |
| ------- | --------- |
| `0.9.x` | Yes       |
| `< 0.9` | No        |

## Reporting a Vulnerability

Please **do not** disclose security vulnerabilities in public GitHub issues, pull requests, or Discussions.

Instead, report vulnerabilities privately through **GitHub Private Vulnerability Reporting**:

- https://github.com/harshit-d3v/fluxwright/security/advisories/new

Include as much detail as needed to reproduce the issue (affected version, component, steps or minimal PoC, and potential impact).

## Scope & Response

Areas of particular security interest include:

- Authentication, token handling, and TLS configuration for `fluxwright serve`, `/cdp`, and `fluxwright coordinate`
- SSRF, scheme, or local-network URL validation in `fluxwright-mcp` and page/route APIs
- Browser context and persistent profile directory isolation
- Critical vulnerabilities in workspace dependencies

We aim to acknowledge private reports within **72 hours** and work with reporters to verify, patch, and coordinate disclosure once a fix is released.
