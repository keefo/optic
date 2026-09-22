//! External heartbeat: an ntfy dead-man's switch
//! (`docs/optic-daemon-digest-heartbeat.md` §4).
//!
//! Every interval, while the station is healthy, the alerts actor publishes
//! a *scheduled* ntfy message ("no check-in since …") with a fixed sequence
//! ID. Each publish replaces the pending one on the ntfy server, so the
//! message is only delivered if the Pi stops re-arming: the alert comes from
//! outside the Pi. Pure half only: gating, timing, and the curl requests;
//! the actor in `optic_alerts` sends them.

use chrono::{DateTime, Duration, Utc};

use crate::optic_alerts::{NtfyConfig, curl_escape};

pub const DEFAULT_SEQUENCE_ID: &str = "optic-heartbeat";
pub const DEFAULT_INTERVAL_SECS: u64 = 600;
pub const DEFAULT_ALERT_AFTER_SECS: u64 = 1800;
pub const MIN_INTERVAL_SECS: u64 = 60;
pub const MAX_INTERVAL_SECS: u64 = 3600;
pub const MIN_ALERT_AFTER_SECS: u64 = 120;
/// ntfy's maximum scheduled-delivery delay.
pub const MAX_ALERT_AFTER_SECS: u64 = 3 * 24 * 3600;
/// `alert_after` must exceed the interval by at least this, so one late
/// tick or a daemon restart doesn't false-alarm.
pub const MIN_ALERT_MARGIN_SECS: u64 = 300;

/// Validated heartbeat settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatPlan {
    pub interval: Duration,
    pub alert_after: Duration,
    pub sequence_id: String,
}

/// The ntfy message a check-in armed: where it lives and how to reach it.
/// Compared to decide whether a settings change orphans it.
#[derive(Clone, PartialEq, Eq)]
pub struct ArmedOn {
    pub server: String,
    pub topic: String,
    pub token: Option<String>,
    pub sequence_id: String,
}

impl std::fmt::Debug for ArmedOn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArmedOn")
            .field("server", &self.server)
            .field("topic", &"<redacted>")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("sequence_id", &self.sequence_id)
            .finish()
    }
}

impl ArmedOn {
    pub fn new(ntfy: &NtfyConfig, sequence_id: &str) -> Self {
        Self {
            server: ntfy.server.clone(),
            topic: ntfy.topic.clone(),
            token: ntfy.token.clone(),
            sequence_id: sequence_id.to_owned(),
        }
    }

    fn ntfy(&self) -> NtfyConfig {
        NtfyConfig {
            server: self.server.clone(),
            topic: self.topic.clone(),
            token: self.token.clone(),
        }
    }
}

pub fn valid_sequence_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether the daemon may check in. `withheld_by` are the titles of
/// capture-path conditions currently firing or recovering.
pub fn gate(withheld_by: &[&str], camera_detected: bool) -> Result<(), String> {
    if !camera_detected {
        return Err("camera not detected at startup".to_owned());
    }
    if !withheld_by.is_empty() {
        return Err(format!("active alert: {}", withheld_by.join(", ")));
    }
    Ok(())
}

/// A check-in is due when there has been none, or the last one is at least
/// one interval old. A failed publish leaves `last_checkin` unchanged, so
/// it is retried on the next tick.
pub fn is_due(
    last_checkin: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    plan: &HeartbeatPlan,
) -> bool {
    last_checkin.is_none_or(|at| now - at >= plan.interval)
}

/// Whether an armed message is still pending on the server (it would be
/// delivered at `last_checkin + alert_after`).
pub fn is_armed(
    last_checkin: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    alert_after: Duration,
) -> bool {
    last_checkin.is_some_and(|at| now - at < alert_after)
}

/// When a check-in succeeds more than `alert_after` after the previous one,
/// the silent alert has almost certainly been delivered: returns the silent
/// period, for a "checking in again" notice.
pub fn silent_period(
    previous: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    alert_after: Duration,
) -> Option<Duration> {
    previous.map(|at| now - at).filter(|gap| *gap > alert_after)
}

/// A settings change orphans the armed message when the new settings would
/// not replace it: heartbeat or ntfy off (`new` = `None`), or a different
/// server, topic, token or sequence ID.
pub fn cancel_needed(armed: Option<&ArmedOn>, new: Option<&ArmedOn>) -> bool {
    armed.is_some_and(|armed| Some(armed) != new)
}

/// ntfy delays are sent in whole minutes, rounded up.
pub fn delay_minutes(alert_after: Duration) -> i64 {
    (alert_after.num_seconds() + 59) / 60
}

/// Header values must be printable ASCII on one line.
fn ascii_header(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn auth_and_url(config: &mut String, ntfy: &NtfyConfig, sequence_id: &str) {
    config.push_str(&format!(
        "url = \"{}/{}/{}\"\n",
        curl_escape(&ntfy.server),
        curl_escape(&ntfy.topic),
        curl_escape(sequence_id)
    ));
    if let Some(token) = &ntfy.token {
        config.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            curl_escape(token)
        ));
    }
}

/// Curl config (fed on stdin) that arms or re-arms the scheduled message.
pub fn arm_curl_config(
    ntfy: &NtfyConfig,
    plan: &HeartbeatPlan,
    title: &str,
    message: &str,
) -> String {
    let mut config = String::new();
    auth_and_url(&mut config, ntfy, &plan.sequence_id);
    config.push_str(&format!(
        "header = \"X-Delay: {}m\"\n",
        delay_minutes(plan.alert_after)
    ));
    config.push_str(&format!(
        "header = \"X-Title: {}\"\n",
        curl_escape(&ascii_header(title))
    ));
    config.push_str("header = \"X-Priority: 5\"\n");
    config.push_str("header = \"X-Tags: rotating_light\"\n");
    config.push_str("header = \"Content-Type: text/plain; charset=utf-8\"\n");
    // `data-raw`: never interpret a leading '@' as a file name.
    config.push_str(&format!("data-raw = \"{}\"\n", curl_escape(message)));
    config
}

/// Curl config that cancels the scheduled message armed on `armed`.
pub fn cancel_curl_config(armed: &ArmedOn) -> String {
    let mut config = String::from("request = \"DELETE\"\n");
    auth_and_url(&mut config, &armed.ntfy(), &armed.sequence_id);
    config
}

/// Title and body of the message that is delivered if the Pi goes silent.
pub fn silent_alert(
    station_name: Option<&str>,
    alert_after: Duration,
    last_checkin_local: &str,
) -> (String, String) {
    let station = station_name
        .map(|name| format!(" {name}"))
        .unwrap_or_default();
    (
        format!(
            "Optic{station}: no check-in for {}m",
            delay_minutes(alert_after)
        ),
        format!(
            "Optic{station} last checked in at {last_checkin_local}. The Pi, the daemon or its network is down, or the daemon withheld its check-in because scheduled captures are failing (see earlier alerts)."
        ),
    )
}

/// Title and body of the "checking in again" notice.
pub fn back_notice(
    station_name: Option<&str>,
    silent_from_local: &str,
    now_local: &str,
    silent_for: &str,
) -> (String, String) {
    let station = station_name
        .map(|name| format!(" {name}"))
        .unwrap_or_default();
    (
        format!("Optic{station}: checking in again"),
        format!(
            "No check-in from {silent_from_local} to {now_local} ({silent_for}). The heartbeat is running again."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 21, h, m, 0).unwrap()
    }

    fn plan() -> HeartbeatPlan {
        HeartbeatPlan {
            interval: Duration::minutes(10),
            alert_after: Duration::minutes(30),
            sequence_id: DEFAULT_SEQUENCE_ID.to_owned(),
        }
    }

    fn ntfy(token: Option<&str>) -> NtfyConfig {
        NtfyConfig {
            server: "https://ntfy.example".to_owned(),
            topic: "optic-secret".to_owned(),
            token: token.map(str::to_owned),
        }
    }

    #[test]
    fn gate_withholds_for_capture_alerts_and_missing_camera() {
        assert_eq!(gate(&[], true), Ok(()));
        assert_eq!(
            gate(&["captures stalled", "captures failing"], true),
            Err("active alert: captures stalled, captures failing".to_owned())
        );
        assert_eq!(
            gate(&[], false),
            Err("camera not detected at startup".to_owned())
        );
    }

    #[test]
    fn due_every_interval_and_retried_after_a_failure() {
        assert!(is_due(None, t(10, 0), &plan()));
        assert!(!is_due(Some(t(10, 0)), t(10, 9), &plan()));
        assert!(is_due(Some(t(10, 0)), t(10, 10), &plan()));
        // A failed publish at 10:10 leaves last_checkin at 10:00: still due.
        assert!(is_due(
            Some(t(10, 0)),
            t(10, 10) + Duration::seconds(30),
            &plan()
        ));
    }

    #[test]
    fn armed_and_silent_period_follow_alert_after() {
        let after = plan().alert_after;
        assert!(is_armed(Some(t(10, 0)), t(10, 29), after));
        assert!(!is_armed(Some(t(10, 0)), t(10, 30), after));
        assert!(!is_armed(None, t(10, 0), after));
        assert_eq!(silent_period(Some(t(10, 0)), t(10, 20), after), None);
        assert_eq!(silent_period(Some(t(10, 0)), t(10, 30), after), None);
        assert_eq!(
            silent_period(Some(t(10, 0)), t(11, 7), after),
            Some(Duration::minutes(67))
        );
        assert_eq!(silent_period(None, t(11, 7), after), None);
    }

    #[test]
    fn cancel_needed_only_when_the_armed_message_would_be_orphaned() {
        let armed = ArmedOn::new(&ntfy(None), DEFAULT_SEQUENCE_ID);
        assert!(!cancel_needed(None, Some(&armed)));
        assert!(!cancel_needed(Some(&armed), Some(&armed.clone())));
        assert!(cancel_needed(Some(&armed), None));
        let mut other_topic = armed.clone();
        other_topic.topic = "optic-new".to_owned();
        assert!(cancel_needed(Some(&armed), Some(&other_topic)));
        let new_token = ArmedOn::new(&ntfy(Some("tk_x")), DEFAULT_SEQUENCE_ID);
        assert!(cancel_needed(Some(&armed), Some(&new_token)));
        let new_seq = ArmedOn::new(&ntfy(None), "other");
        assert!(cancel_needed(Some(&armed), Some(&new_seq)));
    }

    #[test]
    fn arm_request_schedules_on_the_sequence_id_with_ascii_headers() {
        let (title, message) =
            silent_alert(Some("optïc \"roof\""), plan().alert_after, "14:05 PDT");
        let config = arm_curl_config(&ntfy(Some("tk_abc")), &plan(), &title, &message);
        assert!(config.contains("url = \"https://ntfy.example/optic-secret/optic-heartbeat\"\n"));
        assert!(config.contains("header = \"X-Delay: 30m\"\n"));
        assert!(
            config
                .contains("header = \"X-Title: Optic opt?c \\\"roof\\\": no check-in for 30m\"\n")
        );
        assert!(config.contains("header = \"X-Priority: 5\"\n"));
        assert!(config.contains("header = \"Authorization: Bearer tk_abc\"\n"));
        assert!(
            config.contains("data-raw = \"Optic optïc \\\"roof\\\" last checked in at 14:05 PDT.")
        );
        let without_token = arm_curl_config(&ntfy(None), &plan(), &title, &message);
        assert!(!without_token.contains("Authorization"));
    }

    #[test]
    fn delay_rounds_up_to_whole_minutes() {
        assert_eq!(delay_minutes(Duration::seconds(1800)), 30);
        assert_eq!(delay_minutes(Duration::seconds(1801)), 31);
        assert_eq!(delay_minutes(Duration::seconds(120)), 2);
    }

    #[test]
    fn cancel_request_deletes_with_the_old_credentials() {
        let armed = ArmedOn::new(&ntfy(Some("tk_old")), "hb");
        let config = cancel_curl_config(&armed);
        assert!(config.starts_with("request = \"DELETE\"\n"));
        assert!(config.contains("url = \"https://ntfy.example/optic-secret/hb\"\n"));
        assert!(config.contains("Authorization: Bearer tk_old"));
        assert!(!config.contains("data-raw"));
        assert!(!format!("{armed:?}").contains("tk_old"));
        assert!(!format!("{armed:?}").contains("optic-secret"));
    }

    #[test]
    fn sequence_id_validation() {
        assert!(valid_sequence_id("optic-heartbeat_2"));
        for bad in ["", "has space", "slash/", &"x".repeat(65)] {
            assert!(!valid_sequence_id(bad), "{bad}");
        }
    }

    #[test]
    fn back_notice_names_the_silent_period() {
        let (title, message) = back_notice(Some("optic"), "14:05", "15:12", "1h 7m");
        assert_eq!(title, "Optic optic: checking in again");
        assert!(message.starts_with("No check-in from 14:05 to 15:12 (1h 7m)."));
    }
}
