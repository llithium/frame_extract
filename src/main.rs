use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Parser;

/// Extract a single frame from every video in a directory using ffmpeg.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Directory to scan for video files
    #[arg(short, long, default_value = ".")]
    input: PathBuf,

    /// Directory to write extracted frames to
    #[arg(short, long, default_value = "frames")]
    output: PathBuf,

    /// Video file extensions to process (case-insensitive)
    #[arg(
        short,
        long,
        value_delimiter = ',',
        default_value = "mp4,mkv,mov,avi,webm"
    )]
    extensions: Vec<String>,

    /// Image format / extension for the extracted frames
    #[arg(short, long, default_value = "jpg")]
    format: String,

    /// Where to grab the frame: a timestamp (5, 0:30, 00:01:15) or a
    /// percentage of each video's length (50%)
    #[arg(short, long, value_parser = parse_seek)]
    seek: Option<Seek>,

    /// JPEG quality from 2 (best, largest) to 31 (worst, smallest)
    #[arg(short = 'Q', long, value_parser = clap::value_parser!(u8).range(2..=31))]
    quality: Option<u8>,

    /// Also scan subdirectories, mirroring their layout in the output directory
    #[arg(short, long)]
    recursive: bool,

    /// Overwrite existing frame files instead of skipping them
    #[arg(short = 'y', long)]
    overwrite: bool,

    /// Move each source video to the trash after its frame is successfully extracted
    #[arg(short = 'd', long)]
    delete_source: bool,

    /// Print the ffmpeg commands without running them
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Suppress ffmpeg's own output
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Seek {
    /// A timestamp passed to ffmpeg as-is.
    Time(String),
    /// A percentage of the video's duration, in [0, 100).
    Percent(f64),
}

fn parse_seek(s: &str) -> Result<Seek, String> {
    let s = s.trim();
    let Some(pct) = s.strip_suffix('%') else {
        if s.is_empty() {
            return Err("seek position must not be empty".into());
        }
        return Ok(Seek::Time(s.to_string()));
    };
    let pct: f64 = pct
        .trim()
        .parse()
        .map_err(|_| format!("invalid percentage: {s}"))?;
    // 100% is the very end of the video, where there is no frame to grab.
    if !(0.0..100.0).contains(&pct) {
        return Err("percentage must be at least 0% and less than 100%".into());
    }
    Ok(Seek::Percent(pct))
}

const DEFAULT_JPEG_QUALITY: u8 = 2;

fn main() {
    let mut args = Args::parse();

    // Normalize so "--format .png" and "--format PNG" behave like "png".
    args.format = normalize_ext(&args.format);
    if args.format.is_empty() {
        fail("--format must not be empty");
    }
    if args.quality.is_some() && !is_jpeg(&args.format) {
        fail("--quality only applies to the jpg/jpeg format");
    }

    let extensions: Vec<String> = args
        .extensions
        .iter()
        .map(|e| normalize_ext(e))
        .filter(|e| !e.is_empty())
        .collect();

    // Percentage seeks need ffprobe to read durations, even for a dry run.
    if matches!(args.seek, Some(Seek::Percent(_))) && !tool_available("ffprobe") {
        fail("ffprobe was not found on PATH (needed for a percentage --seek)");
    }
    if !args.dry_run {
        if !tool_available("ffmpeg") {
            fail("ffmpeg was not found on PATH");
        }
        if let Err(e) = fs::create_dir_all(&args.output) {
            fail(&format!(
                "failed to create output directory {}: {e}",
                args.output.display()
            ));
        }
    }

    let videos = match find_videos(&args.input, &extensions, args.recursive, &args.output) {
        Ok(v) => v,
        Err(e) => fail(&format!(
            "failed to read input directory {}: {e}",
            args.input.display()
        )),
    };

    let mut processed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut claimed: HashSet<PathBuf> = HashSet::new();

    for path in videos {
        let output = output_path(&path, &args.input, &args.output, &args.format);

        // Two videos with the same stem (clip.mp4, clip.mkv) would write the
        // same frame file. Refuse the second rather than silently skipping it
        // or overwriting the first one's frame.
        if !claimed.insert(output.clone()) {
            failed += 1;
            eprintln!(
                "error: {} would also write {}; rename one of the videos",
                path.display(),
                output.display()
            );
            continue;
        }

        if output.exists() && !args.overwrite {
            skipped += 1;
            if !args.quiet {
                println!("skipping {} (already exists)", output.display());
            }
            continue;
        }

        let seek = match resolve_seek(&path, args.seek.as_ref()) {
            Ok(s) => s,
            Err(msg) => {
                failed += 1;
                eprintln!("error: {}: {msg}", path.display());
                continue;
            }
        };

        if args.dry_run {
            println!("{}", describe(&path, &output, seek.as_deref(), &args));
            continue;
        }

        if let Err(msg) = extract(&path, &output, seek.as_deref(), &args) {
            failed += 1;
            eprintln!("error: {}: {msg}", path.display());
            continue;
        }
        processed += 1;

        if args.delete_source {
            match trash::delete(&path) {
                Ok(()) => {
                    if !args.quiet {
                        println!("trashed {}", path.display());
                    }
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("error: failed to trash {}: {e}", path.display());
                }
            }
        }
    }

    if !args.quiet && !args.dry_run {
        println!("done: {processed} extracted, {skipped} skipped, {failed} failed");
    }

    if failed > 0 {
        std::process::exit(1);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn normalize_ext(ext: &str) -> String {
    ext.trim().trim_start_matches('.').to_lowercase()
}

fn is_jpeg(format: &str) -> bool {
    matches!(format, "jpg" | "jpeg")
}

fn has_extension(path: &Path, extensions: &[String]) -> bool {
    path.extension()
        .map(|e| extensions.contains(&e.to_string_lossy().to_lowercase()))
        .unwrap_or(false)
}

/// Video files inside `root` (and its subdirectories when `recursive`),
/// sorted by path for a predictable order. Directory symlinks are not
/// followed, and `out_dir` is never scanned.
fn find_videos(
    root: &Path,
    extensions: &[String],
    recursive: bool,
    out_dir: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    let out_dir = fs::canonicalize(out_dir).ok();
    let mut videos = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    let mut is_root = true;

    while let Some(dir) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // Only an unreadable top-level directory is fatal.
            Err(e) if is_root => return Err(e),
            Err(e) => {
                eprintln!("warning: skipping {}: {e}", dir.display());
                continue;
            }
        };
        is_root = false;

        for entry in entries.flatten() {
            let path = entry.path();
            // file_type() does not follow symlinks, which avoids loops.
            let is_real_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_real_dir {
                if recursive && fs::canonicalize(&path).ok() != out_dir {
                    dirs.push(path);
                }
            } else if path.is_file() && has_extension(&path, extensions) {
                videos.push(path);
            }
        }
    }

    videos.sort();
    Ok(videos)
}

/// The frame file for `video`: its stem plus `format`, placed under `out_dir`
/// at the same relative location the video has under `in_dir`.
fn output_path(video: &Path, in_dir: &Path, out_dir: &Path, format: &str) -> PathBuf {
    let relative_dir = video
        .parent()
        .and_then(|p| p.strip_prefix(in_dir).ok())
        .unwrap_or(Path::new(""));
    // Work with OsString so file names that aren't valid UTF-8 still work.
    let mut name: OsString = video.file_stem().unwrap_or_default().to_owned();
    name.push(".");
    name.push(format);
    out_dir.join(relative_dir).join(name)
}

/// Turn the --seek option into the value passed to ffmpeg's -ss.
fn resolve_seek(video: &Path, seek: Option<&Seek>) -> Result<Option<String>, String> {
    match seek {
        None => Ok(None),
        Some(Seek::Time(t)) => Ok(Some(t.clone())),
        Some(Seek::Percent(pct)) => {
            let duration = probe_duration(video)?;
            Ok(Some(format!("{:.3}", duration * pct / 100.0)))
        }
    }
}

/// The video's duration in seconds, read with ffprobe.
fn probe_duration(video: &Path) -> Result<f64, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration"])
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(video)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run ffprobe: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.trim().parse::<f64>() {
        Ok(d) if out.status.success() && d.is_finite() && d > 0.0 => Ok(d),
        _ => Err("could not read the video's duration".into()),
    }
}

fn tool_available(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run ffmpeg and confirm a frame was actually written.
fn extract(input: &Path, output: &Path, seek: Option<&str>, args: &Args) -> Result<(), String> {
    if let Some(dir) = output.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    }

    let status = build_command(input, output, seek, args)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("failed to run ffmpeg: {e}"))?;

    let written = fs::metadata(output).map(|m| m.len() > 0).unwrap_or(false);

    if !written {
        // Don't leave an empty file behind, or the next run would skip it.
        let _ = fs::remove_file(output);
    }

    if !status.success() {
        return Err(format!("ffmpeg exited with {status}"));
    }
    if !written {
        // ffmpeg exits 0 without writing anything when, for example, the
        // seek position is past the end of the video.
        return Err("ffmpeg produced no frame (is --seek past the end of the video?)".into());
    }
    Ok(())
}

fn build_command(input: &Path, output: &Path, seek: Option<&str>, args: &Args) -> Command {
    let mut cmd = Command::new("ffmpeg");

    // Don't let ffmpeg read keystrokes from the terminal.
    cmd.arg("-nostdin");
    cmd.arg(if args.overwrite { "-y" } else { "-n" });

    if args.quiet {
        cmd.args(["-loglevel", "error"]);
    }

    // Seeking before -i is fast (keyframe seek).
    if let Some(seek) = seek {
        cmd.arg("-ss").arg(seek);
    }

    cmd.arg("-i").arg(input);

    // -update 1 tells the image2 muxer this is a single image, not a sequence.
    cmd.args(["-frames:v", "1", "-update", "1"]);

    // JPEG is much smaller than PNG while retaining near-maximal visual quality.
    // FFmpeg's MJPEG quality scale runs from 2 (best) to 31 (worst).
    if is_jpeg(&args.format) {
        let quality = args.quality.unwrap_or(DEFAULT_JPEG_QUALITY);
        cmd.arg("-q:v").arg(quality.to_string());
    }

    cmd.arg(output);
    cmd
}

/// The ffmpeg command as a line that can be pasted into a POSIX shell.
fn describe(input: &Path, output: &Path, seek: Option<&str>, args: &Args) -> String {
    let cmd = build_command(input, output, seek, args);
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| shell_quote(&a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,+%@".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_extensions() {
        assert_eq!(normalize_ext(".PNG"), "png");
        assert_eq!(normalize_ext(" mp4 "), "mp4");
    }

    #[test]
    fn output_path_uses_stem_and_format() {
        let p = output_path(
            Path::new("in/my clip.mkv"),
            Path::new("in"),
            Path::new("out"),
            "jpg",
        );
        assert_eq!(p, Path::new("out/my clip.jpg"));
    }

    #[test]
    fn output_path_mirrors_subdirectories() {
        let p = output_path(
            Path::new("in/a/b/clip.mp4"),
            Path::new("in"),
            Path::new("out"),
            "png",
        );
        assert_eq!(p, Path::new("out/a/b/clip.png"));
    }

    #[test]
    fn parses_seek_values() {
        assert_eq!(parse_seek("0:30"), Ok(Seek::Time("0:30".into())));
        assert_eq!(parse_seek("50%"), Ok(Seek::Percent(50.0)));
        assert_eq!(parse_seek(" 12.5 % "), Ok(Seek::Percent(12.5)));
        assert!(parse_seek("100%").is_err());
        assert!(parse_seek("-5%").is_err());
        assert!(parse_seek("abc%").is_err());
        assert!(parse_seek("").is_err());
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("ffmpeg"), "ffmpeg");
        assert_eq!(shell_quote("./a.mp4"), "./a.mp4");
        assert_eq!(shell_quote("my video.mp4"), "'my video.mp4'");
        assert_eq!(shell_quote("it's.mp4"), r"'it'\''s.mp4'");
    }
}
