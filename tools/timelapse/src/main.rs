//! I/O shell around the pure planning code: scans the capture directory
//! (read-only), reads sidecars, prints the plan, and runs ffmpeg.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};

use chrono::Local;

use optic_timelapse::cli::{self, Args, Command};
use optic_timelapse::encode::{self, Codec};
use optic_timelapse::names::{self, Kind};
use optic_timelapse::plan::{self, DropReason, Frame, Segment, Sidecar};

fn main() -> ExitCode {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    match cli::parse_args(std::env::args().skip(1), &home) {
        Ok(Command::Help) => {
            print!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::Run(args)) => match run(&args) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::from(2)
            }
        },
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &Args) -> Result<ExitCode, String> {
    let src = fs::canonicalize(&args.src)
        .map_err(|e| format!("capture directory {}: {e}", args.src.display()))?;
    if !src.is_dir() {
        return Err(format!("{} is not a directory", src.display()));
    }
    let out = resolve_path(&args.out)
        .map_err(|e| format!("output directory {}: {e}", args.out.display()))?;
    encode::check_output_dir(&src, &out)?;

    let (frames, bad_sidecars) = scan(&src, args)?;
    let plan = plan::plan(&frames, &args.selection, &args.plan, &Local);

    println!("source:  {}", src.display());
    println!("output:  {}", out.display());
    println!(
        "matched: {} frame(s); {} without sidecar; {} unreadable sidecar(s)",
        frames.len(),
        plan.frames_without_sidecar,
        bad_sidecars
    );
    for (file, reason) in &plan.dropped {
        match reason {
            DropReason::FailedCapture => println!("dropped: {file} (sidecar success=false)"),
            DropReason::ResolutionMismatch { got, expected } => println!(
                "dropped: {file} ({}x{} != group size {}x{})",
                got.0, got.1, expected.0, expected.1
            ),
        }
    }
    for segment in &plan.skipped {
        println!(
            "skipped: {} / {} {} - {} frame(s), below --min-frames {}",
            segment.rule,
            segment.profile.slug(),
            span(segment),
            segment.frames.len(),
            args.plan.min_frames
        );
    }
    if plan.segments.is_empty() {
        println!("nothing to encode");
        return Ok(ExitCode::SUCCESS);
    }

    if !args.dry_run {
        check_encoder(&args.ffmpeg, args.encode.codec)?;
        fs::create_dir_all(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;
    }

    let mut failures = 0;
    for (index, segment) in plan.segments.iter().enumerate() {
        let size = encode::output_size(segment.size, args.encode.max_width, args.encode.codec);
        let (first, last) = (
            local(segment.frames.first().expect("non-empty").millis),
            local(segment.frames.last().expect("non-empty").millis),
        );
        let output = out.join(encode::output_file_name(
            &segment.rule,
            segment.profile,
            &first,
            &last,
        ));
        let workdir =
            std::env::temp_dir().join(format!("optic-timelapse-{}-{index}", process::id()));
        let ffmpeg_args = encode::ffmpeg_args(
            &workdir.join("%06d.jpg"),
            &encode::partial_path(&output),
            size,
            &args.encode,
        );

        println!();
        println!(
            "segment {}: {} / {} {} - {} frame(s), median interval {}, {} gap(s)",
            index + 1,
            segment.rule,
            segment.profile.slug(),
            span(segment),
            segment.frames.len(),
            segment
                .median_interval_ms
                .map_or("n/a".into(), |ms| format!("{:.1}s", ms as f64 / 1000.0)),
            segment.gaps.len()
        );
        println!(
            "  video: {}x{} -> {}x{} @ {} fps = {:.2}s -> {}",
            segment.size.0,
            segment.size.1,
            size.0,
            size.1,
            args.encode.fps,
            segment.frames.len() as f64 / args.encode.fps as f64,
            output.display()
        );
        for gap in &segment.gaps {
            let before = &segment.frames[gap.after_index];
            println!(
                "  gap: {:.0}s after {} (~{} missed)",
                gap.gap_ms as f64 / 1000.0,
                local(before.millis).format("%H:%M:%S"),
                gap.missed_estimate
            );
        }

        if args.dry_run {
            print_frames(segment);
            println!(
                "  command: {}",
                encode::display_command(&args.ffmpeg, &ffmpeg_args)
            );
            continue;
        }
        match encode_segment(segment, &workdir, &output, &ffmpeg_args, args) {
            Ok(()) => println!("  wrote {}", output.display()),
            Err(err) => {
                eprintln!("  error: {err}");
                failures += 1;
            }
        }
    }

    if args.dry_run {
        println!();
        println!("dry run: nothing written");
    }
    Ok(if failures > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Lists the source directory (non-recursive) and reads the sidecar of
/// each admitted JPG. Never writes to `src`.
fn scan(src: &Path, args: &Args) -> Result<(Vec<Frame>, usize), String> {
    let entries = fs::read_dir(src).map_err(|e| format!("reading {}: {e}", src.display()))?;
    let mut frames = Vec::new();
    let mut bad_sidecars = 0;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", src.display()))?;
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(name) = names::parse_capture_filename(&file_name) else {
            continue;
        };
        if name.kind != Kind::Jpg
            || !args.selection.admits_name(&name, &Local)
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let stem = file_name.strip_suffix(".jpg").expect("jpg kind");
        let sidecar = match read_sidecar(&src.join(format!("{stem}.log.json"))) {
            Ok(sidecar) => sidecar,
            Err(err) => {
                eprintln!("warning: {stem}.log.json: {err}");
                bad_sidecars += 1;
                None
            }
        };
        frames.push(Frame {
            path: entry.path(),
            file_name,
            name,
            sidecar,
        });
    }
    Ok((frames, bad_sidecars))
}

fn read_sidecar(path: &Path) -> Result<Option<Sidecar>, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn check_encoder(ffmpeg: &Path, codec: Codec) -> Result<(), String> {
    let output = process::Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|e| {
            format!(
                "can't run ffmpeg ({}): {e}; install it with `brew install ffmpeg` or pass --ffmpeg PATH",
                ffmpeg.display()
            )
        })?;
    let listing = String::from_utf8_lossy(&output.stdout);
    let wanted = codec.encoder_name();
    if listing
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(wanted))
    {
        Ok(())
    } else {
        Err(format!(
            "{} does not provide the {wanted} encoder",
            ffmpeg.display()
        ))
    }
}

/// Removes the per-segment symlink directory however `encode_segment` exits.
struct WorkDir<'a>(&'a Path);

impl Drop for WorkDir<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0);
    }
}

fn encode_segment(
    segment: &Segment,
    workdir: &Path,
    output: &Path,
    ffmpeg_args: &[std::ffi::OsString],
    args: &Args,
) -> Result<(), String> {
    if output.exists() && !args.overwrite {
        return Err(format!(
            "{} already exists; pass --overwrite to replace it",
            output.display()
        ));
    }
    if workdir.exists() {
        fs::remove_dir_all(workdir).map_err(|e| format!("clearing {}: {e}", workdir.display()))?;
    }
    fs::create_dir(workdir).map_err(|e| format!("creating {}: {e}", workdir.display()))?;
    let _cleanup = WorkDir(workdir);
    for (i, frame) in segment.frames.iter().enumerate() {
        std::os::unix::fs::symlink(&frame.path, workdir.join(format!("{i:06}.jpg")))
            .map_err(|e| format!("linking {}: {e}", frame.file_name))?;
    }

    let partial = encode::partial_path(output);
    let status = process::Command::new(&args.ffmpeg)
        .args(ffmpeg_args)
        .status()
        .map_err(|e| format!("running {}: {e}", args.ffmpeg.display()))?;
    if !status.success() {
        let _ = fs::remove_file(&partial);
        return Err(format!("ffmpeg failed ({status})"));
    }
    fs::rename(&partial, output).map_err(|e| {
        format!(
            "renaming {} to {}: {e}",
            partial.display(),
            output.display()
        )
    })
}

fn print_frames(segment: &Segment) {
    let mut gaps = segment.gaps.iter().peekable();
    for (i, frame) in segment.frames.iter().enumerate() {
        println!(
            "  {:>6}  {}  {}",
            i,
            local(frame.millis).format("%Y-%m-%d %H:%M:%S%.3f"),
            frame.file_name
        );
        if let Some(gap) = gaps.next_if(|g| g.after_index == i) {
            println!(
                "          ---- gap {:.0}s (~{} missed) ----",
                gap.gap_ms as f64 / 1000.0,
                gap.missed_estimate
            );
        }
    }
}

fn local(millis: u64) -> chrono::DateTime<Local> {
    plan::local_time(millis, &Local).expect("capture timestamp in range")
}

fn span(segment: &Segment) -> String {
    let (first, last) = (
        local(segment.frames.first().expect("non-empty").millis),
        local(segment.frames.last().expect("non-empty").millis),
    );
    format!(
        "{} .. {}",
        first.format("%Y-%m-%d %H:%M:%S"),
        last.format("%Y-%m-%d %H:%M:%S")
    )
}

/// Absolute, symlink-resolved form of a path that may not exist yet: the
/// nearest existing ancestor is canonicalized and the rest appended.
fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::env::current_dir()?.join(path);
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(base) => {
                return Ok(rest.iter().rev().fold(base, |acc, part| acc.join(part)));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                rest.push(existing.file_name().ok_or(e)?.to_owned());
                existing = existing
                    .parent()
                    .ok_or_else(|| io::Error::other("no existing ancestor"))?;
            }
            Err(e) => return Err(e),
        }
    }
}
