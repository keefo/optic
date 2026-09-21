#!/usr/bin/env bash

# Classify a CI change set. Reads changed file paths (one per line) on
# stdin and prints `docs_only=true` only when every path is documentation;
# anything else, including empty input, prints `docs_only=false` so CI
# runs in full. Used by .github/workflows/ci.yml; see
# docs/optic-daemon-ci-cd.md §5.1.

set -euo pipefail

count=0
while IFS= read -r path; do
    [[ -z "$path" ]] && continue
    count=$((count + 1))
    case "$path" in
        *.md | docs/* | worklogs/* | case/*) ;;
        *)
            echo "docs_only=false"
            exit 0
            ;;
    esac
done

if ((count == 0)); then
    echo "docs_only=false"
else
    echo "docs_only=true"
fi
