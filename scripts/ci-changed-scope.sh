#!/usr/bin/env bash

# Classify a CI change set. Reads changed file paths (one per line) on
# stdin and prints two lines:
#
#   docs_only=true   only when every path is documentation; anything else,
#                    including empty input, prints docs_only=false so CI
#                    runs in full.
#   timelapse=true   when any path is a non-doc file under tools/timelapse/
#                    or the CI definition that governs its job; empty input
#                    also prints timelapse=true (fail open).
#
# Used by .github/workflows/ci.yml; see docs/optic-daemon-ci-cd.md §5.1.

set -euo pipefail

count=0
docs_only=true
timelapse=false
while IFS= read -r path; do
    [[ -z "$path" ]] && continue
    count=$((count + 1))
    case "$path" in
        *.md | docs/* | worklogs/* | case/*) ;;
        *) docs_only=false ;;
    esac
    case "$path" in
        *.md) ;;
        tools/timelapse/* | .github/workflows/ci.yml | scripts/ci-changed-scope.sh | rust-toolchain.toml)
            timelapse=true
            ;;
    esac
done

if ((count == 0)); then
    docs_only=false
    timelapse=true
fi
echo "docs_only=$docs_only"
echo "timelapse=$timelapse"
