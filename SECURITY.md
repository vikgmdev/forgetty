# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.x.x   | Latest release only |

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

Instead, please use [GitHub Security Advisories](https://github.com/vikgmdev/forgetty/security/advisories/new) to report vulnerabilities privately. Include:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will acknowledge receipt within 48 hours and provide a detailed response within 7 days.

## Scope

Security issues in the following areas are in scope:

- Terminal escape sequence handling (injection attacks)
- Clipboard security (paste protection)
- Socket API authentication
- Sync service encryption
- PTY process isolation
