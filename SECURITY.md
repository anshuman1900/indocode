# Security policy

## Supported versions

Only the [latest release](https://github.com/aicodeking/pandacode/releases/latest)
receives security fixes. Please update before reporting.

## Reporting a vulnerability

Please do **not** open a public issue for security problems.

Report it privately through GitHub Security Advisories: open the repository's
[Security tab](https://github.com/aicodeking/pandacode/security) and click
**Report a vulnerability**. Include:

- the Indocode version, OS and architecture (Settings → About can copy these),
- what an attacker can do and the steps to reproduce,
- any proof of concept, logs or screenshots (remove secrets and personal data).

We aim to acknowledge reports within a few days, keep you updated while we
investigate, and credit you in the advisory unless you prefer otherwise. Please
give us a reasonable time to release a fix before disclosing publicly.

## Scope

In scope: the desktop app in this repository (Rust core, webview frontend,
IPC boundary, local servers such as the bots web chat, credential storage) and
the release workflow.

Out of scope: vulnerabilities in third-party coding CLIs or provider services
that Indocode launches or connects to — report those to their vendors. Actions
an agent takes after you grant it Full access, or approve them, are expected
behavior.
