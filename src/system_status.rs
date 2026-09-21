//! Read-only Pi/host health snapshot for the dashboard's system panel —
//! memory, disk usage, uptime, CPU temperature, and network interfaces.
//! See `worklogs/2026-09-18-system-control-panel.md` for the design
//! decisions (in particular: served from a dedicated, independently-polled
//! endpoint rather than folded into the existing hot `/api/status` path,
//! since CPU temperature requires spawning `vcgencmd`).
//!
//! Deliberately *not* platform-gated like `native_camera`/`native_codec` —
//! `sysinfo`/`if-addrs` are cross-platform, so this module builds and is
//! unit-tested on the macOS dev target too. Only CPU temperature
//! (`vcgencmd` is Raspberry-Pi-specific) is Linux-only, returning `None`
//! elsewhere.
//!
//! `sysinfo`'s `linux-tmpfs` Cargo feature (enabled in `Cargo.toml`) is
//! required for `/mnt/capture` to show up in disk listings at all —
//! `sysinfo` excludes tmpfs mounts by default on Linux, and this project's
//! capture staging directory is specifically a tmpfs. Found live
//! (2026-09-18): without it, the "capture" disk entry silently fell back
//! to reporting root's figures instead, which is actively misleading for
//! the tmpfs-fill risk this panel exists to surface.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::Serialize;
use sysinfo::Disks;
use tokio::process::Command;

#[derive(Debug, Serialize)]
pub struct SystemStatus {
    pub memory: MemoryStatus,
    pub disks: Vec<DiskStatus>,
    pub uptime_seconds: u64,
    pub cpu_temp_celsius: Option<f32>,
    pub network_interfaces: Vec<NetworkInterfaceStatus>,
    pub time_sync: Option<TimeSyncStatus>,
    /// The system clock's current reading — a plain `chrono::Utc::now()`,
    /// not sourced from `timedatectl` and not gated behind `time_sync`
    /// being `Some` (unlike the NTP/timezone fields, "what time is it"
    /// needs no platform-specific tool and is always available, including
    /// on non-Linux dev targets). The footer formats this in the
    /// system's own timezone (`time_sync.timezone` when present) rather
    /// than the viewer's browser timezone, since the whole point is
    /// showing what the *Pi* thinks the time is.
    pub now: chrono::DateTime<chrono::Utc>,
}

/// `timedatectl show`'s view of the system clock, for the Config page's
/// Time & NTP section. `None` on a platform without `timedatectl`
/// (non-Linux dev targets), same posture as `cpu_temp_celsius`.
#[derive(Debug, Serialize)]
pub struct TimeSyncStatus {
    pub timezone: String,
    pub ntp_enabled: bool,
    pub synchronized: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryStatus {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct DiskStatus {
    pub label: &'static str,
    pub path: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct NetworkInterfaceStatus {
    pub name: String,
    pub addresses: Vec<String>,
}

/// The paths whose filesystem usage the dashboard cares about: the root
/// filesystem, the capture staging tmpfs, and the capture-history state
/// directory (each can fill up independently and each matters for a
/// year-long unattended deployment).
#[derive(Clone)]
pub struct WatchedPaths {
    pub capture_dir: PathBuf,
    pub state_dir: PathBuf,
}

#[derive(Clone)]
pub struct SystemStatusReader {
    inner: Arc<Mutex<Disks>>,
    watched: WatchedPaths,
}

impl SystemStatusReader {
    pub fn new(watched: WatchedPaths) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Disks::new())),
            watched,
        }
    }

    pub async fn snapshot(&self) -> SystemStatus {
        let disks = self.inner.clone();
        let watched = self.watched.clone();
        let (memory, disk_statuses, uptime_seconds) =
            tokio::task::spawn_blocking(move || Self::blocking_snapshot(&disks, &watched))
                .await
                .unwrap_or_else(|_| (MemoryStatus::default(), Vec::new(), 0));

        SystemStatus {
            memory,
            disks: disk_statuses,
            uptime_seconds,
            cpu_temp_celsius: cpu_temp_celsius().await,
            network_interfaces: tokio::task::spawn_blocking(network_interfaces)
                .await
                .unwrap_or_default(),
            time_sync: time_sync_status().await,
            now: chrono::Utc::now(),
        }
    }

    fn blocking_snapshot(
        disks: &Mutex<Disks>,
        watched: &WatchedPaths,
    ) -> (MemoryStatus, Vec<DiskStatus>, u64) {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let memory = MemoryStatus {
            total_bytes: system.total_memory(),
            available_bytes: system.available_memory(),
        };

        let mut disks = disks.lock().expect("system status disks mutex poisoned");
        // `refresh()` only updates already-tracked entries and does
        // nothing on an empty list (per its own docs); `refresh_list()`
        // is the one that actually (re-)enumerates mounted filesystems.
        // Confirmed live on the Pi (2026-09-18): using `refresh()` here
        // silently produced an empty disk list on every request.
        disks.refresh_list();
        let candidates: Vec<(&Path, u64, u64)> = disks
            .list()
            .iter()
            .map(|disk| {
                (
                    disk.mount_point(),
                    disk.total_space(),
                    disk.available_space(),
                )
            })
            .collect();

        let targets: [(&'static str, &Path); 3] = [
            ("root", Path::new("/")),
            ("capture", &watched.capture_dir),
            ("state", &watched.state_dir),
        ];
        let disk_statuses = targets
            .into_iter()
            .filter_map(|(label, path)| {
                best_matching_disk(candidates.iter().copied(), path).map(|(total, available)| {
                    DiskStatus {
                        label,
                        path: path.display().to_string(),
                        total_bytes: total,
                        available_bytes: available,
                    }
                })
            })
            .collect();

        (memory, disk_statuses, sysinfo::System::uptime())
    }
}

/// Picks the candidate mount point that is the longest matching prefix of
/// `target` — the standard "which filesystem is this path actually on"
/// resolution, needed because e.g. both `/` and `/mnt/capture` are always
/// candidates and a naive "any prefix match" would pick arbitrarily.
fn best_matching_disk<'a>(
    candidates: impl Iterator<Item = (&'a Path, u64, u64)>,
    target: &Path,
) -> Option<(u64, u64)> {
    candidates
        .filter(|(mount_point, _, _)| target.starts_with(mount_point))
        .max_by_key(|(mount_point, _, _)| mount_point.as_os_str().len())
        .map(|(_, total, available)| (total, available))
}

fn network_interfaces() -> Vec<NetworkInterfaceStatus> {
    let mut by_name: Vec<(String, Vec<String>)> = Vec::new();
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    for interface in interfaces {
        if interface.is_loopback() {
            continue;
        }
        let address = interface.ip().to_string();
        match by_name.iter_mut().find(|(name, _)| *name == interface.name) {
            Some((_, addresses)) => addresses.push(address),
            None => by_name.push((interface.name, vec![address])),
        }
    }
    by_name
        .into_iter()
        .map(|(name, addresses)| NetworkInterfaceStatus { name, addresses })
        .collect()
}

#[cfg(target_os = "linux")]
async fn cpu_temp_celsius() -> Option<f32> {
    let output = Command::new("vcgencmd")
        .arg("measure_temp")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_vcgencmd_temp(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(target_os = "linux"))]
async fn cpu_temp_celsius() -> Option<f32> {
    None
}

/// Parses `vcgencmd measure_temp`'s `temp=42.2'C\n` output.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_vcgencmd_temp(text: &str) -> Option<f32> {
    text.trim()
        .strip_prefix("temp=")?
        .split('\'')
        .next()?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
async fn time_sync_status() -> Option<TimeSyncStatus> {
    let output = Command::new("timedatectl")
        .args([
            "show",
            "-p",
            "Timezone",
            "-p",
            "NTP",
            "-p",
            "NTPSynchronized",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_timedatectl_show(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(target_os = "linux"))]
async fn time_sync_status() -> Option<TimeSyncStatus> {
    None
}

/// Parses `timedatectl show`'s `Key=Value`-per-line output (not the
/// `--value`-only form, which drops the keys and relies on request
/// ordering — the safer format to parse even though it's marginally more
/// code). Real captured output looks like:
/// ```text
/// Timezone=America/Vancouver
/// NTP=yes
/// NTPSynchronized=yes
/// ```
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_timedatectl_show(text: &str) -> Option<TimeSyncStatus> {
    let mut timezone = None;
    let mut ntp_enabled = None;
    let mut synchronized = None;
    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        match key {
            "Timezone" => timezone = Some(value.to_owned()),
            "NTP" => ntp_enabled = Some(value == "yes"),
            "NTPSynchronized" => synchronized = Some(value == "yes"),
            _ => {}
        }
    }
    Some(TimeSyncStatus {
        timezone: timezone?,
        ntp_enabled: ntp_enabled?,
        synchronized: synchronized?,
    })
}

/// Reboots the host via `systemctl reboot`, which talks to systemd/logind
/// over D-Bus (no sudo, no setuid). Requires the scoped PolicyKit rule
/// granting the invoking user `org.freedesktop.login1.reboot` without
/// interactive auth — see `worklogs/2026-09-19-reboot-nonewprivileges-fix.md`.
/// (An earlier `sudo systemctl reboot` version could never work from this
/// sandboxed service: `ProtectSystem=strict`/`ProtectHome=read-only` force a
/// private user namespace that only maps this service's own UID, so `sudo`
/// sees `/usr/bin/sudo` as owned by the unmapped-UID placeholder instead of
/// root and refuses to run. D-Bus authorization uses real kernel-level
/// credentials instead of a namespace-relative file-ownership check, so it
/// isn't affected.)
pub async fn reboot_host() -> std::io::Result<()> {
    let status = Command::new("systemctl").arg("reboot").status().await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "reboot command exited with {status}"
        )))
    }
}

/// Forces an immediate NTP resync via `systemctl restart
/// systemd-timesyncd.service` — the only real lever available:
/// `systemd-timesyncd`'s own D-Bus interface
/// (`org.freedesktop.timesync1.Manager`) exposes `SetRuntimeNTPServers`
/// but no "resync now" method. Restarting a unit needs the broad
/// `org.freedesktop.systemd1.manage-units` PolicyKit action, which (like
/// `reboot_host`'s `org.freedesktop.login1.reboot`) the sandboxed daemon
/// can't authenticate via `sudo` — same private-user-namespace reason
/// documented on `reboot_host`. Rather than grant that broad action
/// wholesale, the installed PolicyKit rule
/// (`/etc/polkit-1/rules.d/61-optic-daemon-ntp-sync.rules`, host config,
/// outside this repo) is scoped to exactly this one unit + the `restart`
/// verb — confirmed live before this function was written (both that it
/// authorizes this exact call and that it correctly denies a restart of
/// any other unit), see `worklogs/2026-09-20-scheduler-phase2i-config-page-station-ntp-timezone.md`.
pub async fn sync_ntp_now() -> std::io::Result<()> {
    let status = Command::new("systemctl")
        .arg("restart")
        .arg("systemd-timesyncd.service")
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "systemd-timesyncd restart exited with {status}"
        )))
    }
}

/// Sets the system timezone via `timedatectl set-timezone`, which talks
/// to `org.freedesktop.timedate1.Manager.SetTimezone` over D-Bus (no
/// sudo). Unlike `sync_ntp_now`, this has its own narrowly-scoped
/// PolicyKit action already (`org.freedesktop.timedate1.set-timezone`) —
/// no need to grant the broad `manage-units` action and detail-match a
/// unit name. Requires the installed PolicyKit rule
/// (`/etc/polkit-1/rules.d/62-optic-daemon-set-timezone.rules`, host
/// config, outside this repo) granting the daemon's user that one action
/// — confirmed live before this function was written, see
/// `worklogs/2026-09-20-scheduler-phase2i-config-page-station-ntp-timezone.md`.
/// Called from the Config page's "Save station" flow (user's explicit
/// choice — keep the Station's timezone and the Pi's system clock in
/// sync, rather than treating them as independent), not from every
/// config commit.
pub async fn set_system_timezone(timezone: &str) -> std::io::Result<()> {
    let status = Command::new("timedatectl")
        .arg("set-timezone")
        .arg(timezone)
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "timedatectl set-timezone exited with {status}"
        )))
    }
}

/// Restarts `optic-daemon.service` (no sudo needed — it's the invoking
/// user's own systemd user service). The restart is spawned detached with
/// a short delay so the HTTP response triggering it can be flushed to the
/// client before this process is killed by its own restart.
pub fn restart_daemon_detached() {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = Command::new("systemctl")
            .arg("--user")
            .arg("restart")
            .arg("optic-daemon.service")
            .status()
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_matching_disk_picks_longest_prefix_match() {
        let candidates = vec![
            (Path::new("/"), 100_u64, 50_u64),
            (Path::new("/mnt/capture"), 200_u64, 190_u64),
        ];
        let result = best_matching_disk(candidates.into_iter(), Path::new("/mnt/capture"));
        assert_eq!(result, Some((200, 190)));
    }

    #[test]
    fn best_matching_disk_falls_back_to_root_for_unrelated_paths() {
        let candidates = vec![
            (Path::new("/"), 100_u64, 50_u64),
            (Path::new("/mnt/capture"), 200_u64, 190_u64),
        ];
        let result = best_matching_disk(
            candidates.into_iter(),
            Path::new("/home/liam/.local/state/optic-daemon"),
        );
        assert_eq!(result, Some((100, 50)));
    }

    #[test]
    fn best_matching_disk_returns_none_when_nothing_matches() {
        let candidates = vec![(Path::new("/mnt/capture"), 200_u64, 190_u64)];
        let result = best_matching_disk(candidates.into_iter(), Path::new("/var/log"));
        assert_eq!(result, None);
    }

    #[test]
    fn vcgencmd_temp_parses_real_output_format() {
        assert_eq!(parse_vcgencmd_temp("temp=42.2'C\n"), Some(42.2));
        assert_eq!(parse_vcgencmd_temp("garbage"), None);
    }

    #[test]
    fn timedatectl_show_parses_real_output_format() {
        let status =
            parse_timedatectl_show("Timezone=America/Vancouver\nNTP=yes\nNTPSynchronized=yes\n")
                .unwrap();
        assert_eq!(status.timezone, "America/Vancouver");
        assert!(status.ntp_enabled);
        assert!(status.synchronized);
    }

    #[test]
    fn timedatectl_show_reflects_ntp_disabled_and_unsynchronized() {
        let status = parse_timedatectl_show("Timezone=UTC\nNTP=no\nNTPSynchronized=no\n").unwrap();
        assert!(!status.ntp_enabled);
        assert!(!status.synchronized);
    }

    #[test]
    fn timedatectl_show_rejects_output_missing_expected_keys() {
        assert!(parse_timedatectl_show("garbage").is_none());
        assert!(parse_timedatectl_show("Timezone=UTC\n").is_none());
    }

    #[tokio::test]
    async fn cpu_temp_is_none_on_non_linux_dev_targets() {
        // This test only asserts the behavior actually exercised on the
        // macOS dev machine this runs on; the Linux/vcgencmd path is
        // validated on the Pi-native target instead (see the worklog).
        if !cfg!(target_os = "linux") {
            assert_eq!(cpu_temp_celsius().await, None);
        }
    }

    #[tokio::test]
    async fn snapshot_returns_sane_memory_and_disk_figures() {
        let reader = SystemStatusReader::new(WatchedPaths {
            capture_dir: std::env::temp_dir(),
            state_dir: std::env::temp_dir(),
        });
        let status = reader.snapshot().await;
        assert!(status.memory.total_bytes > 0);
        assert!(status.memory.available_bytes <= status.memory.total_bytes);
        // Regression check: `Disks::refresh()` silently no-ops on an empty
        // list (only `refresh_list()` actually enumerates filesystems) —
        // a bug that shipped past this test once already because the loop
        // below asserted properties *of* each disk without first asserting
        // any disks were found at all, so it passed vacuously against an
        // always-empty list. `/` must exist on every Unix machine this
        // runs on, dev or Pi.
        assert!(
            !status.disks.is_empty(),
            "expected at least the root filesystem to be listed"
        );
        for disk in &status.disks {
            assert!(disk.total_bytes > 0, "disk {} has zero total", disk.label);
            assert!(
                disk.available_bytes <= disk.total_bytes,
                "disk {} available exceeds total",
                disk.label
            );
        }
        // Sanity check on this dev machine (unsandboxed): some real,
        // non-loopback interface should be found. Doesn't cover the Pi's
        // systemd `RestrictAddressFamilies` sandboxing (AF_NETLINK is
        // needed there for `getifaddrs()`/`if-addrs` to work at all) —
        // that needs its own empirical check against the deployed daemon,
        // this only guards the enumeration logic itself.
        assert!(
            !status.network_interfaces.is_empty(),
            "expected at least one non-loopback network interface on the dev machine"
        );
        // `now` is a plain `Utc::now()` read, not derived from anything
        // else in the snapshot — a regression that left it at a fixed/
        // default value would otherwise pass every other assertion here.
        assert!((chrono::Utc::now() - status.now).num_seconds().abs() < 5);
    }
}
