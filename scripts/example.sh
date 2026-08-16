#!/bin/sh
set -eu

printf 'fixed argument: %s\n' "${1:-}"
shift || true

index=0
for argument in "$@"; do
  index=$((index + 1))
  printf 'request argument %d: %s\n' "$index" "$argument"
done

printf 'example stderr line\n' >&2
