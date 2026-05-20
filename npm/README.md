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
khive kg init          # Initialize .khive/kg/ in a git repo
khive kg commit -m "msg"  # Export + validate + git commit
khive kg sync          # Rebuild working.db from NDJSON
khive kg status        # Show entity/edge counts
khive kg validate      # Check NDJSON against schema
```

## Documentation

- [GitHub](https://github.com/ohdearquant/khive)
- [khive.ai](https://khive.ai)
