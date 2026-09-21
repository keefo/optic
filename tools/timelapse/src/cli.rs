//! Hand-rolled argument parsing (`docs/timelapse-builder.md` §7). Pure:
//! the home directory is passed in so defaults are testable.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::encode::{Codec, EncodeOptions};
use crate::names::Profile;
use crate::plan::{GroupBy, PlanOptions, Selection};

pub const USAGE: &str = "\
optic-timelapse: build timelapse videos from synced Optic captures

USAGE:
  optic-timelapse [OPTIONS]

SELECTION:
  --src DIR            capture directory, read-only [default: ~/Pictures/Optic]
  --rule SLUG          only this rule (repeatable) [default: every rule]
  --profile SLUG       master-archive | 4k-dci | 2k-binning
  --date YYYY-MM-DD    a single local day
  --from YYYY-MM-DD    window start, inclusive
  --to YYYY-MM-DD      window end, inclusive
  --include-manual     also group web-UI testshots under rule `manual`

GROUPING:
  --group-by day|range one video per local day, or one per whole window [default: day]
  --split-gap DUR|off  split where frames are further apart than DUR (e.g. 90s, 30m, 1h, 2d)
                       [default: 1h for day, off for range]
  --min-frames N       skip segments with fewer frames [default: 10]

ENCODING:
  --out DIR            output directory [default: ~/Movies/Optic Timelapses]
  --fps N              [default: 24]
  --width PX           maximum output width, never upscaled [default: 3840]
  --codec C            hevc (libx265) | hevc-vt (VideoToolbox, <=3840x2160) | h264 [default: hevc]
  --bitrate RATE       hevc-vt only [default: 40M]
  --no-deflicker       disable the deflicker filter
  --overwrite          replace existing output files
  --ffmpeg PATH        ffmpeg binary [default: ffmpeg on PATH]

  --dry-run            print frame lists and ffmpeg commands; write nothing
  -h, --help           show this help
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub src: PathBuf,
    pub out: PathBuf,
    pub ffmpeg: PathBuf,
    pub selection: Selection,
    pub plan: PlanOptions,
    pub encode: EncodeOptions,
    pub overwrite: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(Args),
    Help,
}

pub fn parse_args<I, S>(args: I, home: &Path) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut src = home.join("Pictures/Optic");
    let mut out = home.join("Movies/Optic Timelapses");
    let mut ffmpeg = PathBuf::from("ffmpeg");
    let mut selection = Selection::default();
    let mut group_by = GroupBy::Day;
    let mut split_gap: Option<Option<u64>> = None;
    let mut min_frames = 10usize;
    let mut encode = EncodeOptions {
        codec: Codec::Hevc,
        fps: 24,
        max_width: 3840,
        bitrate: "40M".into(),
        deflicker: true,
    };
    let mut overwrite = false;
    let mut dry_run = false;
    let mut date: Option<NaiveDate> = None;

    let mut args = args.into_iter().map(Into::into);
    while let Some(flag) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--src" => src = PathBuf::from(value("--src")?),
            "--out" => out = PathBuf::from(value("--out")?),
            "--ffmpeg" => ffmpeg = PathBuf::from(value("--ffmpeg")?),
            "--rule" => selection.rules.push(value("--rule")?),
            "--profile" => {
                let v = value("--profile")?;
                selection.profile =
                    Some(Profile::from_slug(&v).ok_or_else(|| format!("unknown profile '{v}'"))?);
            }
            "--date" => date = Some(parse_date(&value("--date")?)?),
            "--from" => selection.from = Some(parse_date(&value("--from")?)?),
            "--to" => selection.to = Some(parse_date(&value("--to")?)?),
            "--include-manual" => selection.include_manual = true,
            "--group-by" => {
                group_by = match value("--group-by")?.as_str() {
                    "day" => GroupBy::Day,
                    "range" => GroupBy::Range,
                    other => return Err(format!("--group-by must be day or range, got '{other}'")),
                }
            }
            "--split-gap" => split_gap = Some(parse_duration(&value("--split-gap")?)?),
            "--min-frames" => {
                min_frames = parse_positive("--min-frames", &value("--min-frames")?)? as usize
            }
            "--fps" => encode.fps = parse_positive("--fps", &value("--fps")?)?,
            "--width" => encode.max_width = parse_positive("--width", &value("--width")?)?,
            "--codec" => {
                let v = value("--codec")?;
                encode.codec = Codec::parse(&v)
                    .ok_or_else(|| format!("--codec must be hevc, hevc-vt, or h264, got '{v}'"))?;
            }
            "--bitrate" => encode.bitrate = value("--bitrate")?,
            "--no-deflicker" => encode.deflicker = false,
            "--overwrite" => overwrite = true,
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown argument '{other}' (see --help)")),
        }
    }

    if let Some(day) = date {
        if selection.from.is_some() || selection.to.is_some() {
            return Err("--date can't be combined with --from/--to".into());
        }
        selection.from = Some(day);
        selection.to = Some(day);
    }
    if let (Some(from), Some(to)) = (selection.from, selection.to)
        && from > to
    {
        return Err(format!("--from {from} is after --to {to}"));
    }
    let split_gap_ms = split_gap.unwrap_or(match group_by {
        GroupBy::Day => Some(3_600_000),
        GroupBy::Range => None,
    });

    Ok(Command::Run(Args {
        src,
        out,
        ffmpeg,
        selection,
        plan: PlanOptions {
            group_by,
            split_gap_ms,
            min_frames,
        },
        encode,
        overwrite,
        dry_run,
    }))
}

pub fn parse_date(value: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("invalid date '{value}', expected YYYY-MM-DD"))
}

/// `off` gives `None`; otherwise `<n>s|m|h|d` in milliseconds.
pub fn parse_duration(value: &str) -> Result<Option<u64>, String> {
    if value == "off" {
        return Ok(None);
    }
    let err = || format!("invalid duration '{value}', expected e.g. 90s, 30m, 1h, 2d, or off");
    let unit_ms = match value.chars().last().ok_or_else(err)? {
        's' => 1_000,
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        _ => return Err(err()),
    };
    let n: u64 = value[..value.len() - 1].parse().map_err(|_| err())?;
    if n == 0 {
        return Err(err());
    }
    n.checked_mul(unit_ms).map(Some).ok_or_else(err)
}

fn parse_positive(name: &str, value: &str) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("{name} must be a positive integer, got '{value}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Args, String> {
        match parse_args(args.iter().copied(), Path::new("/Users/me"))? {
            Command::Run(args) => Ok(args),
            Command::Help => Err("help".into()),
        }
    }

    #[test]
    fn defaults_match_the_design() {
        let args = run(&[]).unwrap();
        assert_eq!(args.src, PathBuf::from("/Users/me/Pictures/Optic"));
        assert_eq!(args.out, PathBuf::from("/Users/me/Movies/Optic Timelapses"));
        assert_eq!(args.encode.codec, Codec::Hevc);
        assert_eq!(args.encode.fps, 24);
        assert_eq!(args.encode.max_width, 3840);
        assert!(args.encode.deflicker);
        assert_eq!(args.plan.group_by, GroupBy::Day);
        assert_eq!(args.plan.split_gap_ms, Some(3_600_000));
        assert_eq!(args.plan.min_frames, 10);
        assert!(!args.dry_run && !args.overwrite);
    }

    #[test]
    fn range_mode_defaults_split_gap_off_unless_given() {
        assert_eq!(
            run(&["--group-by", "range"]).unwrap().plan.split_gap_ms,
            None
        );
        assert_eq!(
            run(&["--group-by", "range", "--split-gap", "2d"])
                .unwrap()
                .plan
                .split_gap_ms,
            Some(2 * 86_400_000)
        );
        assert_eq!(
            run(&["--split-gap", "off"]).unwrap().plan.split_gap_ms,
            None
        );
    }

    #[test]
    fn date_sets_a_one_day_window() {
        let args = run(&["--date", "2026-09-20", "--rule", "a", "--rule", "b"]).unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 9, 20);
        assert_eq!((args.selection.from, args.selection.to), (d, d));
        assert_eq!(args.selection.rules, ["a", "b"]);
    }

    #[test]
    fn rejects_bad_combinations_and_values() {
        for bad in [
            &["--date", "2026-09-20", "--from", "2026-09-19"][..],
            &["--from", "2026-09-21", "--to", "2026-09-20"],
            &["--date", "2026-9-40"],
            &["--profile", "master_archive"],
            &["--codec", "prores"],
            &["--fps", "0"],
            &["--split-gap", "10"],
            &["--split-gap", "0h"],
            &["--group-by", "week"],
            &["--rule"],
            &["--bogus"],
        ] {
            assert!(run(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn help_flag() {
        assert_eq!(
            parse_args(["--dry-run", "-h"], Path::new("/")).unwrap(),
            Command::Help
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Ok(Some(90_000)));
        assert_eq!(parse_duration("30m"), Ok(Some(1_800_000)));
        assert_eq!(parse_duration("1h"), Ok(Some(3_600_000)));
        assert_eq!(parse_duration("off"), Ok(None));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("1.5h").is_err());
        assert!(parse_duration("99999999999999999d").is_err());
    }
}
