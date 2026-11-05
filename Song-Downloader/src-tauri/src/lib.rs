use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Emitter;

const CHATBOT_TIP: &str = "\n\nStill stuck? Copy this exact message and paste it into an AI chatbot (like ChatGPT or Claude) and ask it to help you fix it on your machine.";

/// Songs in a row that may fail with a yt-dlp download error before a Spotify
/// download stops: one unavailable video is normal, several in a row means
/// every remaining song would fail too (each costing 5 slow attempts).
const MAX_YTDLP_FAILURE_STREAK: usize = 3;

#[tauri::command]
fn detect_source(url: &str) -> &str {
    if url.contains("spotify.com") {
        "spotify"
    } else if url.contains("youtube.com") || url.contains("youtu.be") {
        "youtube"
    } else if url.contains("soundcloud.com") {
        "soundcloud"
    } else {
        "unknown"
    }
}

fn install_hint(tool: &str) -> String {
    match tool {
        "spotdl" => "spotdl isn't installed or isn't on your PATH.\n\nFix:\n  1. Open Command Prompt and run:\n       pip install --user pipx\n       pipx install spotdl\n  2. Fully close and reopen this app (or restart your PC) so it picks up the updated PATH.".to_string(),
        "python" => "Python or yt-dlp isn't installed correctly.\n\nFix:\n  1. Install Python from python.org and check \"Add Python to PATH\" during setup.\n  2. Open Command Prompt and run:\n       pip install yt-dlp\n  3. Fully close and reopen this app (or restart your PC) so it picks up the updated PATH.".to_string(),
        "ffmpeg" | "ffprobe" => "ffmpeg isn't installed or isn't on your PATH.\n\nFix:\n  1. Open Command Prompt and run:\n       winget install Gyan.FFmpeg\n  2. Fully close and reopen this app (or restart your PC) so it picks up the updated PATH.".to_string(),
        other => format!("{} isn't installed or isn't on your PATH.", other),
    }
}

fn friendly_spawn_error(tool: &str, e: &std::io::Error) -> String {
    format!(
        "Couldn't start {tool}: {e}\n\n{hint}{tip}",
        tool = tool,
        e = e,
        hint = install_hint(tool),
        tip = CHATBOT_TIP
    )
}

fn friendly_process_error(what: &str) -> String {
    format!(
        "{what}\n\nThis is usually caused by a missing/outdated ffmpeg install, no internet connection, or the song/link being unavailable. Check the log above for the specific error, then:\n  1. Make sure ffmpeg is installed: winget install Gyan.FFmpeg\n  2. Make sure spotdl/yt-dlp are up to date: pipx upgrade spotdl  /  pipx runpip spotdl install -U yt-dlp  /  pip install -U yt-dlp\n  3. Fully close and reopen this app (or restart your PC) after installing anything.{tip}",
        what = what,
        tip = CHATBOT_TIP
    )
}

/// spotdl uses its own copy of yt-dlp, separate from the one `python -m yt_dlp`
/// runs, so the fix depends on which tool hit the error.
fn outdated_ytdlp_error(tool: &str, what: &str) -> String {
    let update_step = match tool {
        "spotdl" => "  1. spotdl uses its own copy of yt-dlp, so that's the one to update. Open Command Prompt and run the line that matches how you installed spotdl:\n       pipx:  pipx runpip spotdl install -U yt-dlp\n       venv:  %USERPROFILE%\\.spotdl-venv\\Scripts\\python -m pip install -U yt-dlp",
        _ => "  1. Open Command Prompt and run:\n       pip install -U yt-dlp",
    };
    format!(
        "{what}\n\nYouTube regularly changes how it serves audio, so this usually means yt-dlp is out of date.\n\nFix:\n{update_step}\n  2. Start the download again.{tip}",
        what = what,
        update_step = update_step,
        tip = CHATBOT_TIP
    )
}

/// spotdl exits 0 when yt-dlp fails and only reports it in its output (without
/// the underlying HTTP status, unless run at debug log level), so the output is
/// the only way to tell this apart from a bad match.
fn has_ytdlp_download_error(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.contains("YT-DLP download error") || l.contains("HTTP Error 403"))
}

/// Runs `cmd`, streaming stdout/stderr to the frontend as progress lines, and
/// returns the exit status plus the stdout and stderr lines. Spawn failures are
/// turned into a user-actionable message instead of a raw OS error.
fn run_and_stream(
    window: &tauri::Window,
    mut cmd: Command,
    tool_name: &str,
) -> Result<(std::process::ExitStatus, Vec<String>, Vec<String>), String> {
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return Err(friendly_spawn_error(tool_name, &e)),
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let window_stderr = window.clone();
    let stderr_thread = thread::spawn(move || {
        let mut stderr_lines = Vec::new();
        for line in BufReader::new(stderr).lines().flatten() {
            let _ = window_stderr.emit("download-progress", line.clone());
            stderr_lines.push(line);
        }
        stderr_lines
    });

    let mut stdout_lines = Vec::new();
    for line in BufReader::new(stdout).lines().flatten() {
        let _ = window.emit("download-progress", line.clone());
        stdout_lines.push(line);
    }
    let stderr_lines = stderr_thread.join().unwrap_or_default();

    let status = child
        .wait()
        .map_err(|e| friendly_process_error(&format!("{} process error: {}", tool_name, e)))?;
    Ok((status, stdout_lines, stderr_lines))
}

fn sanitize_filename(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    s = s.trim().trim_matches('.').to_string();
    if s.is_empty() {
        s = "untitled".to_string();
    }
    if s.chars().count() > 180 {
        s = s.chars().take(180).collect();
    }
    s
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect()
}

/// Lenient check used only as a sanity signal alongside the duration check.
/// spotdl always embeds the *expected* Spotify title/artist into the file's
/// tags regardless of which audio was actually matched, so this alone can't
/// catch a wrong-audio match — duration is the real signal for that.
fn loosely_matches(expected: &str, actual: &str) -> bool {
    let a = normalize(actual);
    let e = normalize(expected);
    if a.trim().is_empty() || e.trim().is_empty() {
        return false;
    }
    if e.contains(&a) || a.contains(&e) {
        return true;
    }
    let e_tokens: HashSet<&str> = e.split_whitespace().filter(|t| t.len() > 1).collect();
    let a_tokens: HashSet<&str> = a.split_whitespace().filter(|t| t.len() > 1).collect();
    if e_tokens.is_empty() {
        return false;
    }
    let overlap = e_tokens.intersection(&a_tokens).count();
    overlap as f64 / e_tokens.len() as f64 >= 0.4
}

fn probe_duration_secs(path: &Path) -> Option<f64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().ok()
}

fn probe_tags(path: &Path) -> (String, String) {
    let mut title = String::new();
    let mut artist = String::new();
    if let Ok(out) = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format_tags=title,artist",
            "-of",
            "default=noprint_wrappers=1:nokey=0",
        ])
        .arg(path)
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(v) = line.strip_prefix("TAG:title=") {
                title = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("TAG:artist=") {
                artist = v.trim().to_string();
            }
        }
    }
    (title, artist)
}

#[derive(Deserialize)]
struct SpotdlSong {
    name: String,
    artist: String,
    #[serde(default)]
    artists: Vec<String>,
    duration: f64,
    url: String,
    song_id: String,
}

struct SongFailure {
    reason: String,
    /// yt-dlp failed and no attempt produced any audio to verify.
    ytdlp_blocked: bool,
}

/// Downloads one Spotify song, verifying the result's length and embedded
/// title/artist against the expected metadata, retrying up to 5 times (each
/// attempt varying the audio source/provider) before giving up.
fn download_and_verify_song(
    window: &tauri::Window,
    song: &SpotdlSong,
    format: &str,
    output_path: &str,
    staging_dir: &Path,
    delay_secs: u32,
) -> Result<PathBuf, SongFailure> {
    let spotdl_format = if format == "aiff" { "wav" } else { "mp3" };
    let staged_download = staging_dir.join(format!("{}.{}", song.song_id, spotdl_format));
    // Relative on purpose, with spotdl run from inside staging_dir: spotdl strips
    // leading/trailing dots from every part of --output, so an absolute path
    // would silently write to a different folder than staging_dir.
    let output_template = "{track-id}.{output-ext}";

    let attempts: [&[&str]; 5] = [
        &[],
        &["--audio", "youtube-music", "youtube"],
        &["--audio", "youtube", "youtube-music"],
        &["--only-verified-results"],
        &["--audio", "youtube-music", "youtube", "soundcloud"],
    ];

    let mut last_reason = "unknown error".to_string();
    let mut ytdlp_failed = false;
    let mut got_audio = false;

    for (attempt_idx, extra_args) in attempts.iter().enumerate() {
        let _ = fs::remove_file(&staged_download);

        if attempt_idx > 0 {
            let _ = window.emit(
                "download-progress",
                format!("  retry {}/5 for \"{} - {}\"...", attempt_idx + 1, song.artist, song.name),
            );
        }

        let mut cmd = Command::new("spotdl");
        cmd.current_dir(staging_dir)
            .arg("download")
            .arg(&song.url)
            .arg("--format")
            .arg(spotdl_format)
            .arg("--output")
            .arg(output_template)
            .arg("--overwrite")
            .arg("force");
        if spotdl_format == "mp3" {
            cmd.arg("--bitrate").arg("320k");
        }
        if delay_secs > 0 {
            cmd.arg("--threads").arg("1");
        }
        for a in extra_args.iter() {
            cmd.arg(a);
        }

        let (status, stdout_lines, stderr_lines) = match run_and_stream(window, cmd, "spotdl") {
            Ok(result) => result,
            Err(msg) => {
                last_reason = msg;
                continue;
            }
        };

        if has_ytdlp_download_error(&stdout_lines) || has_ytdlp_download_error(&stderr_lines) {
            ytdlp_failed = true;
            last_reason = "yt-dlp couldn't download the audio from YouTube".to_string();
            continue;
        }

        if !status.success() {
            last_reason = format!("spotdl exited with {}", status);
            continue;
        }

        if !staged_download.exists() {
            last_reason = "spotdl reported success but no file was produced".to_string();
            continue;
        }
        got_audio = true;

        let actual_duration = probe_duration_secs(&staged_download);
        let duration_ok = match actual_duration {
            Some(actual) => {
                let tolerance = (song.duration * 0.15).max(12.0);
                (actual - song.duration).abs() <= tolerance
            }
            None => false,
        };

        let (tag_title, tag_artist) = probe_tags(&staged_download);
        let tags_ok = (tag_title.trim().is_empty() && tag_artist.trim().is_empty())
            || loosely_matches(&song.name, &tag_title)
            || loosely_matches(&song.artist, &tag_artist)
            || song.artists.iter().any(|a| loosely_matches(a, &tag_artist));

        if duration_ok && tags_ok {
            return finalize_song_file(&staged_download, song, format, output_path)
                .map_err(|reason| SongFailure { reason, ytdlp_blocked: false });
        }

        last_reason = match actual_duration {
            Some(actual) => format!(
                "downloaded audio is {:.0}s but the song is {:.0}s long — likely a wrong or teaser match",
                actual, song.duration
            ),
            None => "couldn't read the downloaded audio's length".to_string(),
        };
        let _ = fs::remove_file(&staged_download);
    }

    let _ = fs::remove_file(&staged_download);
    Err(SongFailure {
        reason: format!("{} (tried 5 times)", last_reason),
        ytdlp_blocked: ytdlp_failed && !got_audio,
    })
}

fn finalize_song_file(staged: &Path, song: &SpotdlSong, format: &str, output_path: &str) -> Result<PathBuf, String> {
    let final_ext = if format == "aiff" { "aiff" } else { "mp3" };
    let file_stem = sanitize_filename(&format!("{} - {}", song.artist, song.name));
    let dest = Path::new(output_path).join(format!("{}.{}", file_stem, final_ext));

    let source_for_move: PathBuf = if format == "aiff" {
        let aiff_path = staged.with_extension("aiff");
        match Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(staged)
            .arg(&aiff_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(s) if s.success() => {
                let _ = fs::remove_file(staged);
                aiff_path
            }
            Ok(_) => return Err("ffmpeg failed to convert the download to AIFF".to_string()),
            Err(e) => return Err(friendly_spawn_error("ffmpeg", &e)),
        }
    } else {
        staged.to_path_buf()
    };

    if dest.exists() {
        let _ = fs::remove_file(&dest);
    }
    fs::rename(&source_for_move, &dest).map_err(|e| format!("couldn't move the downloaded file into place: {}", e))?;
    Ok(dest)
}

fn spotify_download(window: &tauri::Window, url: &str, format: &str, output_path: &str, delay_secs: u32) {
    let _ = window.emit("download-progress", "Fetching song info from Spotify...".to_string());

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let save_file = std::env::temp_dir().join(format!("song-downloader-{}.spotdl", unique));

    let mut save_cmd = Command::new("spotdl");
    save_cmd.arg("save").arg(url).arg("--save-file").arg(&save_file);

    match run_and_stream(window, save_cmd, "spotdl") {
        Ok((status, _, _)) if status.success() => {}
        Ok((status, _, _)) => {
            let _ = window.emit(
                "download-error",
                friendly_process_error(&format!("spotdl couldn't look up that link (exited with {}).", status)),
            );
            let _ = fs::remove_file(&save_file);
            return;
        }
        Err(msg) => {
            let _ = window.emit("download-error", msg);
            let _ = fs::remove_file(&save_file);
            return;
        }
    }

    let songs: Vec<SpotdlSong> = match fs::read_to_string(&save_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(v) => v,
        None => {
            let _ = window.emit(
                "download-error",
                friendly_process_error("Couldn't read the song info spotdl returned for that link."),
            );
            let _ = fs::remove_file(&save_file);
            return;
        }
    };
    let _ = fs::remove_file(&save_file);

    if songs.is_empty() {
        let _ = window.emit("download-error", "No songs were found for that Spotify link.".to_string());
        return;
    }

    // Unique per run because it's deleted afterwards: a fixed name could match a
    // folder that already holds the user's files.
    let staging_dir = Path::new(output_path).join(format!(".song-downloader-tmp-{}", unique));
    let _ = fs::create_dir_all(&staging_dir);

    let total = songs.len();
    let mut verified = 0usize;
    let mut skipped = 0usize;
    let mut ytdlp_blocked_songs = 0usize;
    let mut ytdlp_failure_streak = 0usize;

    for (i, song) in songs.iter().enumerate() {
        let label = format!("{} - {}", song.artist, song.name);
        let _ = window.emit("download-progress", format!("\n▶ [{}/{}] {}", i + 1, total, label));

        match download_and_verify_song(window, song, format, output_path, &staging_dir, delay_secs) {
            Ok(final_path) => {
                verified += 1;
                ytdlp_failure_streak = 0;
                let name = final_path.file_name().and_then(|n| n.to_str()).unwrap_or(&label);
                let _ = window.emit("download-progress", format!("✓ Verified and saved: {}", name));
            }
            Err(failure) => {
                skipped += 1;
                if failure.ytdlp_blocked {
                    ytdlp_blocked_songs += 1;
                    ytdlp_failure_streak += 1;
                } else {
                    ytdlp_failure_streak = 0;
                }
                let _ = window.emit("download-progress", format!("⚠ Skipped \"{}\": {}", label, failure.reason));
            }
        }

        if ytdlp_failure_streak >= MAX_YTDLP_FAILURE_STREAK {
            break;
        }

        if delay_secs > 0 && i + 1 < total {
            thread::sleep(std::time::Duration::from_secs(delay_secs as u64));
        }
    }

    let _ = fs::remove_dir_all(&staging_dir);

    if ytdlp_failure_streak >= MAX_YTDLP_FAILURE_STREAK {
        let _ = window.emit(
            "download-error",
            outdated_ytdlp_error(
                "spotdl",
                &format!(
                    "Stopped: yt-dlp couldn't download the last {} songs in a row from YouTube ({} of {} songs saved).",
                    ytdlp_failure_streak, verified, total
                ),
            ),
        );
        return;
    }

    if verified == 0 && ytdlp_blocked_songs > 0 {
        let _ = window.emit(
            "download-error",
            outdated_ytdlp_error("spotdl", "yt-dlp couldn't download the audio from YouTube."),
        );
        return;
    }

    if verified == 0 {
        let _ = window.emit(
            "download-error",
            friendly_process_error(
                "Couldn't verify a correct match — every candidate spotdl found didn't match the expected song length/title/artist closely enough.",
            ),
        );
        return;
    }

    let summary = if skipped == 0 {
        "Download complete.".to_string()
    } else {
        format!("Download complete. {} downloaded, {} skipped (couldn't verify a correct match).", verified, skipped)
    };
    let _ = window.emit("download-done", summary);
}

/// yt-dlp's --audio-format doesn't support aiff (only aac/alac/flac/m4a/mp3/opus/vorbis/wav),
/// so AIFF requests download as wav and get converted afterward. --print + --no-quiet keeps
/// normal progress output while also reporting the exact final path of each downloaded file,
/// which is needed to find the right file(s) to convert (there can be more than one for playlists).
fn other_download(window: &tauri::Window, url: &str, format: &str, output_path: &str, delay_secs: u32) {
    let want_aiff = format == "aiff";
    let yt_format = if want_aiff { "wav" } else { "mp3" };

    let mut cmd = Command::new("python");
    cmd.arg("-m").arg("yt_dlp").arg("-x").arg("--no-overwrites");
    cmd.arg("--audio-format").arg(yt_format);
    if want_aiff {
        cmd.arg("--print").arg("after_move:filepath").arg("--no-quiet");
    } else {
        cmd.arg("--audio-quality").arg("0");
    }
    if delay_secs > 0 {
        cmd.arg("--sleep-interval")
            .arg(delay_secs.to_string())
            .arg("--max-sleep-interval")
            .arg((delay_secs * 2).to_string());
    }
    cmd.arg("-o").arg(format!("{}/%(title)s.%(ext)s", output_path)).arg(url);

    match run_and_stream(window, cmd, "python") {
        Ok((status, stdout_lines, _)) if status.success() => {
            if want_aiff {
                let wav_files: Vec<PathBuf> = stdout_lines
                    .iter()
                    .map(|l| PathBuf::from(l.trim()))
                    .filter(|p| p.extension().map(|e| e == "wav").unwrap_or(false) && p.exists())
                    .collect();

                let mut failed = Vec::new();
                for wav in &wav_files {
                    let aiff_path = wav.with_extension("aiff");
                    let converted = Command::new("ffmpeg")
                        .arg("-y")
                        .arg("-i")
                        .arg(wav)
                        .arg(&aiff_path)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    match converted {
                        Ok(s) if s.success() => {
                            let _ = fs::remove_file(wav);
                        }
                        _ => failed.push(wav.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string()),
                    }
                }

                if !failed.is_empty() {
                    let _ = window.emit(
                        "download-error",
                        friendly_process_error(&format!(
                            "Downloaded but couldn't convert to AIFF: {}",
                            failed.join(", ")
                        )),
                    );
                    return;
                }
            }
            let _ = window.emit("download-done", "Download complete.");
        }
        Ok((status, stdout_lines, stderr_lines)) => {
            let what = format!("yt-dlp exited with {}.", status);
            let message = if has_ytdlp_download_error(&stdout_lines) || has_ytdlp_download_error(&stderr_lines) {
                outdated_ytdlp_error("python", &what)
            } else {
                friendly_process_error(&what)
            };
            let _ = window.emit("download-error", message);
        }
        Err(msg) => {
            let _ = window.emit("download-error", msg);
        }
    }
}

#[tauri::command]
fn download(window: tauri::Window, url: String, format: String, output_path: String, delay_secs: u32) {
    thread::spawn(move || {
        let source = detect_source(&url);
        if source == "spotify" {
            spotify_download(&window, &url, &format, &output_path, delay_secs);
        } else {
            other_download(&window, &url, &format, &output_path, delay_secs);
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            #[cfg(desktop)]
            app.handle().plugin(tauri_plugin_updater::Builder::new().build())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![detect_source, download])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn detects_spotdl_download_error_output() {
        let output = lines(&[
            "Processing query: https://open.spotify.com/track/0DiWol3AO6WpXZgp0goxAV        ",
            "AudioProviderError: YT-DLP download error -                                    ",
            "https://www.youtube.com/watch?v=FGBhQbmPwH8                                    ",
        ]);
        assert!(has_ytdlp_download_error(&output));
    }

    #[test]
    fn detects_ytdlp_403_output() {
        let output = lines(&["ERROR: unable to download video data: HTTP Error 403: Forbidden"]);
        assert!(has_ytdlp_download_error(&output));
    }

    #[test]
    fn ignores_successful_download_output() {
        let output = lines(&[
            "Downloaded \"Daft Punk - One More Time\":                                      ",
            "https://www.youtube.com/watch?v=FGBhQbmPwH8                                    ",
        ]);
        assert!(!has_ytdlp_download_error(&output));
    }
}
