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

    /// Seek to this timestamp before grabbing the frame (e.g. 5, 0:30, 00:01:15)
    #[arg(short, long)]
    seek: Option<String>,

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

fn main() {
    let mut args = Args::parse();

    // Normalize so "--format .png" and "--format PNG" behave like "png".
    args.format = normalize_ext(&args.format);
    if args.format.is_empty() {
        fail("--format must not be empty");
    }

    let extensions: Vec<String> = args
        .extensions
        .iter()
        .map(|e| normalize_ext(e))
        .filter(|e| !e.is_empty())
        .collect();

    if !args.dry_run {
        if !ffmpeg_available() {
            fail("ffmpeg was not found on PATH");
        }
        if let Err(e) = fs::create_dir_all(&args.output) {
            fail(&format!(
                "failed to create output directory {}: {e}",
                args.output.display()
            ));
        }
    }

    let videos = match find_videos(&args.input, &extensions) {
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
        let output = output_path(&path, &args.output, &args.format);

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

        if args.dry_run {
            println!("{}", describe(&path, &output, &args));
            continue;
        }

        if let Err(msg) = extract(&path, &output, &args) {
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

/// Video files directly inside `dir`, sorted by name for a predictable order.
fn find_videos(dir: &Path, extensions: &[String]) -> std::io::Result<Vec<PathBuf>> {
    let mut videos: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .map(|e| extensions.contains(&e.to_string_lossy().to_lowercase()))
                .unwrap_or(false)
        })
        .collect();
    videos.sort();
    Ok(videos)
}

fn output_path(video: &Path, out_dir: &Path, format: &str) -> PathBuf {
    // Work with OsString so file names that aren't valid UTF-8 still work.
    let mut name: OsString = video.file_stem().unwrap_or_default().to_owned();
    name.push(".");
    name.push(format);
    out_dir.join(name)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run ffmpeg and confirm a frame was actually written.
fn extract(input: &Path, output: &Path, args: &Args) -> Result<(), String> {
    let status = build_command(input, output, args)
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

fn build_command(input: &Path, output: &Path, args: &Args) -> Command {
    let mut cmd = Command::new("ffmpeg");

    // Don't let ffmpeg read keystrokes from the terminal.
    cmd.arg("-nostdin");
    cmd.arg(if args.overwrite { "-y" } else { "-n" });

    if args.quiet {
        cmd.args(["-loglevel", "error"]);
    }

    // Seeking before -i is fast (keyframe seek).
    if let Some(seek) = &args.seek {
        cmd.arg("-ss").arg(seek);
    }

    cmd.arg("-i").arg(input);

    // -update 1 tells the image2 muxer this is a single image, not a sequence.
    cmd.args(["-frames:v", "1", "-update", "1"]);

    // JPEG is much smaller than PNG while retaining near-maximal visual quality.
    // FFmpeg's MJPEG quality scale runs from 2 (best) to 31 (worst).
    if matches!(args.format.as_str(), "jpg" | "jpeg") {
        cmd.args(["-q:v", "2"]);
    }

    cmd.arg(output);
    cmd
}

/// The ffmpeg command as a line that can be pasted into a POSIX shell.
fn describe(input: &Path, output: &Path, args: &Args) -> String {
    let cmd = build_command(input, output, args);
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
        let p = output_path(Path::new("in/my clip.mkv"), Path::new("out"), "jpg");
        assert_eq!(p, Path::new("out/my clip.jpg"));
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("ffmpeg"), "ffmpeg");
        assert_eq!(shell_quote("./a.mp4"), "./a.mp4");
        assert_eq!(shell_quote("my video.mp4"), "'my video.mp4'");
        assert_eq!(shell_quote("it's.mp4"), r"'it'\''s.mp4'");
    }
}
