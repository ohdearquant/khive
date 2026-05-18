# Release: khive v{VERSION}

**Date**: {YYYY-MM-DD}
**Tag**: `v{VERSION}`
**Crates**: https://crates.io/crates/khive-mcp

---

## Highlights

<!-- 2-3 sentences on what this release means for users. Not a changelog — the story. -->

## Breaking changes

<!-- List any breaking changes. If none, write "None." -->

- None.

## New features

<!-- One bullet per feature. Link to ADR if relevant. -->

-

## Bug fixes

<!-- One bullet per fix. Reference PR/issue if available. -->

-

## Crates published

| Crate         | Version   | crates.io                                      |
| ------------- | --------- | ---------------------------------------------- |
| khive-types   | {VERSION} | [link](https://crates.io/crates/khive-types)   |
| khive-score   | {VERSION} | [link](https://crates.io/crates/khive-score)   |
| khive-storage | {VERSION} | [link](https://crates.io/crates/khive-storage) |
| khive-db      | {VERSION} | [link](https://crates.io/crates/khive-db)      |
| khive-query   | {VERSION} | [link](https://crates.io/crates/khive-query)   |
| khive-runtime | {VERSION} | [link](https://crates.io/crates/khive-runtime) |
| khive-mcp     | {VERSION} | [link](https://crates.io/crates/khive-mcp)     |

## Install

```bash
cargo install khive-mcp@{VERSION}
```

## MCP configuration

```json
{
  "mcpServers": {
    "khive": {
      "command": "khive-mcp",
      "args": []
    }
  }
}
```

## Migration notes

<!-- For users upgrading from a previous version. Schema migrations, config changes, etc. -->
<!-- If first release, write "First public release — no migration needed." -->

## Pre-release checklist

- [ ] All workspace tests pass: `make ci`
- [ ] Smoke test passes: `python3 tests/smoke_test.py`
- [ ] No secrets in crate source: grep for tokens, keys, internal URLs
- [ ] Crate descriptions present on all workspace crates
- [ ] Inter-crate deps have `version = "{VERSION}"` alongside `path`
- [ ] `CHANGELOG.md` updated (if maintained)
- [ ] README status section updated
- [ ] Git tag created: `git tag v{VERSION} && git push origin v{VERSION}`
- [ ] `make publish-dry` succeeds
- [ ] `make publish` completes (all workspace crates on crates.io)
- [ ] GitHub release created from tag with this document as body

## Known issues

<!-- Anything users should be aware of. Workarounds if available. -->

-

## What's next

<!-- 1-3 bullets on what's planned for the next release. -->

-
