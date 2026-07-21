use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    #[arg(short, long, default_value = "png")]
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
    let args = Args::parse();

    // Normalize extensions for case-insensitive comparison.
    let extensions: Vec<String> = args
        .extensions
        .iter()
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    if !args.dry_run {
        if let Err(e) = fs::create_dir_all(&args.output) {
            eprintln!(
                "error: failed to create output directory {}: {e}",
                args.output.display()
            );
            std::process::exit(1);
        }
    }

    let entries = match fs::read_dir(&args.input) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!(
                "error: failed to read input directory {}: {e}",
                args.input.display()
            );
            std::process::exit(1);
        }
    };

    let mut processed = 0usize;
    let mut failed = 0usize;

    for entry in entries {
        let path = match entry {
            Ok(e) => e.path(),
            Err(_) => continue,
        };

        if !path.is_file() {
            continue;
        }

        let is_video = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| extensions.contains(&e.to_lowercase()))
            .unwrap_or(false);

        if !is_video {
            continue;
        }

        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };

        let output = args.output.join(format!("{stem}.{}", args.format));

        if output.exists() && !args.overwrite {
            if !args.quiet {
                println!("skipping {} (already exists)", output.display());
            }
            continue;
        }

        if args.dry_run {
            println!("{}", describe(&path, &output, &args));
            continue;
        }

        match run_ffmpeg(&path, &output, &args) {
            Ok(true) => {
                processed += 1;
                if args.delete_source {
                    match trash::delete(&path) {
                        Ok(()) => {
                            if !args.quiet {
                                println!("trashed {}", path.display());
                            }
                        }
                        Err(e) => eprintln!("failed to trash {}: {e}", path.display()),
                    }
                }
            }
            Ok(false) => {
                failed += 1;
                eprintln!("ffmpeg failed for {}", path.display());
            }
            Err(e) => {
                failed += 1;
                eprintln!("failed to run ffmpeg for {}: {e}", path.display());
            }
        }
    }

    if !args.quiet && !args.dry_run {
        println!("done: {processed} extracted, {failed} failed");
    }

    if failed > 0 {
        std::process::exit(1);
    }
}

fn build_command(input: &Path, output: &Path, args: &Args) -> Command {
    let mut cmd = Command::new("ffmpeg");

    if args.overwrite {
        cmd.arg("-y");
    } else {
        cmd.arg("-n");
    }

    if args.quiet {
        cmd.args(["-loglevel", "error"]);
    }

    // Seeking before -i is fast (keyframe seek).
    if let Some(seek) = &args.seek {
        cmd.arg("-ss").arg(seek);
    }

    cmd.arg("-i").arg(input);
    cmd.args(["-frames:v", "1"]);
    cmd.arg(output);
    cmd
}

fn run_ffmpeg(input: &Path, output: &Path, args: &Args) -> std::io::Result<bool> {
    let status = build_command(input, output, args).status()?;
    Ok(status.success())
}

fn describe(input: &Path, output: &Path, args: &Args) -> String {
    let cmd = build_command(input, output, args);
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}
