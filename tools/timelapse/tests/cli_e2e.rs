//! End-to-end tests of the `optic-timelapse` binary against tiny generated
//! fixtures in a temp directory (never real captures). The encode test needs
//! `ffmpeg` on PATH and is skipped, loudly, without it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_optic-timelapse");
/// 2026-09-20 12:30 UTC: mid-day in every timezone from UTC-11 to UTC+11,
/// so a 12-minute run never crosses a local midnight.
const BASE_MS: u64 = 1_789_907_400_000;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> TempRoot {
        let root =
            std::env::temp_dir().join(format!("optic-timelapse-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        TempRoot(root)
    }
    fn src(&self) -> PathBuf {
        self.0.join("src")
    }
    fn out(&self) -> PathBuf {
        self.0.join("out")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Name, size, and modification time of every entry, to prove the source
/// directory is untouched.
fn listing(dir: &Path) -> BTreeMap<String, (u64, std::time::SystemTime)> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let meta = e.metadata().unwrap();
            (
                e.file_name().into_string().unwrap(),
                (meta.len(), meta.modified().unwrap()),
            )
        })
        .collect()
}

fn write_sidecar(src: &Path, stem: &str, rule: &str, size: (u32, u32)) {
    fs::write(
        src.join(format!("{stem}.log.json")),
        format!(
            r#"{{"capture_id":"{stem}","success":true,"triggered_by":["{rule}"],"width":{},"height":{}}}"#,
            size.0, size.1
        ),
    )
    .unwrap();
}

fn run(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn encodes_generated_frames_in_timestamp_order_and_leaves_source_untouched() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED: ffmpeg not on PATH");
        return;
    }
    let root = TempRoot::new("order");
    let src = root.src();
    const N: u64 = 12;
    // Written newest-first so directory order can't accidentally match.
    for i in (0..N).rev() {
        let level = 40 + 16 * i;
        let stem = format!("scheduler-2k-binning-synth-{}", BASE_MS + i * 60_000);
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!(
                "color=c=0x{level:02x}{level:02x}{level:02x}:s=64x48"
            ))
            .args(["-frames:v", "1", "-q:v", "2"])
            .arg(src.join(format!("{stem}.jpg")))
            .status()
            .unwrap();
        assert!(status.success());
        write_sidecar(&src, &stem, "synth", (64, 48));
    }
    let before = listing(&src);

    let output = run(&[
        "--src".as_ref(),
        src.as_os_str(),
        "--out".as_ref(),
        root.out().as_os_str(),
        "--no-deflicker".as_ref(),
        "--codec".as_ref(),
        "h264".as_ref(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(listing(&src), before, "source directory changed");

    let videos: Vec<PathBuf> = fs::read_dir(root.out())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(videos.len(), 1, "{videos:?}");
    let name = videos[0].file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("synth_2k-binning_") && name.ends_with(".mp4"));
    assert!(!name.contains(".partial"));

    // One mean-luma byte per decoded frame.
    let decoded = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(&videos[0])
        .args([
            "-vf",
            "format=gray,scale=1:1:flags=area",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .unwrap();
    assert!(decoded.status.success());
    let lumas = decoded.stdout;
    assert_eq!(lumas.len(), N as usize, "frame count");
    assert!(
        lumas.windows(2).all(|w| w[0] < w[1]),
        "frames out of order: {lumas:?}"
    );
}

#[test]
fn refuses_an_output_directory_inside_the_source() {
    let root = TempRoot::new("inside");
    let src = root.src();
    let before = listing(&src);
    let output = run(&[
        "--src".as_ref(),
        src.as_os_str(),
        "--out".as_ref(),
        src.join("videos").as_os_str(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("inside the capture directory"));
    assert_eq!(listing(&src), before);
}

#[test]
fn reports_a_missing_ffmpeg_clearly() {
    let root = TempRoot::new("noffmpeg");
    let src = root.src();
    for i in 0..10 {
        let stem = format!("scheduler-master-archive-r-{}", BASE_MS + i * 60_000);
        // Planning never decodes images, so empty files are enough here.
        fs::write(src.join(format!("{stem}.jpg")), b"").unwrap();
    }
    let output = run(&[
        "--src".as_ref(),
        src.as_os_str(),
        "--out".as_ref(),
        root.out().as_os_str(),
        "--ffmpeg".as_ref(),
        "/nonexistent/ffmpeg".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("can't run ffmpeg"), "{stderr}");
    assert!(
        !root.out().exists(),
        "output dir created before ffmpeg check"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let root = TempRoot::new("dry");
    let src = root.src();
    for i in 0..10 {
        let stem = format!("scheduler-master-archive-r-{}", BASE_MS + i * 60_000);
        fs::write(src.join(format!("{stem}.jpg")), b"").unwrap();
    }
    let output = run(&[
        "--src".as_ref(),
        src.as_os_str(),
        "--out".as_ref(),
        root.out().as_os_str(),
        "--dry-run".as_ref(),
    ]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("segment 1: r / master-archive"), "{stdout}");
    assert!(stdout.contains("command: ffmpeg "), "{stdout}");
    assert!(!root.out().exists());
}
