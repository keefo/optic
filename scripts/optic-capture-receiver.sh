#!/bin/bash

# Restricted forced command used by the iMac capture-only SSH daemon.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/bin:/bin:/usr/sbin:/sbin"

DESTINATION="/Users/admin/Pictures/Optic"
INCOMING="$DESTINATION/.incoming"
MAX_FILE_BYTES=268435456
TEMPORARY=""

cleanup() {
    [[ -z "$TEMPORARY" ]] || rm -f -- "$TEMPORARY"
}
trap cleanup EXIT HUP INT TERM

fail() {
    printf 'ERROR %s\n' "$1" >&2
    exit 1
}

set -f
set -- ${SSH_ORIGINAL_COMMAND:-}
operation=${1:-}

case "$operation" in
    ping)
        [[ $# -eq 1 ]] || fail "invalid ping request"
        printf '%s\n' 'OK receiver'
        ;;
    put)
        [[ $# -eq 4 ]] || fail "invalid put request"
        encoded_name=$2
        expected_size=$3
        expected_sha=$4

        [[ "$expected_size" =~ ^[0-9]+$ ]] || fail "invalid size"
        ((expected_size <= MAX_FILE_BYTES)) || fail "file exceeds receiver limit"
        [[ "$expected_sha" =~ ^[0-9a-f]{64}$ ]] || fail "invalid SHA-256"
        name=$(printf '%s' "$encoded_name" | /usr/bin/base64 -D 2>/dev/null) ||
            fail "invalid filename encoding"
        case "$name" in
            ""|.*|*/*|*$'\n'*|*$'\r'*) fail "invalid filename" ;;
        esac

        [[ -d "$DESTINATION" && -d "$INCOMING" ]] || fail "destination unavailable"
        final="$DESTINATION/$name"
        [[ ! -L "$final" ]] || fail "destination is a symbolic link"

        umask 077
        TEMPORARY=$(mktemp "$INCOMING/.optic-receive.XXXXXX")
        ulimit -f 524288
        cat > "$TEMPORARY"

        actual_size=$(stat -f '%z' "$TEMPORARY")
        actual_sha=$(shasum -a 256 "$TEMPORARY" | awk '{print $1}')
        [[ "$actual_size" == "$expected_size" ]] || fail "size mismatch"
        [[ "$actual_sha" == "$expected_sha" ]] || fail "SHA-256 mismatch"

        if [[ -e "$final" ]]; then
            existing_size=$(stat -f '%z' "$final")
            existing_sha=$(shasum -a 256 "$final" | awk '{print $1}')
            if [[ "$existing_size" == "$expected_size" &&
                  "$existing_sha" == "$expected_sha" ]]; then
                rm -f -- "$TEMPORARY"
                TEMPORARY=""
                printf 'OK existing %s %s\n' "$expected_size" "$expected_sha"
                exit 0
            fi
            fail "destination conflict"
        fi

        chmod 0640 "$TEMPORARY"
        mv -n -- "$TEMPORARY" "$final"
        if [[ -e "$TEMPORARY" ]]; then
            existing_size=$(stat -f '%z' "$final")
            existing_sha=$(shasum -a 256 "$final" | awk '{print $1}')
            if [[ "$existing_size" == "$expected_size" &&
                  "$existing_sha" == "$expected_sha" ]]; then
                rm -f -- "$TEMPORARY"
                TEMPORARY=""
                printf 'OK existing %s %s\n' "$expected_size" "$expected_sha"
                exit 0
            fi
            fail "destination conflict"
        fi
        TEMPORARY=""
        sync

        committed_size=$(stat -f '%z' "$final")
        committed_sha=$(shasum -a 256 "$final" | awk '{print $1}')
        [[ "$committed_size" == "$expected_size" ]] || fail "committed size mismatch"
        [[ "$committed_sha" == "$expected_sha" ]] || fail "committed SHA-256 mismatch"
        printf 'OK stored %s %s\n' "$expected_size" "$expected_sha"
        ;;
    *)
        fail "unsupported operation"
        ;;
esac