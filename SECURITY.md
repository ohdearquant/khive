# Security Policy

## Supported versions

Security fixes are released for the latest published minor version of khive. Older minor versions do not
receive fixes; upgrade to the latest release to get them.

## Reporting a vulnerability

Please do not report security vulnerabilities through public GitHub issues, discussions or pull requests.

Send a description of the vulnerability to **leo@khive.ai**. Include:

- A description of the vulnerability and its potential impact
- The version or commit you tested, and how khive was run (command line, MCP client, hosted endpoint)
- Steps to reproduce or a proof of concept, if you have one
- Any mitigation you are aware of

## What happens next

- **Acknowledgement within 72 hours** of your report, with a tracking reference.
- **Assessment**: we confirm or rule out the issue and agree a severity with you.
- **Fix target**: 14 days from acknowledgement for a critical vulnerability. Other severities are fixed in a
  scheduled release under coordinated disclosure, within 90 days of the report. If a fix needs longer, we
  tell you the timeline and the reason before the deadline passes.
- **Disclosure**: we publish a GitHub security advisory for the repository when the fixed release is out, and
  credit you unless you ask us not to.

## Advisories in releases

Every release that fixes a vulnerability carries a line in `CHANGELOG.md` naming the advisory, the affected
versions and the fixed version.

## Dependencies

Advisories against khive's locked dependencies open a security update pull request automatically, and
`cargo-deny` checks the Rust dependency tree against the RustSec advisory database on every pull request
that changes more than documentation.
