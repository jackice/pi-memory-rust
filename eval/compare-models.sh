#!/usr/bin/env bash
set -euo pipefail

candidate_arg_count=0
for argument in "$@"; do
  [[ "$argument" = "--" ]] && break
  candidate_arg_count=$((candidate_arg_count + 1))
done
if (( candidate_arg_count == 0 || candidate_arg_count % 3 != 0 )); then
  cat >&2 <<'USAGE'
Usage: eval/compare-models.sh <model-id> <model-dir> <manifest.json> [<model-id> <model-dir> <manifest.json> ...] [-- <eval args>]

Each candidate needs a pinned manifest with model_file, tokenizer_file, all
required weight files, dimension, max_length, pooling, and optional
query_prefix/document_prefix fields. The script
uses a fresh temporary DB per candidate, seeds the same fixture, and runs the same
40-query evaluation so model vectors never overwrite each other.
USAGE
  exit 2
fi

candidate_args=()
while (( $# > 0 )) && [[ "$1" != "--" ]]; do
  candidate_args+=("$1")
  shift
done
eval_args=()
if (( $# > 0 )); then
  shift
  eval_args=("$@")
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
caller_dir="$PWD"
cd "$repo_root"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/pi-memory-model-eval.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT
run=0
for ((index = 0; index < candidate_arg_count; index += 3)); do
  model_id="${candidate_args[index]}"
  model_dir="${candidate_args[index + 1]}"
  manifest="${candidate_args[index + 2]}"
  [[ "$model_dir" = /* ]] || model_dir="$caller_dir/$model_dir"
  [[ "$manifest" = /* ]] || manifest="$caller_dir/$manifest"
  run=$((run + 1))
  printf '\n=== candidate %s: %s ===\n' "$run" "$model_id"
  PI_MEMORY_EMBEDDING_MODEL="$model_id" \
    PI_MEMORY_MODEL_PATH="$model_dir" \
    PI_MEMORY_MODEL_MANIFEST="$manifest" \
    cargo run --release -p pi-memory-core --bin pi-memory-eval -- \
      --db "$work_dir/candidate-$run.db" \
      --scope "$work_dir/project" \
      --seed-fixture "${eval_args[@]}"
done
