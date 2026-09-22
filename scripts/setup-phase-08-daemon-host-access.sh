#!/usr/bin/env bash

# Configure the host access optic-daemon's dashboard system actions depend on:
# the scoped PolicyKit rules behind the Power menu (Reboot, Shut down), NTP
# Sync now, the Station timezone save, and restarting optic-daemon.service
# (Restart daemon and deploys), plus lingering (for the Beszel agent's user
# service) and the device groups the daemon needs.

set -euo pipefail

export LC_ALL=C
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

ORIGINAL_ARGS=("$@")
DRY_RUN=0
PRINT_DIR=""
DAEMON_USER="liam"
REQUIRED_GROUPS=(video render)
BACKUP_DIR="/var/backups/optic-hardening"
RULES_DIR="/etc/polkit-1/rules.d"
RULE_NAMES=(
    60-optic-daemon-reboot.rules
    61-optic-daemon-ntp-sync.rules
    62-optic-daemon-set-timezone.rules
    63-optic-daemon-power-off.rules
    64-optic-daemon-manage-unit.rules
)
LINGER_FILE="/var/lib/systemd/linger/$DAEMON_USER"
TEMPORARY=""
SUBJECT_PID=""

usage() {
    cat <<'EOF'
Usage: setup-phase-08-daemon-host-access.sh [--dry-run] [--print-rules DIR] [--help]

Idempotently installs the PolicyKit rules, lingering, and group membership
that optic-daemon's dashboard system actions depend on.

  --dry-run          Report required changes without modifying the system.
                     Run it with sudo to compare the installed rules, which
                     are readable only by root and polkitd.
  --print-rules DIR  Write the managed rule files into DIR and exit; needs no
                     privileges and touches nothing else.
  --help             Display this help.
EOF
}

while (($#)); do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --print-rules)
            [[ $# -ge 2 ]] || { printf '%s\n' 'Missing value for --print-rules.' >&2; exit 64; }
            PRINT_DIR=$2
            shift
            ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 64 ;;
    esac
    shift
done

# Each rule grants exactly one action to the daemon's user. The text matches
# the rules originally installed by hand on the Pi byte for byte, comments
# included, so the dry run reports [OK] there. The daemon runs
# these actions over D-Bus rather than sudo, because its sandbox cannot use
# sudo (worklogs/2026-09-19-reboot-nonewprivileges-fix.md).
rule_content() {
    case "$1" in
        60-optic-daemon-reboot.rules)
            cat <<'EOF'
// Allow liam to reboot the Pi without interactive PolicyKit authentication.
// Scoped to exactly this one action for optic-daemon's reboot control.
// See worklogs/2026-09-19-reboot-nonewprivileges-fix.md.
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.login1.reboot" &&
        subject.user == "liam") {
        return polkit.Result.YES;
    }
});
EOF
            ;;
        61-optic-daemon-ntp-sync.rules)
            cat <<'EOF'
// Allow liam to restart systemd-timesyncd.service (force an immediate NTP
// resync) without interactive PolicyKit authentication. Scoped to exactly
// this one unit + verb for optic-daemon Config page's "Sync now"
// control -- NOT a blanket grant to manage any systemd unit.
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.systemd1.manage-units" &&
        action.lookup("unit") == "systemd-timesyncd.service" &&
        action.lookup("verb") == "restart" &&
        subject.user == "liam") {
        return polkit.Result.YES;
    }
});
EOF
            ;;
        62-optic-daemon-set-timezone.rules)
            cat <<'EOF'
// Allow liam to set the system timezone (org.freedesktop.timedate1.SetTimezone)
// without interactive PolicyKit authentication, for optic-daemon Config
// page Station-timezone-save integration.
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.timedate1.set-timezone" &&
        subject.user == "liam") {
        return polkit.Result.YES;
    }
});
EOF
            ;;
        63-optic-daemon-power-off.rules)
            cat <<'EOF'
// Allow liam to power off the Pi without interactive PolicyKit
// authentication. Scoped to exactly this one action for optic-daemon's
// Shut down Pi control.
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.login1.power-off" &&
        subject.user == "liam") {
        return polkit.Result.YES;
    }
});
EOF
            ;;
        64-optic-daemon-manage-unit.rules)
            cat <<'EOF'
// Allow liam to start, stop, and restart optic-daemon.service (a system
// service running as liam) without interactive PolicyKit authentication,
// for the dashboard's Restart daemon control and unprivileged deploys.
// Scoped to exactly this one unit and these verbs -- NOT enable/disable,
// unit-file changes, or any other unit.
// See docs/optic-daemon-system-service.md.
polkit.addRule(function(action, subject) {
    var verb = action.lookup("verb");
    if (action.id == "org.freedesktop.systemd1.manage-units" &&
        action.lookup("unit") == "optic-daemon.service" &&
        (verb == "start" || verb == "stop" || verb == "restart") &&
        subject.user == "liam") {
        return polkit.Result.YES;
    }
});
EOF
            ;;
        *) printf 'Unknown rule: %s\n' "$1" >&2; return 1 ;;
    esac
}

cleanup() {
    [[ -z "$TEMPORARY" ]] || rm -f -- "$TEMPORARY"
    [[ -z "$SUBJECT_PID" ]] || kill "$SUBJECT_PID" 2>/dev/null || true
}
trap cleanup EXIT

rule_matches() {
    local name=$1
    [[ -r "$RULES_DIR/$name" ]] && cmp -s "$RULES_DIR/$name" <(rule_content "$name")
}

rule_metadata() {
    stat -c '%a %U:%G' "$RULES_DIR/$1" 2>/dev/null || true
}

user_in_group() {
    id -nG "$DAEMON_USER" 2>/dev/null | tr ' ' '\n' | grep -qx "$1"
}

if [[ -n "$PRINT_DIR" ]]; then
    install -d -m 0755 "$PRINT_DIR"
    for name in "${RULE_NAMES[@]}"; do
        rule_content "$name" > "$PRINT_DIR/$name"
        printf 'Wrote %s\n' "$PRINT_DIR/$name"
    done
    exit 0
fi

if ((DRY_RUN)); then
    printf '%s\n' 'Project Optic Phase 8 daemon host access dry run'
    if ! id "$DAEMON_USER" >/dev/null 2>&1; then
        printf '[MISSING] User %s does not exist.\n' "$DAEMON_USER"
    fi
    if ! command -v pkaction >/dev/null 2>&1; then
        printf '%s\n' '[MISSING] PolicyKit (polkitd) is not installed.'
    fi
    if [[ ! -d "$RULES_DIR" ]]; then
        printf '[MISSING] %s does not exist.\n' "$RULES_DIR"
    elif [[ ! -r "$RULES_DIR" || ! -x "$RULES_DIR" ]]; then
        printf '[UNKNOWN] %s is not readable by %s; rerun the dry run with sudo.\n' \
            "$RULES_DIR" "$(id -un)"
    else
        for name in "${RULE_NAMES[@]}"; do
            path="$RULES_DIR/$name"
            if [[ ! -e "$path" ]]; then
                printf '[CHANGE] Install PolicyKit rule: %s\n' "$path"
            elif [[ ! -r "$path" ]]; then
                printf '[UNKNOWN] %s is not readable; rerun the dry run with sudo.\n' "$path"
            elif ! rule_matches "$name"; then
                printf '[CHANGE] Replace PolicyKit rule (differences below): %s\n' "$path"
                diff -u --label "installed $path" --label "managed $name" \
                    "$path" <(rule_content "$name") || true
            elif [[ "$(rule_metadata "$name")" != "644 root:root" ]]; then
                printf '[CHANGE] Reset %s to 644 root:root (now %s).\n' "$path" "$(rule_metadata "$name")"
            else
                printf '[OK] %s already matches the plan.\n' "$path"
            fi
        done
    fi
    if [[ -e "$LINGER_FILE" ]]; then
        printf '[OK] Lingering is enabled for %s.\n' "$DAEMON_USER"
    else
        printf '[CHANGE] Enable lingering for %s.\n' "$DAEMON_USER"
    fi
    for group in "${REQUIRED_GROUPS[@]}"; do
        if ! getent group "$group" >/dev/null; then
            printf '[MISSING] Group %s does not exist.\n' "$group"
        elif user_in_group "$group"; then
            printf '[OK] %s is in group %s.\n' "$DAEMON_USER" "$group"
        else
            printf '[CHANGE] Add %s to group %s.\n' "$DAEMON_USER" "$group"
        fi
    done
    exit 0
fi

if ((EUID != 0)); then
    if [[ -f "$0" ]] && command -v sudo >/dev/null 2>&1; then
        exec sudo -- "$0" "${ORIGINAL_ARGS[@]}"
    fi
    printf '%s\n' 'This setup must run as root (copy it to the Pi first; it cannot re-run itself through sudo from stdin).' >&2
    exit 77
fi

id "$DAEMON_USER" >/dev/null 2>&1 || {
    printf 'User %s does not exist; no changes were made.\n' "$DAEMON_USER" >&2
    exit 69
}
command -v pkaction >/dev/null 2>&1 && command -v pkcheck >/dev/null 2>&1 || {
    printf '%s\n' 'PolicyKit (polkitd, pkcheck) is not installed; no changes were made.' >&2
    exit 69
}
# polkitd owns the directory and its mode (root:polkitd 0750 on Debian 13);
# only files inside it are managed here.
[[ -d "$RULES_DIR" ]] || {
    printf '%s does not exist; no changes were made.\n' "$RULES_DIR" >&2
    exit 69
}
for group in "${REQUIRED_GROUPS[@]}"; do
    getent group "$group" >/dev/null || {
        printf 'Group %s does not exist; no changes were made.\n' "$group" >&2
        exit 69
    }
done

install_rule() {
    local name=$1 path backup_name backup
    path="$RULES_DIR/$name"
    if rule_matches "$name"; then
        if [[ "$(rule_metadata "$name")" == "644 root:root" ]]; then
            printf '%s already matches the plan.\n' "$path"
        else
            chown root:root "$path"
            chmod 0644 "$path"
            printf 'Reset %s to 644 root:root.\n' "$path"
        fi
        return 0
    fi

    if [[ -e "$path" ]]; then
        install -d -m 0700 "$BACKUP_DIR"
        backup_name=${path#/}
        backup_name=${backup_name//\//_}
        backup="$BACKUP_DIR/$backup_name.$(date +%Y%m%dT%H%M%S).bak"
        cp -a -- "$path" "$backup"
        printf 'Backed up previous rule to %s\n' "$backup"
    fi

    # Write beside the target and rename, so polkitd's directory watch never
    # loads a partially written rule.
    TEMPORARY=$(mktemp "$RULES_DIR/.$name.XXXXXX")
    rule_content "$name" > "$TEMPORARY"
    chmod 0644 "$TEMPORARY"
    chown root:root "$TEMPORARY"
    mv -f -- "$TEMPORARY" "$path"
    TEMPORARY=""
    printf 'Installed %s\n' "$path"
}

printf '%s\n' 'Configuring Project Optic Phase 8 daemon host access'

for name in "${RULE_NAMES[@]}"; do
    install_rule "$name"
done

if [[ -e "$LINGER_FILE" ]]; then
    printf 'Lingering is already enabled for %s.\n' "$DAEMON_USER"
else
    loginctl enable-linger "$DAEMON_USER"
    printf 'Enabled lingering for %s.\n' "$DAEMON_USER"
fi

GROUPS_CHANGED=0
for group in "${REQUIRED_GROUPS[@]}"; do
    if user_in_group "$group"; then
        printf '%s is already in group %s.\n' "$DAEMON_USER" "$group"
    else
        usermod -a -G "$group" "$DAEMON_USER"
        GROUPS_CHANGED=1
        printf 'Added %s to group %s.\n' "$DAEMON_USER" "$group"
    fi
done

# Verify the rules as polkitd actually evaluates them. Passing --detail needs
# a trusted (root) caller, so the subject is a short-lived process owned by
# the daemon user rather than this script. polkitd reloads rules.d on change;
# allow it a moment to pick up a rule that was just written.
subject_uid=$(id -u "$DAEMON_USER")
subject_gid=$(id -g "$DAEMON_USER")
setpriv --reuid="$subject_uid" --regid="$subject_gid" --init-groups -- sleep 60 &
SUBJECT_PID=$!
for _ in {1..20}; do
    [[ "$(stat -c %u "/proc/$SUBJECT_PID" 2>/dev/null || true)" == "$subject_uid" ]] && break
    sleep 0.1
done
[[ "$(stat -c %u "/proc/$SUBJECT_PID" 2>/dev/null || true)" == "$subject_uid" ]] || {
    printf 'Could not start a %s-owned process to check PolicyKit decisions.\n' "$DAEMON_USER" >&2
    exit 1
}

authorized() {
    kill -0 "$SUBJECT_PID" 2>/dev/null || {
        printf '%s\n' 'The PolicyKit check subject exited early.' >&2
        exit 1
    }
    pkcheck --process "$SUBJECT_PID" --action-id "$@" >/dev/null 2>&1
}

for attempt in {1..10}; do
    if authorized org.freedesktop.login1.reboot &&
       authorized org.freedesktop.login1.power-off &&
       authorized org.freedesktop.timedate1.set-timezone &&
       authorized org.freedesktop.systemd1.manage-units \
           --detail unit systemd-timesyncd.service --detail verb restart &&
       authorized org.freedesktop.systemd1.manage-units \
           --detail unit optic-daemon.service --detail verb restart; then
        break
    fi
    ((attempt < 10)) || {
        printf 'PolicyKit does not authorize every daemon action for %s.\n' "$DAEMON_USER" >&2
        exit 1
    }
    sleep 1
done

# pkcheck exits 1 (not authorized) or 2 (authentication required) for a
# denial; anything else is an error, not proof that the rule is scoped.
scope_status=0
authorized org.freedesktop.systemd1.manage-units \
    --detail unit ssh.service --detail verb restart || scope_status=$?
if ((scope_status == 0)); then
    printf 'The NTP rule is too broad: %s may restart ssh.service without authentication.\n' \
        "$DAEMON_USER" >&2
    exit 1
elif ((scope_status != 1 && scope_status != 2)); then
    printf 'Could not check the NTP rule scope (pkcheck exit status %s).\n' "$scope_status" >&2
    exit 1
fi

# Rule 64 must not reach past start/stop/restart of optic-daemon.service:
# "reload" of the same unit is an unlisted verb and must still be refused.
scope_status=0
authorized org.freedesktop.systemd1.manage-units \
    --detail unit optic-daemon.service --detail verb reload || scope_status=$?
if ((scope_status == 0)); then
    printf 'The optic-daemon unit rule is too broad: %s may reload it without authentication.\n' \
        "$DAEMON_USER" >&2
    exit 1
elif ((scope_status != 1 && scope_status != 2)); then
    printf 'Could not check the optic-daemon unit rule scope (pkcheck exit status %s).\n' "$scope_status" >&2
    exit 1
fi

[[ -e "$LINGER_FILE" ]]
for group in "${REQUIRED_GROUPS[@]}"; do
    user_in_group "$group"
done

if ((GROUPS_CHANGED)); then
    printf '%s\n' "New group membership applies to $DAEMON_USER's next login and user manager start; reboot (with approval) before relying on it."
fi
printf '%s\n' 'Phase 8 daemon host access setup complete: Reboot, shutdown, NTP sync, timezone, and optic-daemon restart actions are authorized.'
