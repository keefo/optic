//! Frame selection, rule resolution, grouping, and segmentation
//! (`docs/timelapse-builder.md` §5). Pure: no filesystem or clock access;
//! the timezone is a parameter so tests can pin it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use chrono::{DateTime, NaiveDate, TimeZone};
use serde::Deserialize;

use crate::names::{CaptureName, Kind, Profile, Source};

pub const MANUAL_RULE: &str = "manual";
pub const UNTAGGED_RULE: &str = "untagged";

/// The subset of a `.log.json` sidecar this tool uses. Every field is
/// optional so older sidecars (no `triggered_by`) still parse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Sidecar {
    pub success: Option<bool>,
    pub triggered_by: Option<Vec<String>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Sidecar {
    fn size(&self) -> Option<(u32, u32)> {
        Some((self.width?, self.height?))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub path: PathBuf,
    pub file_name: String,
    pub name: CaptureName,
    pub sidecar: Option<Sidecar>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Day,
    Range,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Empty means every rule.
    pub rules: Vec<String>,
    pub profile: Option<Profile>,
    /// Inclusive local-date window.
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub include_manual: bool,
}

impl Selection {
    /// Cheap pre-filter on the filename alone (before any sidecar is read).
    pub fn admits_name<Tz: TimeZone>(&self, name: &CaptureName, tz: &Tz) -> bool {
        if name.kind != Kind::Jpg {
            return false;
        }
        if name.source == Source::Manual && !self.include_manual {
            return false;
        }
        if self.profile.is_some_and(|p| p != name.profile) {
            return false;
        }
        let Some(date) = local_time(name.millis, tz).map(|t| t.date_naive()) else {
            return false;
        };
        self.from.is_none_or(|from| date >= from) && self.to.is_none_or(|to| date <= to)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanOptions {
    pub group_by: GroupBy,
    /// Split a group where consecutive frames are more than this far apart.
    pub split_gap_ms: Option<u64>,
    pub min_frames: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFrame {
    pub millis: u64,
    pub file_name: String,
    pub path: PathBuf,
}

/// A gap inside a segment larger than 1.5× the segment's median interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// Index of the frame *before* the gap.
    pub after_index: usize,
    pub gap_ms: u64,
    /// `round(gap / median) - 1`.
    pub missed_estimate: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub rule: String,
    pub profile: Profile,
    /// Local date in day mode, `None` in range mode.
    pub day: Option<NaiveDate>,
    pub size: (u32, u32),
    pub frames: Vec<PlannedFrame>,
    pub median_interval_ms: Option<u64>,
    pub gaps: Vec<Gap>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    FailedCapture,
    ResolutionMismatch {
        got: (u32, u32),
        expected: (u32, u32),
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub segments: Vec<Segment>,
    /// Segments shorter than `min_frames`.
    pub skipped: Vec<Segment>,
    pub dropped: Vec<(String, DropReason)>,
    pub frames_without_sidecar: usize,
}

pub fn local_time<Tz: TimeZone>(millis: u64, tz: &Tz) -> Option<DateTime<Tz>> {
    tz.timestamp_millis_opt(i64::try_from(millis).ok()?)
        .single()
}

/// Rule membership for one frame (`docs/timelapse-builder.md` §4): sidecar
/// `triggered_by` first, then the filename tags split into known slugs,
/// then the whole tag string as one slug.
pub fn resolve_rules(
    name: &CaptureName,
    sidecar: Option<&Sidecar>,
    known_slugs: &BTreeSet<String>,
) -> Vec<String> {
    if name.source == Source::Manual {
        return vec![MANUAL_RULE.to_owned()];
    }
    if let Some(slugs) = sidecar.and_then(|s| s.triggered_by.as_ref())
        && !slugs.is_empty()
    {
        let set: BTreeSet<String> = slugs.iter().cloned().collect();
        return set.into_iter().collect();
    }
    match &name.tags {
        None => vec![UNTAGGED_RULE.to_owned()],
        Some(tags) => split_known(tags, known_slugs).unwrap_or_else(|| vec![tags.clone()]),
    }
}

/// Splits a hyphen-joined tag string into known slugs, trying the longest
/// prefix first (so a known whole slug wins over its pieces). `None` if no
/// exact split exists.
fn split_known(tags: &str, known: &BTreeSet<String>) -> Option<Vec<String>> {
    if known.contains(tags) {
        return Some(vec![tags.to_owned()]);
    }
    let cuts: Vec<usize> = tags.match_indices('-').map(|(i, _)| i).collect();
    for &cut in cuts.iter().rev() {
        let (head, tail) = (&tags[..cut], &tags[cut + 1..]);
        if known.contains(head)
            && let Some(mut rest) = split_known(tail, known)
        {
            rest.insert(0, head.to_owned());
            return Some(rest);
        }
    }
    None
}

/// Slugs named in any sidecar's `triggered_by`, used to split filename tags
/// of frames whose own sidecar lacks them.
pub fn known_slugs(frames: &[Frame]) -> BTreeSet<String> {
    frames
        .iter()
        .filter_map(|f| f.sidecar.as_ref()?.triggered_by.as_ref())
        .flatten()
        .cloned()
        .collect()
}

pub fn plan<Tz: TimeZone>(
    frames: &[Frame],
    selection: &Selection,
    options: &PlanOptions,
    tz: &Tz,
) -> Plan {
    let known = known_slugs(frames);
    let mut result = Plan::default();
    type Key = (String, Profile, Option<NaiveDate>);
    let mut groups: BTreeMap<Key, Vec<&Frame>> = BTreeMap::new();

    for frame in frames {
        if !selection.admits_name(&frame.name, tz) {
            continue;
        }
        if frame.sidecar.as_ref().and_then(|s| s.success) == Some(false) {
            result
                .dropped
                .push((frame.file_name.clone(), DropReason::FailedCapture));
            continue;
        }
        let day = match options.group_by {
            GroupBy::Day => local_time(frame.name.millis, tz).map(|t| t.date_naive()),
            GroupBy::Range => None,
        };
        let rules = resolve_rules(&frame.name, frame.sidecar.as_ref(), &known);
        let mut counted = false;
        for rule in rules {
            if !selection.rules.is_empty() && !selection.rules.contains(&rule) {
                continue;
            }
            if !counted && frame.sidecar.is_none() {
                result.frames_without_sidecar += 1;
            }
            counted = true;
            groups
                .entry((rule, frame.name.profile, day))
                .or_default()
                .push(frame);
        }
    }

    for ((rule, profile, day), mut members) in groups {
        members.sort_by_key(|f| f.name.millis);
        members.dedup_by_key(|f| f.name.millis);

        let size = majority_size(&members).unwrap_or_else(|| profile.native_size());
        members.retain(|f| match f.sidecar.as_ref().and_then(Sidecar::size) {
            Some(got) if got != size => {
                result.dropped.push((
                    f.file_name.clone(),
                    DropReason::ResolutionMismatch {
                        got,
                        expected: size,
                    },
                ));
                false
            }
            _ => true,
        });

        for run in split_runs(&members, options.split_gap_ms) {
            let frames: Vec<PlannedFrame> = run
                .iter()
                .map(|f| PlannedFrame {
                    millis: f.name.millis,
                    file_name: f.file_name.clone(),
                    path: f.path.clone(),
                })
                .collect();
            let millis: Vec<u64> = frames.iter().map(|f| f.millis).collect();
            let (median_interval_ms, gaps) = find_gaps(&millis);
            let segment = Segment {
                rule: rule.clone(),
                profile,
                day,
                size,
                frames,
                median_interval_ms,
                gaps,
            };
            if segment.frames.len() >= options.min_frames {
                result.segments.push(segment);
            } else {
                result.skipped.push(segment);
            }
        }
    }
    result
}

/// Most common sidecar-reported size in a group; ties go to the larger
/// frame count, then the smaller size, for determinism.
fn majority_size(members: &[&Frame]) -> Option<(u32, u32)> {
    let mut counts: HashMap<(u32, u32), usize> = HashMap::new();
    for size in members
        .iter()
        .filter_map(|f| f.sidecar.as_ref().and_then(Sidecar::size))
    {
        *counts.entry(size).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|(a_size, a_n), (b_size, b_n)| a_n.cmp(b_n).then(b_size.cmp(a_size)))
        .map(|(size, _)| size)
}

/// Splits sorted frames wherever consecutive frames are more than
/// `split_gap_ms` apart. A gap exactly equal to the limit doesn't split.
fn split_runs<'a>(members: &[&'a Frame], split_gap_ms: Option<u64>) -> Vec<Vec<&'a Frame>> {
    let mut runs: Vec<Vec<&Frame>> = Vec::new();
    for &frame in members {
        let starts_new = match (runs.last().and_then(|r| r.last()), split_gap_ms) {
            (None, _) => true,
            (Some(prev), Some(limit)) => frame.name.millis - prev.name.millis > limit,
            (Some(_), None) => false,
        };
        if starts_new {
            runs.push(vec![frame]);
        } else {
            runs.last_mut().expect("non-empty").push(frame);
        }
    }
    runs
}

/// Median interval and every interval above 1.5× the median.
pub fn find_gaps(millis: &[u64]) -> (Option<u64>, Vec<Gap>) {
    let deltas: Vec<u64> = millis.windows(2).map(|w| w[1] - w[0]).collect();
    if deltas.is_empty() {
        return (None, Vec::new());
    }
    let mut sorted = deltas.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    if median == 0 {
        return (Some(0), Vec::new());
    }
    let gaps = deltas
        .iter()
        .enumerate()
        .filter(|&(_, &d)| d * 2 > median * 3)
        .map(|(i, &d)| Gap {
            after_index: i,
            gap_ms: d,
            missed_estimate: ((d as f64 / median as f64).round() as u64).saturating_sub(1),
        })
        .collect();
    (Some(median), gaps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::parse_capture_filename;
    use chrono::FixedOffset;

    const MIN: u64 = 60_000;
    const HOUR: u64 = 60 * MIN;
    /// 2026-09-20 00:00:00 PDT (UTC-7) in unix ms.
    const DAY0: u64 = 1_789_887_600_000;

    fn pdt() -> FixedOffset {
        FixedOffset::west_opt(7 * 3600).unwrap()
    }

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, d).unwrap()
    }

    fn frame(file_name: &str, sidecar: Option<Sidecar>) -> Frame {
        Frame {
            path: PathBuf::from("/src").join(file_name),
            file_name: file_name.to_owned(),
            name: parse_capture_filename(file_name).unwrap(),
            sidecar,
        }
    }

    fn sidecar(rules: &[&str]) -> Option<Sidecar> {
        Some(Sidecar {
            success: Some(true),
            triggered_by: Some(rules.iter().map(|s| s.to_string()).collect()),
            width: Some(4056),
            height: Some(3040),
        })
    }

    fn sched(tags: &str, millis: u64) -> Frame {
        frame(
            &format!("scheduler-master-archive-{tags}-{millis}.jpg"),
            sidecar(&[tags]),
        )
    }

    fn opts(group_by: GroupBy, split_gap_ms: Option<u64>, min_frames: usize) -> PlanOptions {
        PlanOptions {
            group_by,
            split_gap_ms,
            min_frames,
        }
    }

    fn day_opts() -> PlanOptions {
        opts(GroupBy::Day, Some(HOUR), 1)
    }

    fn millis_of(segment: &Segment) -> Vec<u64> {
        segment.frames.iter().map(|f| f.millis).collect()
    }

    // --- resolve_rules ---

    #[test]
    fn sidecar_triggered_by_wins_over_filename() {
        let f = frame(
            "scheduler-master-archive-a-b-1.jpg",
            sidecar(&["zeta", "alpha", "zeta"]),
        );
        let rules = resolve_rules(&f.name, f.sidecar.as_ref(), &BTreeSet::new());
        assert_eq!(rules, ["alpha", "zeta"]);
    }

    #[test]
    fn filename_tags_split_into_known_slugs() {
        let known = BTreeSet::from(["dawn".to_owned(), "golden-hour".to_owned()]);
        let f = frame("scheduler-master-archive-dawn-golden-hour-1.jpg", None);
        assert_eq!(
            resolve_rules(&f.name, None, &known),
            ["dawn", "golden-hour"]
        );
    }

    #[test]
    fn a_known_whole_slug_beats_its_pieces() {
        let known = BTreeSet::from(["dawn".to_owned(), "hour".to_owned(), "dawn-hour".to_owned()]);
        let f = frame("scheduler-master-archive-dawn-hour-1.jpg", None);
        assert_eq!(resolve_rules(&f.name, None, &known), ["dawn-hour"]);
    }

    #[test]
    fn unknown_tag_string_is_one_slug() {
        let f = frame(
            "scheduler-master-archive-every1min-1.jpg",
            Some(Sidecar::default()),
        );
        assert_eq!(
            resolve_rules(&f.name, f.sidecar.as_ref(), &BTreeSet::new()),
            ["every1min"]
        );
    }

    #[test]
    fn untagged_scheduler_and_manual_frames_get_pseudo_rules() {
        let untagged = frame("scheduler-master-archive-1.jpg", None);
        assert_eq!(
            resolve_rules(&untagged.name, None, &BTreeSet::new()),
            [UNTAGGED_RULE]
        );
        let manual = frame("testshot-master-archive-1.jpg", None);
        assert_eq!(
            resolve_rules(&manual.name, None, &BTreeSet::new()),
            [MANUAL_RULE]
        );
    }

    #[test]
    fn empty_triggered_by_falls_back_to_filename() {
        let f = frame("scheduler-master-archive-dusk-1.jpg", sidecar(&[]));
        assert_eq!(
            resolve_rules(&f.name, f.sidecar.as_ref(), &BTreeSet::new()),
            ["dusk"]
        );
    }

    // --- plan ---

    #[test]
    fn frames_are_sorted_by_millis_regardless_of_input_order() {
        let frames = vec![
            sched("r", DAY0 + 3 * MIN),
            sched("r", DAY0 + MIN),
            sched("r", DAY0 + 2 * MIN),
        ];
        let plan = plan(&frames, &Selection::default(), &day_opts(), &pdt());
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(
            millis_of(&plan.segments[0]),
            [DAY0 + MIN, DAY0 + 2 * MIN, DAY0 + 3 * MIN]
        );
    }

    #[test]
    fn a_merged_two_rule_frame_lands_in_both_groups() {
        let frames = vec![
            frame(
                &format!("scheduler-master-archive-a-b-{}.jpg", DAY0 + MIN),
                sidecar(&["a", "b"]),
            ),
            sched("a", DAY0 + 2 * MIN),
        ];
        let plan = plan(&frames, &Selection::default(), &day_opts(), &pdt());
        let by_rule: BTreeMap<_, _> = plan
            .segments
            .iter()
            .map(|s| (s.rule.as_str(), millis_of(s)))
            .collect();
        assert_eq!(by_rule["a"], [DAY0 + MIN, DAY0 + 2 * MIN]);
        assert_eq!(by_rule["b"], [DAY0 + MIN]);
    }

    #[test]
    fn day_mode_splits_at_local_midnight_and_range_mode_does_not() {
        let frames = vec![
            sched("r", DAY0 - 2 * MIN), // 2026-09-19 23:58 PDT
            sched("r", DAY0 - MIN),
            sched("r", DAY0), // 2026-09-20 00:00 PDT
            sched("r", DAY0 + MIN),
        ];
        let day = plan(&frames, &Selection::default(), &day_opts(), &pdt());
        let days: Vec<_> = day.segments.iter().map(|s| s.day).collect();
        assert_eq!(days, [Some(date(19)), Some(date(20))]);
        assert_eq!(day.segments[0].frames.len(), 2);

        let range = plan(
            &frames,
            &Selection::default(),
            &opts(GroupBy::Range, None, 1),
            &pdt(),
        );
        assert_eq!(range.segments.len(), 1);
        assert_eq!(range.segments[0].day, None);
        assert_eq!(range.segments[0].frames.len(), 4);
    }

    #[test]
    fn split_gap_boundary_is_exclusive_and_off_never_splits() {
        let frames = vec![
            sched("r", DAY0),
            sched("r", DAY0 + HOUR), // exactly the limit: same segment
            sched("r", DAY0 + 2 * HOUR + 1), // just over: new segment
        ];
        let split = plan(&frames, &Selection::default(), &day_opts(), &pdt());
        let lens: Vec<_> = split.segments.iter().map(|s| s.frames.len()).collect();
        assert_eq!(lens, [2, 1]);

        let off = plan(
            &frames,
            &Selection::default(),
            &opts(GroupBy::Day, None, 1),
            &pdt(),
        );
        assert_eq!(off.segments.len(), 1);
    }

    #[test]
    fn segments_below_min_frames_are_skipped_not_dropped() {
        // Mirrors the real 2026-09-20 data: one early frame, a 17.5 h gap,
        // then a long evening run.
        let mut frames = vec![sched("every1min", DAY0 + HOUR)];
        frames.extend((0..12).map(|i| sched("every1min", DAY0 + 19 * HOUR + i * MIN)));
        let plan = plan(
            &frames,
            &Selection::default(),
            &opts(GroupBy::Day, Some(HOUR), 10),
            &pdt(),
        );
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].frames.len(), 12);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].frames.len(), 1);
    }

    #[test]
    fn gap_report_flags_missed_shots_but_not_jitter() {
        let millis = [0, 60_000, 124_000, 303_000, 363_000];
        let (median, gaps) = find_gaps(&millis);
        assert_eq!(median, Some(64_000));
        assert_eq!(
            gaps,
            [Gap {
                after_index: 2,
                gap_ms: 179_000,
                missed_estimate: 2,
            }]
        );
        assert_eq!(find_gaps(&[5]), (None, vec![]));
    }

    #[test]
    fn filters_by_rule_profile_and_inclusive_date_window() {
        let frames = vec![
            sched("keep", DAY0 - MIN),             // 09-19
            sched("keep", DAY0 + MIN),             // 09-20
            sched("keep", DAY0 + 24 * HOUR + MIN), // 09-21
            sched("keep", DAY0 + 48 * HOUR + MIN), // 09-22
            sched("other", DAY0 + 2 * MIN),
            frame(
                &format!("scheduler-4k-dci-keep-{}.jpg", DAY0 + 3 * MIN),
                None,
            ),
        ];
        let selection = Selection {
            rules: vec!["keep".into()],
            profile: Some(Profile::MasterArchive),
            from: Some(date(20)),
            to: Some(date(21)),
            include_manual: false,
        };
        let plan = plan(&frames, &selection, &day_opts(), &pdt());
        let got: Vec<_> = plan
            .segments
            .iter()
            .map(|s| (s.rule.as_str(), s.profile, s.day, s.frames.len()))
            .collect();
        assert_eq!(
            got,
            [
                ("keep", Profile::MasterArchive, Some(date(20)), 1),
                ("keep", Profile::MasterArchive, Some(date(21)), 1),
            ]
        );
    }

    #[test]
    fn manual_frames_only_with_include_manual() {
        let frames = vec![
            frame(&format!("testshot-master-archive-{}.jpg", DAY0 + MIN), None),
            sched("r", DAY0 + 2 * MIN),
        ];
        let without = plan(&frames, &Selection::default(), &day_opts(), &pdt());
        assert!(without.segments.iter().all(|s| s.rule != MANUAL_RULE));

        let with = plan(
            &frames,
            &Selection {
                include_manual: true,
                ..Selection::default()
            },
            &day_opts(),
            &pdt(),
        );
        assert!(with.segments.iter().any(|s| s.rule == MANUAL_RULE));
    }

    #[test]
    fn failed_sidecar_and_resolution_mismatch_drop_the_frame() {
        let mut odd = sched("r", DAY0 + 3 * MIN);
        odd.sidecar.as_mut().unwrap().width = Some(4056);
        odd.sidecar.as_mut().unwrap().height = Some(2160);
        let mut failed = sched("r", DAY0 + 4 * MIN);
        failed.sidecar.as_mut().unwrap().success = Some(false);
        let frames = vec![
            sched("r", DAY0 + MIN),
            sched("r", DAY0 + 2 * MIN),
            odd,
            failed,
        ];
        let plan = plan(&frames, &Selection::default(), &day_opts(), &pdt());
        assert_eq!(plan.segments[0].frames.len(), 2);
        assert_eq!(plan.segments[0].size, (4056, 3040));
        let reasons: Vec<_> = plan.dropped.iter().map(|(_, r)| r.clone()).collect();
        assert!(reasons.contains(&DropReason::FailedCapture));
        assert!(reasons.contains(&DropReason::ResolutionMismatch {
            got: (4056, 2160),
            expected: (4056, 3040),
        }));
    }

    #[test]
    fn frames_without_sidecar_are_kept_counted_and_use_native_size() {
        let frames = vec![
            frame(&format!("scheduler-2k-binning-r-{}.jpg", DAY0 + MIN), None),
            frame(
                &format!("scheduler-2k-binning-r-{}.jpg", DAY0 + 2 * MIN),
                None,
            ),
        ];
        let plan = plan(&frames, &Selection::default(), &day_opts(), &pdt());
        assert_eq!(plan.frames_without_sidecar, 2);
        assert_eq!(plan.segments[0].size, (2028, 1520));
    }
}
