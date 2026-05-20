#!/bin/bash
set -euo pipefail

VERSION="${1:-$(jq -r .version npm/package.json)}"

echo "Publishing khive v${VERSION} to npm..."

# Compile all platform binaries
bash "$(dirname "$0")/compile.sh" "$VERSION"

# Update version in package.json
cd npm
jq --arg v "$VERSION" '.version = $v' package.json > package.json.tmp
mv package.json.tmp package.json

# Publish
npm publish --access public

echo "Published: https://www.npmjs.com/package/khive"
