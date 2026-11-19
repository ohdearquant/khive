#!/usr/bin/env bash
# ci-install-ripgrep.sh: install ripgrep on a hosted Ubuntu runner.
#
# The repository-wide identifier ban in tests/khive-contract runs ripgrep and fails (rather than skips)
# when CI lacks rg. ubuntu-latest does not include it, so the full-suite shard that runs the contract suite
# and the docs job, which runs the ban on the documentation lane, both install it here.
set -euo pipefail

# The runner image carries third-party apt lists this job needs
# nothing from, and `apt-get update` exits 100 when any one of them
# serves a bad index. Google's chrome list did exactly that on
# 2026-09-09 (Hash Sum mismatch) and reddened every full-suite
# shard. ripgrep comes from the Ubuntu archive, so disable that list
# instead of retrying into the same mismatch. apt reads only .list
# and .sources files, so a renamed copy is inert, and the Ubuntu
# archive itself lives in ubuntu.sources and is untouched. A different
# third-party list failing this way needs its own line here, by
# name: this is deliberately not a blanket sweep of the directory.
for f in /etc/apt/sources.list.d/google-chrome.list \
         /etc/apt/sources.list.d/google-chrome.sources; do
  if [ -e "$f" ]; then sudo mv "$f" "$f.disabled"; fi
done
# Print what still points at the chrome repo: empty is the pass, a
# non-empty line says the rename missed a filename this does not know.
sudo grep -rl 'dl[.]google[.]com/linux/chrome' /etc/apt/ || true
sudo apt-get update
sudo apt-get install --yes ripgrep
rg --version
