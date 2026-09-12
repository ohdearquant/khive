#!/usr/bin/env bash
# Pre-fetch embedding model files into the lattice model cache.
#
# WHY THIS EXISTS. The embedder fetches its weights from huggingface.co the first
# time it is built. CI built it on every run, and on 2026-09-12 Hugging Face
# answered with HTTP 429 twelve times in a single job, turning an unrelated pull
# request red. A rate limit on a third-party CDN is not a signal about the change
# under test, and while it is intermittent it makes a real red indistinguishable
# from an infrastructure one at a glance.
#
# The fix is not a retry loop. It is to fetch once, cache the result, and then run
# the tests with LATTICE_OFFLINE set so the cache is load-bearing: a run that would
# have fetched fails immediately, naming the directory it expected, instead of
# reaching the network at all.
#
# WHAT IT MIRRORS, AND WHY THAT IS SAFE. The cache layout and the model-name to
# repository mapping below are lattice-inference's, in its `download` module. This
# script is a second copy of that table and can drift from it. It is safe to copy
# because the crate checksum-verifies a pre-fetched cache before using it (with the
# artifact preserved rather than deleted), so a drifted mapping here surfaces as a
# named checksum failure at the first use, never as wrong bytes served quietly.
#
# Usage: scripts/warm-model-cache.sh <model-name> [model-name...]
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <model-name> [model-name...]" >&2
  exit 2
fi

CACHE_ROOT="${LATTICE_MODEL_CACHE:-$HOME/.lattice/models}"

# Deliberately NOT honouring LATTICE_OFFLINE: this script is the thing that fills
# the cache, so it is the one caller that must be allowed to reach the network.
# Every consumer of the cache runs with the flag set.
hf_repo_for() {
  case "$1" in
    all-minilm-l6-v2) echo "sentence-transformers/all-MiniLM-L6-v2" ;;
    bge-small-en-v1.5) echo "BAAI/bge-small-en-v1.5" ;;
    bge-base-en-v1.5) echo "BAAI/bge-base-en-v1.5" ;;
    bge-large-en-v1.5) echo "BAAI/bge-large-en-v1.5" ;;
    multilingual-e5-small) echo "intfloat/multilingual-e5-small" ;;
    multilingual-e5-base) echo "intfloat/multilingual-e5-base" ;;
    paraphrase-multilingual-minilm-l12-v2) echo "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2" ;;
    *) return 1 ;;
  esac
}

# The tokenizer file a model ships is selected by substring, not by family: a name
# containing "e5-" or "multilingual" carries tokenizer.json, everything else
# carries vocab.txt.
tokenizer_file_for() {
  case "$1" in
    *e5-*|*multilingual*) echo "tokenizer.json" ;;
    *) echo "vocab.txt" ;;
  esac
}

fetch() {
  local url="$1" dest="$2"
  if [ -s "$dest" ]; then
    echo "present  $dest"
    return 0
  fi
  echo "fetching $url"
  # --fail so an HTML error page never lands as a weights file; the retry budget
  # covers the rate limit this script exists for, and 429 is retryable only with
  # --retry-all-errors on curl's default list.
  curl --fail --location --silent --show-error \
       --retry 5 --retry-delay 5 --retry-all-errors \
       --output "$dest.partial" "$url"
  mv "$dest.partial" "$dest"
  echo "fetched  $dest ($(wc -c < "$dest") bytes)"
}

for model in "$@"; do
  repo="$(hf_repo_for "$model")" || {
    echo "unsupported model name: $model" >&2
    exit 3
  }
  dir="$CACHE_ROOT/$model"
  mkdir -p "$dir"
  base="https://huggingface.co/$repo/resolve/main"
  fetch "$base/model.safetensors" "$dir/model.safetensors"
  fetch "$base/$(tokenizer_file_for "$model")" "$dir/$(tokenizer_file_for "$model")"
done

echo "cache root: $CACHE_ROOT"
