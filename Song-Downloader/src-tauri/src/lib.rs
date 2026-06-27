use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use tauri::Emitter;

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

#[tauri::command]
fn download(window: tauri::Window, url: String, format: String, output_path: String) {
    thread::spawn(move || {
        let source = detect_source(&url);

        let mut cmd = if source == "spotify" {
            let mut c = Command::new("spotdl");
            c.arg("--format").arg(&format)
                .arg("--output").arg(&output_path)
                .arg(&url);
            c
        } else {
            let mut c = Command::new("python");
            c.arg("-m").arg("yt_dlp").arg("-x").arg("--no-overwrites");
            if format == "aiff" {
                c.arg("--audio-format").arg("aiff");
            } else {
                c.arg("--audio-format").arg("mp3")
                    .arg("--audio-quality").arg("0");
            }
            c.arg("-o")
                .arg(format!("{}/%(title)s.%(ext)s", output_path))
                .arg(&url);
            c
        };

        let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = window.emit("download-error", format!("Failed to start process: {}", e));
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let window_stderr = window.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if let Ok(line) = line {
                    let _ = window_stderr.emit("download-progress", line);
                }
            }
        });

        for line in BufReader::new(stdout).lines() {
            if let Ok(line) = line {
                let _ = window.emit("download-progress", line);
            }
        }

        match child.wait() {
            Ok(status) if status.success() => {
                let _ = window.emit("download-done", "Download complete.");
            }
            Ok(status) => {
                let _ = window.emit("download-error", format!("Process exited with: {}", status));
            }
            Err(e) => {
                let _ = window.emit("download-error", format!("Process error: {}", e));
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![detect_source, download])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
