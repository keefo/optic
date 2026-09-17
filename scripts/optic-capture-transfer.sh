#!/usr/bin/env bash

# Transfer stable files from the Pi RAM stage to the restricted iMac receiver.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

CONFIG_FILE="/etc/optic/capture-transfer.conf"
[[ -r "$CONFIG_FILE" ]] || { printf 'Missing %s\n' "$CONFIG_FILE" >&2; exit 1; }
# shellcheck source=/dev/null
source "$CONFIG_FILE"

: "${STAGING_DIR:?missing STAGING_DIR}"
: "${REMOTE_HOST:?missing REMOTE_HOST}"
: "${REMOTE_PORT:?missing REMOTE_PORT}"
: "${REMOTE_USER:?missing REMOTE_USER}"
: "${IDENTITY_FILE:?missing IDENTITY_FILE}"
: "${KNOWN_HOSTS_FILE:?missing KNOWN_HOSTS_FILE}"

[[ "$(findmnt -n -o FSTYPE "$STAGING_DIR" 2>/dev/null || true)" == "tmpfs" ]] || {
    printf '%s is not a tmpfs mount; refusing transfer.\n' "$STAGING_DIR" >&2
    exit 1
}

exec 9>"$STAGING_DIR/.transfer.lock"
flock -n 9 || exit 0

ssh_options=(
    -T -p "$REMOTE_PORT"
    -o BatchMode=yes
    -o IdentitiesOnly=yes
    -o StrictHostKeyChecking=yes
    -o UserKnownHostsFile="$KNOWN_HOSTS_FILE"
    -o ConnectTimeout=5
    -o ServerAliveInterval=5
    -o ServerAliveCountMax=2
    -i "$IDENTITY_FILE"
)

now=$(date +%s)
transferred=0

while IFS= read -r -d '' file; do
    name=${file##*/}
    case "$name" in
        .*|*/*|*$'\n'*|*$'\r'*) continue ;;
    esac

    modified=$(stat -c '%Y' "$file")
    ((now - modified >= 10)) || continue

    inode_before=$(stat -c '%i' "$file")
    size_before=$(stat -c '%s' "$file")
    sha_before=$(sha256sum "$file" | awk '{print $1}')
    encoded_name=$(printf '%s' "$name" | base64 -w0)

    response=$(ssh "${ssh_options[@]}" "$REMOTE_USER@$REMOTE_HOST" \
        put "$encoded_name" "$size_before" "$sha_before" < "$file")
    [[ "$response" == "OK stored $size_before $sha_before" ||
       "$response" == "OK existing $size_before $sha_before" ]] || {
        printf 'Receiver rejected %s: %s\n' "$name" "${response:-no response}" >&2
        exit 1
    }

    [[ -f "$file" && ! -L "$file" ]] || continue
    inode_after=$(stat -c '%i' "$file")
    size_after=$(stat -c '%s' "$file")
    modified_after=$(stat -c '%Y' "$file")
    sha_after=$(sha256sum "$file" | awk '{print $1}')
    if [[ "$inode_after" != "$inode_before" || "$size_after" != "$size_before" ||
          "$modified_after" != "$modified" || "$sha_after" != "$sha_before" ]]; then
        printf 'Source changed during transfer; retaining %s for review.\n' "$name" >&2
        continue
    fi

    rm -- "$file"
    printf 'Transferred and verified %s (%s bytes).\n' "$name" "$size_before"
    ((transferred += 1))
done < <(find "$STAGING_DIR" -maxdepth 1 -type f ! -name '.*' -print0 | sort -z)

printf 'Capture transfer complete: files=%d\n' "$transferred"