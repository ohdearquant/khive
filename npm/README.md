# khive

Research knowledge graph CLI — git-native KG versioning.

## Install

```bash
npm install -g khive
# or
npx khive kg init
```

## Commands

```bash
khive kg init              # Initialize .khive/kg/ in a git repo
khive kg validate          # Check NDJSON files against schema
khive kg commit -m "msg"   # Validate + stage + git commit
khive kg sync              # Validate NDJSON (DB rebuild: Phase C2)
khive kg status            # Show entity/edge counts and uncommitted changes
```

## Documentation

- [GitHub](https://github.com/ohdearquant/khive)
