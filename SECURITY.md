# Security Policy

## Supported Versions

Security fixes target the latest release and `main`. Use the latest published
release; older releases receive no separate backports.

## Reporting a Vulnerability

Use [GitHub private vulnerability reporting](https://github.com/r4ai/buildq/security/advisories/new).
Include the affected version/OS, reproduction steps, and impact. Do not put
credentials, private repository content, or exploit details in a public issue.
If private reporting is unavailable, ask the maintainer to enable it in a public
issue without disclosing vulnerability details.

## Trust Boundaries

buildq runs developer commands with the caller's permissions. A queue slot or
shared target directory is not a security sandbox. Share the queue/cache only
among trusted users and repositories; do not run untrusted build scripts in a
trusted cache. Remote hosts and SSH configuration must also be trusted.

## Supply Chain and Releases

CI checks RustSec advisories, licenses, duplicate versions, and dependency
sources with `deny.toml`; registry dependencies must come from crates.io.
Actions are pinned to full commit SHAs, checkouts do not retain credentials,
and pull requests receive read-only permissions. No dependency/build cache is
shared between pull requests and releases.

Release tags must match `Cargo.toml` and point into `main` history. The release
workflow reruns CI before building five native targets. Separate jobs attest
archives and publish the release, so compilation has no write permission or
OIDC token. Verify both SHA256 checksums and GitHub artifact attestations as
described in the README. Provenance identifies the producing workflow; it does
not certify that the source or executable is free of vulnerabilities.

Maintainers should protect `main` and version tags, review workflow and lockfile
changes, enable Dependabot security updates and private vulnerability reporting,
and review pinned Rust, cargo-deny, and actionlint versions alongside dependency
updates. Weekly CI also checks newly published advisories.
