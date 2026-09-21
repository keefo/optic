//! Capture filename parsing (`docs/timelapse-builder.md` §4).
//!
//! Mirrors the daemon's naming: `testshot-<profile>-<ms>` for web-UI
//! captures, `scheduler-<profile>[-<tags>]-<ms>` for scheduler captures,
//! and `<prefix>-<profile>-failed-<ms>.log.json` for failed captures
//! (`src/native_camera.rs`, `src/optic_capture_log.rs`, and
//! `src/camera.rs::format_rule_tags` in the daemon crate).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Profile {
    MasterArchive,
    Dci4k,
    Binning2k,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::MasterArchive, Profile::Dci4k, Profile::Binning2k];

    /// Filename slug, as written by the daemon (`CaptureProfile::spec().slug`).
    pub fn slug(self) -> &'static str {
        match self {
            Profile::MasterArchive => "master-archive",
            Profile::Dci4k => "4k-dci",
            Profile::Binning2k => "2k-binning",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Profile> {
        Profile::ALL.into_iter().find(|p| p.slug() == slug)
    }

    /// Still-capture resolution per profile (README "Camera Daemon"). Used
    /// only when no sidecar reports the real size.
    pub fn native_size(self) -> (u32, u32) {
        match self {
            Profile::MasterArchive => (4056, 3040),
            Profile::Dci4k => (4056, 2160),
            Profile::Binning2k => (2028, 1520),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Scheduler,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Jpg,
    Dng,
    LogJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureName {
    pub source: Source,
    pub profile: Profile,
    /// Raw rule-tag string from a scheduler filename, `+N` overflow removed.
    /// Hyphen-joined, so it can't be split into slugs on its own.
    pub tags: Option<String>,
    /// The `N` of a `+N` overflow marker (0 when absent).
    pub tag_overflow: u32,
    pub failed: bool,
    pub millis: u64,
    pub kind: Kind,
}

/// Parses a capture filename, or returns `None` for anything that isn't
/// one (config files, `.DS_Store`, unknown profiles, malformed suffixes).
pub fn parse_capture_filename(name: &str) -> Option<CaptureName> {
    let (stem, kind) = [
        (".log.json", Kind::LogJson),
        (".jpg", Kind::Jpg),
        (".dng", Kind::Dng),
    ]
    .into_iter()
    .find_map(|(ext, kind)| Some((name.strip_suffix(ext)?, kind)))?;

    let (source, rest) = [
        ("scheduler-", Source::Scheduler),
        ("testshot-", Source::Manual),
    ]
    .into_iter()
    .find_map(|(prefix, source)| Some((source, stem.strip_prefix(prefix)?)))?;

    let (profile, rest) = Profile::ALL.into_iter().find_map(|profile| {
        let rest = rest.strip_prefix(profile.slug())?.strip_prefix('-')?;
        Some((profile, rest))
    })?;

    let (middle, millis) = match rest.rsplit_once('-') {
        Some((middle, millis)) => (Some(middle), millis),
        None => (None, rest),
    };
    if millis.is_empty() || !millis.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis = millis.parse().ok()?;

    let mut parsed = CaptureName {
        source,
        profile,
        tags: None,
        tag_overflow: 0,
        failed: false,
        millis,
        kind,
    };
    match middle {
        None => {}
        Some("failed") if kind == Kind::LogJson => parsed.failed = true,
        Some(_) if source == Source::Manual => return None,
        Some(tags) => {
            let (tags, overflow) = split_overflow(tags)?;
            if !is_valid_tag_string(tags) {
                return None;
            }
            parsed.tags = Some(tags.to_owned());
            parsed.tag_overflow = overflow;
        }
    }
    Some(parsed)
}

/// Splits `a-b-c+2` into (`a-b-c`, 2). A string without `+` has overflow 0.
fn split_overflow(tags: &str) -> Option<(&str, u32)> {
    match tags.rsplit_once('+') {
        None => Some((tags, 0)),
        Some((tags, count)) => {
            if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let count: u32 = count.parse().ok()?;
            (count > 0).then_some((tags, count))
        }
    }
}

/// Slugs match `^[a-z0-9][a-z0-9-]*$` (scheduler design §3.1), so a joined
/// tag string is non-empty, starts with a letter or digit, and uses only
/// that alphabet.
fn is_valid_tag_string(tags: &str) -> bool {
    !tags.is_empty()
        && !tags.starts_with('-')
        && tags
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_shaped_scheduler_jpg() {
        let parsed =
            parse_capture_filename("scheduler-master-archive-every1min-1789894386246.jpg").unwrap();
        assert_eq!(
            parsed,
            CaptureName {
                source: Source::Scheduler,
                profile: Profile::MasterArchive,
                tags: Some("every1min".into()),
                tag_overflow: 0,
                failed: false,
                millis: 1789894386246,
                kind: Kind::Jpg,
            }
        );
    }

    #[test]
    fn parses_every_profile_slug_including_hyphenated_ones() {
        for profile in Profile::ALL {
            let name = format!("scheduler-{}-dawn-42.jpg", profile.slug());
            let parsed = parse_capture_filename(&name).unwrap();
            assert_eq!(parsed.profile, profile, "{name}");
            assert_eq!(parsed.tags.as_deref(), Some("dawn"), "{name}");
            assert_eq!(parsed.millis, 42);
        }
    }

    #[test]
    fn scheduler_name_without_tags_has_none() {
        let parsed = parse_capture_filename("scheduler-4k-dci-1000.jpg").unwrap();
        assert_eq!(parsed.tags, None);
        assert_eq!(parsed.millis, 1000);
    }

    #[test]
    fn keeps_a_multi_hyphen_tag_string_whole() {
        let parsed = parse_capture_filename("scheduler-2k-binning-dawn-golden-hour-7.jpg").unwrap();
        assert_eq!(parsed.profile, Profile::Binning2k);
        assert_eq!(parsed.tags.as_deref(), Some("dawn-golden-hour"));
    }

    #[test]
    fn strips_a_plus_n_overflow_marker() {
        let parsed = parse_capture_filename("scheduler-master-archive-a-b-c+2-9.jpg").unwrap();
        assert_eq!(parsed.tags.as_deref(), Some("a-b-c"));
        assert_eq!(parsed.tag_overflow, 2);
    }

    #[test]
    fn parses_manual_and_failed_names() {
        let manual = parse_capture_filename("testshot-4k-dci-1789898505999.jpg").unwrap();
        assert_eq!(manual.source, Source::Manual);
        assert_eq!(manual.tags, None);

        let failed =
            parse_capture_filename("testshot-2k-binning-failed-1789925144094.log.json").unwrap();
        assert!(failed.failed);
        assert_eq!(failed.kind, Kind::LogJson);

        let sched_failed =
            parse_capture_filename("scheduler-master-archive-failed-5.log.json").unwrap();
        assert!(sched_failed.failed);
        assert_eq!(sched_failed.source, Source::Scheduler);
    }

    #[test]
    fn recognizes_dng_and_log_json_kinds() {
        assert_eq!(
            parse_capture_filename("testshot-master-archive-1.dng")
                .unwrap()
                .kind,
            Kind::Dng
        );
        assert_eq!(
            parse_capture_filename("scheduler-master-archive-x-1.log.json")
                .unwrap()
                .kind,
            Kind::LogJson
        );
    }

    #[test]
    fn a_rule_slugged_failed_on_a_jpg_is_a_tag_not_a_failure() {
        let parsed = parse_capture_filename("scheduler-master-archive-failed-5.jpg").unwrap();
        assert!(!parsed.failed);
        assert_eq!(parsed.tags.as_deref(), Some("failed"));
    }

    #[test]
    fn rejects_non_capture_names() {
        for name in [
            "config.json",
            "preview_config.json",
            ".DS_Store",
            "optic-web-master-archive-1789713405041.dng",
            "scheduler-unknown-profile-1.jpg",
            "scheduler-master-archive-abc.jpg",
            "scheduler-master-archive-.jpg",
            "scheduler-master-archive-1.JPG",
            "scheduler-master-archive--1.jpg",
            "scheduler-master-archive-UPPER-1.jpg",
            "scheduler-master-archive-a+0-1.jpg",
            "scheduler-master-archive-a+x-1.jpg",
            "testshot-master-archive-dawn-1.jpg",
            "testshot-master-archive-failed-1.jpg",
            "scheduler-master-archive",
        ] {
            assert_eq!(parse_capture_filename(name), None, "{name}");
        }
    }
}
