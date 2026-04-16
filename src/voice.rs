/// Voice input — audio capture + transcription.
///
/// Recording: uses system `arecord` (Linux) or `sox` if available.
/// Transcription: tries in priority order:
///   1. Local `whisper` CLI (OpenAI whisper or whisper.cpp)
///   2. OpenAI-compatible /v1/audio/transcriptions API endpoint
///      (reads OPENAI_API_KEY or WHISPER_API_KEY from env)
///
/// Usage:
///   /voice          — show status + setup instructions
///   /voice enable   — enable voice mode
///   /voice disable  — disable voice mode
///   Ctrl+R          — while voice mode is on: start/stop recording
///
/// The transcribed text is inserted directly into the input buffer.
use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

// ── Availability checks ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum RecorderBackend {
    Arecord,
    Sox,
    Ffmpeg,
}

pub fn find_recorder() -> Option<RecorderBackend> {
    if which("arecord") {
        Some(RecorderBackend::Arecord)
    } else if which("sox") {
        Some(RecorderBackend::Sox)
    } else if which("ffmpeg") {
        Some(RecorderBackend::Ffmpeg)
    } else {
        None
    }
}

pub fn local_whisper_available() -> bool {
    which("whisper") || which("whisper-cpp") || which("whisper.cpp")
}

pub fn voice_api_key() -> Option<String> {
    std::env::var("OPENAI_API_KEY")
        .ok()
        .or_else(|| std::env::var("WHISPER_API_KEY").ok())
}

fn which(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Temp file path ────────────────────────────────────────────────────────────

/// Returns the path for the voice recording WAV file.
///
/// IMPORTANT: Always use `std::env::temp_dir()` here, never hardcode `/tmp`.
/// On many systems (custom TMPDIR, NixOS, Arch with TMPDIR on separate partition,
/// macOS which uses /var/folders/..., etc.) the real temp dir is NOT /tmp.
/// Using the wrong dir means the recorder writes the WAV somewhere whisper
/// never finds it, producing a "Failed to load audio" error.
pub fn temp_wav_path() -> PathBuf {
    std::env::temp_dir().join("rustyclaw-voice.wav")
}

// ── Recording ─────────────────────────────────────────────────────────────────

/// Spawn the recorder process. Returns the child process handle.
/// The caller is responsible for killing it when recording should stop.
pub async fn start_recording(backend: &RecorderBackend) -> Result<tokio::process::Child> {
    let out = temp_wav_path();
    // Clean up any previous recording
    let _ = tokio::fs::remove_file(&out).await;

    let child = match backend {
        RecorderBackend::Arecord => {
            Command::new("arecord")
                .args([
                    "-f",
                    "S16_LE", // 16-bit signed little-endian
                    "-r",
                    "16000", // 16kHz (Whisper optimal)
                    "-c",
                    "1", // mono
                    "-t",
                    "wav",
                    &out.display().to_string(),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?
        }
        RecorderBackend::Sox => {
            Command::new("sox")
                .args([
                    "-d", // default audio device
                    "-r",
                    "16000",
                    "-c",
                    "1",
                    "-b",
                    "16",
                    &out.display().to_string(),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?
        }
        RecorderBackend::Ffmpeg => Command::new("ffmpeg")
            .args([
                "-f",
                "alsa",
                "-i",
                "default",
                "-ar",
                "16000",
                "-ac",
                "1",
                "-y",
                &out.display().to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    };
    Ok(child)
}

// ── Transcription ─────────────────────────────────────────────────────────────

/// Transcribe the recorded WAV file. Returns the transcribed text.
pub async fn transcribe(api_url: Option<&str>, api_key: Option<&str>) -> Result<String> {
    let wav = temp_wav_path();
    if !wav.exists() {
        return Err(anyhow!("No recording found at {}", wav.display()));
    }

    // Try local whisper first (no API key needed, works offline)
    if local_whisper_available() {
        return transcribe_local(&wav).await;
    }

    // Fall back to OpenAI-compatible API
    let key = api_key
        .map(|s| s.to_string())
        .or_else(voice_api_key)
        .ok_or_else(|| {
            anyhow!(
                "No transcription available.\n\
             Set OPENAI_API_KEY or WHISPER_API_KEY env var,\n\
             or install whisper: pip install openai-whisper"
            )
        })?;

    let url = api_url.unwrap_or("https://api.openai.com/v1/audio/transcriptions");
    transcribe_api(&wav, url, &key).await
}

async fn transcribe_local(wav: &std::path::Path) -> Result<String> {
    // Try whisper CLI tools in order
    for binary in &["whisper", "whisper-cpp", "whisper.cpp"] {
        if which(binary) {
            // Use std::env::temp_dir() — never hardcode /tmp.
            // TMPDIR can be /mnt/Storage/tmp, /var/folders/..., or any custom path.
            // The --output_dir passed to whisper MUST match so we can find the .txt output.
            let tmp_dir = std::env::temp_dir();
            let tmp_dir_str = tmp_dir.to_string_lossy();
            let out = Command::new(binary)
                .args([
                    &wav.display().to_string(),
                    "--model",
                    "base",
                    "--output_format",
                    "txt",
                    "--fp16",
                    "False",
                    "--output_dir",
                    tmp_dir_str.as_ref(),
                ])
                .output()
                .await?;

            if out.status.success() {
                // whisper writes <filename>.txt in output_dir
                let txt_path = tmp_dir
                    .join(wav.file_stem().unwrap_or_default())
                    .with_extension("txt");
                if let Ok(text) = tokio::fs::read_to_string(&txt_path).await {
                    let _ = tokio::fs::remove_file(&txt_path).await;
                    return Ok(text.trim().to_string());
                }
                // Some versions print to stdout
                return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
            }
        }
    }
    Err(anyhow!("Local whisper transcription failed"))
}

async fn transcribe_api(wav: &std::path::Path, url: &str, api_key: &str) -> Result<String> {
    let wav_bytes = tokio::fs::read(wav).await?;
    let filename = wav
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.wav")
        .to_string();

    let part = reqwest::multipart::Part::bytes(wav_bytes)
        .file_name(filename)
        .mime_str("audio/wav")?;
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", "whisper-1")
        .text("response_format", "text");

    let client = reqwest::Client::new();
    let resp = client
        .post(url)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Transcription API error {}: {}", status, body));
    }

    Ok(resp.text().await?.trim().to_string())
}

// ── Text-to-Speech ───────────────────────────────────────────────────────────

pub fn audio_player_available() -> bool {
    which("aplay") || which("paplay") || which("mpv") || which("ffplay") || which("play")
}

/// Find all installed XTTS v2 voice clone samples.
/// Returns Vec of (display_name, full_path) sorted alphabetically by name.
pub fn find_all_voices() -> Vec<(String, String)> {
    let mut voices = Vec::new();
    // Check for voice clone sample
    if let Some(clone_path) = voice_clone_sample_path()
        && clone_path.exists()
    {
        let tier = detect_clone_tier(&clone_path);
        voices.push((
            format!("Your voice ({tier} tier)"),
            clone_path.display().to_string(),
        ));
    }
    // Default XTTS v2 speaker
    if xtts_available() {
        voices.push((
            format!("XTTS v2 default ({XTTS_DEFAULT_SPEAKER})"),
            XTTS_DEFAULT_SPEAKER.to_string(),
        ));
    }
    voices
}

/// Strip markdown so text reads naturally aloud.
fn strip_for_speech(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code_block = false;

    for line in text.lines() {
        let trimmed = line.trim_start();

        // Toggle fenced code block — skip content inside
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }

        // Strip heading markers
        let line = trimmed.trim_start_matches('#').trim_start();

        // Strip leading list markers (-, *, +, or "1. 2." etc.)
        let line = if let Some(rest) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            rest
        } else {
            // numbered list: "1. " "12. "
            let maybe = line.trim_start_matches(|c: char| c.is_ascii_digit());
            if maybe.starts_with(". ") {
                maybe.trim_start_matches(". ")
            } else {
                line
            }
        };

        // Strip block quotes
        let line = line.strip_prefix("> ").unwrap_or(line);

        let line = strip_inline_md(line);
        let line = line.trim();
        if !line.is_empty() {
            out.push_str(line);
            out.push(' ');
        }
    }
    out.trim().to_string()
}

fn strip_inline_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                // Skip inline code
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    i += 1;
                }
            }
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 1; // skip second *
            }
            '_' if i + 1 < chars.len() && chars[i + 1] == '_' => {
                i += 1; // skip second _
            }
            '*' | '_' => { /* skip single marker */ }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// Maximum words spoken per response — keeps TTS under ~90 seconds.
pub const TTS_WORD_LIMIT: usize = 200;

/// Default XTTS v2 speaker when no voice clone is configured.
/// "Daisy Studious" — clear, warm female voice from the XTTS v2 multi-dataset model.
pub const XTTS_DEFAULT_SPEAKER: &str = "Daisy Studious";

/// Port for the XTTS v2 background server.
const XTTS_SERVER_PORT: u16 = 5002;

// ── CUDA detection ───────────────────────────────────────────────────────────

/// Check if an NVIDIA GPU with CUDA is available.
pub fn cuda_available() -> bool {
    which("nvidia-smi")
}

// ── XTTS v2 server lifecycle ─────────────────────────────────────────────────

/// Find the xtts-server.py script bundled with RustyClaw.
fn xtts_server_script() -> Option<PathBuf> {
    // Check relative to the running binary
    if let Ok(exe) = std::env::current_exe() {
        let exe_dir = exe.parent()?;
        // release binary: target/release/rustyclaw → ../../scripts/
        for candidate in &[
            exe_dir.join("../../scripts/xtts-server.py"),
            exe_dir.join("../scripts/xtts-server.py"),
            exe_dir.join("scripts/xtts-server.py"),
        ] {
            if candidate.exists() {
                return candidate.canonicalize().ok();
            }
        }
    }
    // Check relative to CWD
    let cwd_script = PathBuf::from("scripts/xtts-server.py");
    if cwd_script.exists() {
        return cwd_script.canonicalize().ok();
    }
    None
}

/// Find the Python interpreter inside the TTS uv tool venv.
fn tts_python() -> Option<String> {
    // uv tool installs to ~/.local/share/uv/tools/tts/bin/python
    if let Some(home) = dirs::home_dir() {
        let uv_python = home.join(".local/share/uv/tools/tts/bin/python");
        if uv_python.exists() {
            return Some(uv_python.display().to_string());
        }
    }
    // Fallback: python3.11 in PATH
    if which("python3.11") {
        return Some("python3.11".into());
    }
    None
}

/// Check if the XTTS v2 server is already running.
pub fn xtts_server_running() -> bool {
    std::net::TcpStream::connect(format!("127.0.0.1:{XTTS_SERVER_PORT}")).is_ok()
}

/// Start the XTTS v2 background server if not already running.
/// Returns Ok(port) on success. The server process is detached and persists
/// until RustyClaw exits or /voice speak off is called.
pub async fn ensure_xtts_server() -> Result<u16> {
    if xtts_server_running() {
        return Ok(XTTS_SERVER_PORT);
    }

    let script = xtts_server_script().ok_or_else(|| {
        anyhow!("xtts-server.py not found. Rebuild RustyClaw or check scripts/ dir.")
    })?;
    let python = tts_python()
        .ok_or_else(|| anyhow!("No Python for TTS venv. Run: uv tool install TTS --python 3.11"))?;

    let mut args = vec![script.display().to_string(), XTTS_SERVER_PORT.to_string()];
    if !cuda_available() {
        args.push("--cpu".into());
    }

    // Spawn detached server process
    let _child = std::process::Command::new(&python)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("Failed to start XTTS v2 server: {e}"))?;

    // Wait for server to become ready (up to 60s for model loading)
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if xtts_server_running() {
            return Ok(XTTS_SERVER_PORT);
        }
    }

    Err(anyhow!("XTTS v2 server failed to start within 60 seconds"))
}

/// Stop the XTTS v2 server if running.
pub fn stop_xtts_server() {
    let _ = std::process::Command::new("sh")
        .args([
            "-c",
            &format!("kill $(lsof -ti:{XTTS_SERVER_PORT}) 2>/dev/null"),
        ])
        .output();
}

// ── Server-based synthesis ───────────────────────────────────────────────────

/// Synthesise via the XTTS v2 server (fast — model stays loaded in GPU VRAM).
async fn speak_via_server(text: &str, stop_rx: tokio::sync::oneshot::Receiver<()>) -> Result<bool> {
    let clean = strip_for_speech(text);
    if clean.is_empty() {
        return Ok(false);
    }

    let words: Vec<&str> = clean.split_whitespace().collect();
    let truncated = words.len() > TTS_WORD_LIMIT;
    let speech_text = if truncated {
        words[..TTS_WORD_LIMIT].join(" ") + ". Response trimmed."
    } else {
        clean
    };

    // Build JSON payload
    let clone_path = voice_clone_sample_path().filter(|p| p.exists());
    let body = if let Some(ref wav) = clone_path {
        format!(
            r#"{{"text":"{}","speaker_wav":"{}","language":"en"}}"#,
            speech_text.replace('\\', "\\\\").replace('"', "\\\""),
            wav.display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\""),
        )
    } else {
        format!(
            r#"{{"text":"{}","speaker":"{}","language":"en"}}"#,
            speech_text.replace('\\', "\\\\").replace('"', "\\\""),
            XTTS_DEFAULT_SPEAKER,
        )
    };

    let wav_out = std::env::temp_dir().join("rustyclaw-xtts-server.wav");
    tokio::pin!(stop_rx);

    // HTTP POST to server
    let mut curl = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            &format!("http://127.0.0.1:{XTTS_SERVER_PORT}/tts"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            "--output",
            &wav_out.display().to_string(),
            "--max-time",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    tokio::select! {
        biased;
        _ = &mut stop_rx => {
            let _ = curl.kill().await;
            let _ = tokio::fs::remove_file(&wav_out).await;
            return Ok(truncated);
        }
        status = curl.wait() => {
            if !status?.success() {
                return Err(anyhow!("XTTS v2 server request failed"));
            }
        }
    }

    // Verify we got a real WAV (not an error page)
    let meta = tokio::fs::metadata(&wav_out).await?;
    if meta.len() < 1000 {
        let _ = tokio::fs::remove_file(&wav_out).await;
        return Err(anyhow!("XTTS v2 server returned invalid audio"));
    }

    play_wav(&wav_out, stop_rx).await?;
    Ok(truncated)
}

/// Synthesise via XTTS v2 server using ONLY the default speaker (no clone).
async fn speak_via_server_default(
    text: &str,
    stop_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<bool> {
    let clean = strip_for_speech(text);
    if clean.is_empty() {
        return Ok(false);
    }

    let words: Vec<&str> = clean.split_whitespace().collect();
    let truncated = words.len() > TTS_WORD_LIMIT;
    let speech_text = if truncated {
        words[..TTS_WORD_LIMIT].join(" ") + ". Response trimmed."
    } else {
        clean
    };

    let body = format!(
        r#"{{"text":"{}","speaker":"{}","language":"en"}}"#,
        speech_text.replace('\\', "\\\\").replace('"', "\\\""),
        XTTS_DEFAULT_SPEAKER,
    );

    let wav_out = std::env::temp_dir().join("rustyclaw-xtts-test.wav");
    tokio::pin!(stop_rx);

    let mut curl = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            &format!("http://127.0.0.1:{XTTS_SERVER_PORT}/tts"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            "--output",
            &wav_out.display().to_string(),
            "--max-time",
            "30",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    tokio::select! {
        biased;
        _ = &mut stop_rx => {
            let _ = curl.kill().await;
            let _ = tokio::fs::remove_file(&wav_out).await;
            return Ok(truncated);
        }
        status = curl.wait() => {
            if !status?.success() {
                return Err(anyhow!("XTTS v2 server request failed"));
            }
        }
    }

    let meta = tokio::fs::metadata(&wav_out).await?;
    if meta.len() < 1000 {
        let _ = tokio::fs::remove_file(&wav_out).await;
        return Err(anyhow!("XTTS v2 server returned invalid audio"));
    }

    play_wav(&wav_out, stop_rx).await?;
    Ok(truncated)
}

/// Synthesise `text` and play it via XTTS v2.
///
/// Priority: XTTS v2 server (GPU, fast) → XTTS v2 CLI.
/// `stop_rx` — send () to abort mid-synthesis or mid-playback.
/// Returns `Ok(true)` if truncated (hit word limit), `Ok(false)` if complete, `Err` on failure.
pub async fn speak(
    text: &str,
    _voice_model: Option<&str>,
    stop_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<bool> {
    // ── Try XTTS v2 server first (fastest — model pre-loaded in VRAM) ──────
    if xtts_server_running() {
        return speak_via_server(text, stop_rx).await;
    }

    // ── XTTS v2 CLI fallback (cold start each call) ───────────────────────
    if xtts_available() {
        if let Some(clone_path) = voice_clone_sample_path()
            && clone_path.exists()
        {
            return speak_cloned(text, &clone_path, stop_rx).await;
        }
        return speak_xtts_default(text, stop_rx).await;
    }

    Err(anyhow!(
        "No TTS engine found.\n\n\
         Install XTTS v2:\n  \
         uv tool install TTS --python 3.11 \\\n    \
         --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'"
    ))
}

/// Synthesise using only the default speaker — ignores any voice clone.
/// Used by `/voice test` so the demo is always clean and consistent.
pub async fn speak_default_only(
    text: &str,
    stop_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<bool> {
    if xtts_server_running() {
        return speak_via_server_default(text, stop_rx).await;
    }
    if xtts_available() {
        return speak_xtts_default(text, stop_rx).await;
    }
    Err(anyhow!(
        "No TTS engine found.\n\n\
         Install XTTS v2:\n  \
         uv tool install TTS --python 3.11 \\\n    \
         --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'"
    ))
}

/// Synthesise via XTTS v2 using a built-in default speaker (no clone needed).
async fn speak_xtts_default(
    text: &str,
    stop_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<bool> {
    let clean = strip_for_speech(text);
    if clean.is_empty() {
        return Ok(false);
    }

    let words: Vec<&str> = clean.split_whitespace().collect();
    let truncated = words.len() > TTS_WORD_LIMIT;
    let speech_text = if truncated {
        words[..TTS_WORD_LIMIT].join(" ") + ". Response trimmed."
    } else {
        clean
    };

    let wav_out = std::env::temp_dir().join("rustyclaw-xtts-default.wav");
    let wav_out_str = wav_out.display().to_string();
    tokio::pin!(stop_rx);

    let mut cli_args = vec![
        "--model_name",
        "tts_models/multilingual/multi-dataset/xtts_v2",
        "--speaker_idx",
        XTTS_DEFAULT_SPEAKER,
        "--language_idx",
        "en",
        "--out_path",
        &wav_out_str,
        "--text",
        &speech_text,
    ];
    if cuda_available() {
        cli_args.extend(["--use_cuda", "true"]);
    }
    let mut tts_proc = Command::new("tts")
        .args(&cli_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    tokio::select! {
        biased;
        _ = &mut stop_rx => {
            let _ = tts_proc.kill().await;
            let _ = tokio::fs::remove_file(&wav_out).await;
            return Ok(truncated);
        }
        status = tts_proc.wait() => {
            if !status?.success() {
                return Err(anyhow!(
                    "XTTS v2 synthesis failed.\n\
                     Install:  uv tool install TTS --python 3.11 --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'"
                ));
            }
        }
    }

    play_wav(&wav_out, stop_rx).await?;
    Ok(truncated)
}

/// Play a WAV file through the first available audio player, then clean up.
async fn play_wav(
    wav_path: &std::path::Path,
    mut stop_rx: std::pin::Pin<&mut tokio::sync::oneshot::Receiver<()>>,
) -> Result<()> {
    let path_str = wav_path.display().to_string();
    let players: &[(&str, &[&str])] = &[
        ("aplay", &["-q"]),
        ("paplay", &[]),
        ("mpv", &["--really-quiet", "--no-video"]),
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
        ("play", &["-q"]),
    ];
    let mut played = false;
    for (player, args) in players {
        if which(player) {
            let mut child = Command::new(player)
                .args(*args)
                .arg(&path_str)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            tokio::select! {
                biased;
                _ = stop_rx.as_mut() => { let _ = child.kill().await; }
                _ = child.wait() => {}
            }
            played = true;
            break;
        }
    }
    let _ = tokio::fs::remove_file(wav_path).await;

    if !played {
        return Err(anyhow!(
            "No audio player found.\n\
             Install:  sudo pacman -S alsa-utils   # provides aplay\n\
             Or:       sudo pacman -S mpv"
        ));
    }
    Ok(())
}

// ── Browse prefix routing ─────────────────────────────────────────────────────

/// Decide whether a voice transcript should enter autonomous browse mode.
/// Only unambiguous prefixes route to /browse; "find" is deliberately excluded
/// to avoid collisions with codebase/chat "find" intent.
pub fn voice_routes_to_browse(transcript: &str) -> bool {
    let t = transcript.trim().to_lowercase();
    t.starts_with("browse ")
        || t.starts_with("browser ")
        || t.starts_with("web ")
        || t.starts_with("go to ")
        || t.starts_with("open ")
        || t.starts_with("shop for ")
        || t.starts_with("book ")
        || t.starts_with("order ")
}

/// Strip the browse prefix, leaving the goal text.
pub fn strip_browse_prefix(transcript: &str) -> String {
    let t = transcript.trim();
    let lower = t.to_lowercase();
    for prefix in &[
        "browse ", "browser ", "web ", "go to ", "open ", "shop for ", "book ", "order ",
    ] {
        if lower.starts_with(prefix) {
            return t[prefix.len()..].to_string();
        }
    }
    t.to_string()
}

// ── Browse milestone TTS ──────────────────────────────────────────────────────

pub enum BrowseMilestone {
    Start,
    GateTrip,
    End,
}

/// Speak one of the three browse milestones via TTS.
/// Only called when voice == true. Fire-and-forget: errors are silently ignored
/// so a missing TTS engine never blocks the browse loop.
pub async fn speak_browse_milestone(milestone: BrowseMilestone, text: &str) {
    let phrase = match milestone {
        BrowseMilestone::Start => format!("Searching for {text}"),
        BrowseMilestone::GateTrip => text.to_string(),
        BrowseMilestone::End => text.to_string(),
    };
    // Create a dummy stop channel — milestone phrases are short; we never need
    // to cancel them mid-word.
    let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let _ = speak(&phrase, None, stop_rx).await;
}

// ── Voice approval listener ───────────────────────────────────────────────────

/// Listen for a voice approve/deny reply during an approval prompt.
/// Returns true if the user said "confirm"/"yes"/"approve"/"ok", false otherwise.
/// Times out after `timeout_secs`, returning false on timeout.
pub async fn await_voice_approval(timeout_secs: u64) -> bool {
    // Requires a recorder to be available; return deny if none found.
    let backend = match find_recorder() {
        Some(b) => b,
        None => return false,
    };

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    // Record for at most timeout_secs then stop automatically.
    let record_task = tokio::spawn(async move {
        match start_recording(&backend).await {
            Ok(mut child) => {
                tokio::select! {
                    _ = stop_rx => {
                        if let Some(pid) = child.id() {
                            let _ = tokio::process::Command::new("kill")
                                .args(["-2", &pid.to_string()])
                                .status()
                                .await;
                        }
                        let _ = child.wait().await;
                    }
                    _ = child.wait() => {}
                }
            }
            Err(_) => {}
        }
    });

    // Wait for the timeout then signal the recorder to stop.
    tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
    let _ = stop_tx.send(());
    let _ = record_task.await;

    // Transcribe with a 30s timeout and check for affirmative keywords.
    let transcribe_result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        transcribe(None, None),
    )
    .await;
    let transcript = match transcribe_result {
        Ok(Ok(text)) => text,
        _ => return false, // timeout or transcription error = deny
    };
    let lower = transcript.trim().to_lowercase();
    lower.contains("confirm")
        || lower.contains("yes")
        || lower.contains("approve")
        || lower.contains("ok")
}

// ── Status display ────────────────────────────────────────────────────────────

pub fn voice_status(enabled: bool, tts_enabled: bool) -> String {
    let recorder = find_recorder();
    let rec_status = match &recorder {
        Some(RecorderBackend::Arecord) => "✓ arecord",
        Some(RecorderBackend::Sox) => "✓ sox",
        Some(RecorderBackend::Ffmpeg) => "✓ ffmpeg",
        None => "✗ no recorder (install: sudo pacman -S alsa-utils)",
    };

    let whisper = if local_whisper_available() {
        "✓ local whisper (offline)"
    } else {
        "✗ whisper not installed (pipx install openai-whisper)"
    };

    let api_key_status = match voice_api_key() {
        Some(_) => "✓ API key found".to_string(),
        None => "✗ no API key — add OPENAI_API_KEY=sk-... to ~/.env".to_string(),
    };

    let transcription_ok = local_whisper_available() || voice_api_key().is_some();
    let recorder_ok = recorder.is_some();

    let input_overall = if enabled {
        if recorder_ok && transcription_ok {
            "ENABLED  ● Ready — press Ctrl+R to record"
        } else {
            "ENABLED  ⚠ Setup incomplete (see below)"
        }
    } else {
        "DISABLED"
    };

    // ── TTS status (XTTS v2 only) ──────────────────────────────────────────
    let xtts_ok = xtts_available();
    let player_ok = audio_player_available();
    let tts_overall = if tts_enabled { "ENABLED" } else { "DISABLED" };

    let server_up = xtts_server_running();
    let gpu = cuda_available();
    let tts_engine = if xtts_ok && server_up {
        if gpu {
            "✓ XTTS v2 server running (GPU — fast)"
        } else {
            "✓ XTTS v2 server running (CPU)"
        }
    } else if xtts_ok {
        "✓ XTTS v2 available (server will auto-start on use)"
    } else {
        "✗ XTTS v2 not installed"
    };

    // Voice clone status. Race-safe: if the sample file disappears between
    // voice_clone_sample_path() returning Some and the tier detection, we
    // treat it the same as "no clone" rather than panicking in a doctor path.
    let clone_path = voice_clone_sample_path();
    let clone_status = match clone_path.as_ref().filter(|p| p.exists()) {
        Some(p) => {
            let tier = detect_clone_tier(p);
            format!("✓ your voice ({tier} tier)")
        }
        None if xtts_ok => format!("default speaker ({XTTS_DEFAULT_SPEAKER})"),
        None => "— (requires XTTS v2)".to_string(),
    };

    let tts_ready = xtts_ok;
    let all_input_ok = recorder_ok && transcription_ok;

    let mut out = format!(
        "Voice Input  {input_overall}\n\
         \n\
         Audio capture:      {rec_status}\n\
         Transcription:      {whisper}\n\
         API key:            {api_key_status}\n\
         \n\
         TTS output  {tts_overall}\n\
         \n\
         Engine:             {tts_engine}\n\
         Voice:              {clone_status}\n\
         Audio player:       {}\n\
         \n\
         Commands:\n\
           /voice enable         — enable voice input (Ctrl+R to record)\n\
           /voice disable        — disable voice input\n\
           /voice speak on|off   — enable/disable TTS output\n\
           /voice test           — play a test phrase\n\
           /voice clone          — record a custom voice for TTS\n\
           /voice clone remove   — revert to default speaker\n\
           Ctrl+R                — start/stop recording (when enabled)",
        if player_ok {
            "✓ available"
        } else {
            "✗ no player (install: sudo pacman -S mpv)"
        },
    );

    // Install instructions for missing components
    if all_input_ok && tts_ready && player_ok {
        out.push_str("\n\n  ✓ All set — voice input and TTS fully configured.");
    } else {
        if !recorder_ok {
            out.push_str(
                "\n\n  Setup needed — audio capture:\n\
                           \n    sudo pacman -S alsa-utils",
            );
        }
        if !transcription_ok {
            out.push_str(
                "\n\n  Setup needed — transcription:\n\
                           \n    pipx install openai-whisper    # offline\n\
                           \n    — or: echo 'OPENAI_API_KEY=sk-...' >> ~/.env",
            );
        }
        if !xtts_ok {
            out.push_str("\n\n  Setup needed — XTTS v2:\n\
                           \n    uv tool install TTS --python 3.11 \\\n\
                             \x20     --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'");
        }
        if !player_ok {
            out.push_str(
                "\n\n  Setup needed — audio player:\n\
                           \n    sudo pacman -S mpv             # or alsa-utils for aplay",
            );
        }
    }

    out
}

// ── Voice cloning via XTTS v2 ────────────────────────────────────────────────

/// Clone quality tiers — determines recording duration and guidance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CloneTier {
    /// 10 seconds — recognizable but synthetic
    Quick,
    /// 60 seconds — natural rhythm, occasional artifacts
    Recommended,
    /// 5+ minutes — near-perfect clone
    Premium,
}

impl CloneTier {
    pub fn label(&self) -> &'static str {
        match self {
            CloneTier::Quick => "quick",
            CloneTier::Recommended => "recommended",
            CloneTier::Premium => "premium",
        }
    }

    pub fn duration_secs(&self) -> u64 {
        match self {
            CloneTier::Quick => 10,
            CloneTier::Recommended => 60,
            CloneTier::Premium => 300,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            CloneTier::Quick => "10 seconds — recognizable you, but clearly synthetic",
            CloneTier::Recommended => "60 seconds — natural rhythm, good quality (guided prompts)",
            CloneTier::Premium => "5+ minutes — near-perfect voice clone",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "quick" | "1" | "10s" => Some(CloneTier::Quick),
            "recommended" | "2" | "60s" => Some(CloneTier::Recommended),
            "premium" | "3" | "5m" => Some(CloneTier::Premium),
            _ => None,
        }
    }
}

impl std::fmt::Display for CloneTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// Check if XTTS v2 is available (Coqui TTS Python package).
pub fn xtts_available() -> bool {
    which("tts")
}

/// Directory where voice clone samples are stored.
pub fn voice_clone_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".local/share/rustyclaw/voice-clone"))
}

/// Path to the active voice clone WAV sample.
pub fn voice_clone_sample_path() -> Option<std::path::PathBuf> {
    voice_clone_dir().map(|d| d.join("my-voice.wav"))
}

/// Detect which tier a recorded sample falls into based on file duration.
fn detect_clone_tier(wav_path: &std::path::Path) -> &'static str {
    // Estimate duration from file size: 16-bit mono 22050 Hz ≈ 44100 bytes/sec
    let size = std::fs::metadata(wav_path).map(|m| m.len()).unwrap_or(0);
    let est_secs = size / 44100;
    if est_secs >= 240 {
        "premium"
    } else if est_secs >= 40 {
        "recommended"
    } else {
        "quick"
    }
}

/// Guided reading prompts for the recommended tier.
/// Designed to exercise varied phonemes, prosody, questions, and exclamations.
pub const GUIDED_PROMPTS: &[&str] = &[
    "The quick brown fox jumps over the lazy dog. Every letter matters when you're building a voice.",
    "How does this function handle edge cases? I think we need to refactor the error path.",
    "That's a great idea! Let me check if the tests pass before we merge this pull request.",
    "The server responded with a five hundred error. We should add retry logic with exponential backoff.",
    "Why would anyone use a linked list here? An array would be so much faster for sequential access.",
    "Perfect. Ship it. The benchmarks look incredible — forty percent faster than the previous version.",
    "I'm not sure about this approach. Could we try something simpler first? Maybe a hash map instead.",
    "Documentation is important, but working code is more important. Let's fix the bug, then write docs.",
];

/// Build instructions text for recording at a given tier.
pub fn recording_instructions(tier: CloneTier) -> String {
    let duration = tier.duration_secs();
    let mut text = format!(
        "Voice Clone Recording — {} tier ({} seconds)\n\n\
         Tips for best quality:\n\
         • Use a quiet room — no background noise, fans, or music\n\
         • Speak at your normal pace and volume\n\
         • Hold your mic 6-12 inches from your mouth\n\
         • Vary your tone naturally — don't read in monotone\n\n",
        tier.label(),
        duration,
    );

    match tier {
        CloneTier::Quick => {
            text.push_str(
                "Read this aloud when recording starts:\n\n\
                 \"The quick brown fox jumps over the lazy dog.\n\
                 Every letter matters when you're building a voice.\"\n\n\
                 Press Ctrl+R to start recording. Press Ctrl+R again to stop after ~10 seconds.",
            );
        }
        CloneTier::Recommended => {
            text.push_str("Read these prompts aloud, one after another:\n\n");
            for (i, prompt) in GUIDED_PROMPTS.iter().enumerate() {
                text.push_str(&format!("  {}. \"{}\"\n\n", i + 1, prompt));
            }
            text.push_str(
                "Press Ctrl+R to start recording. Read all prompts naturally, then press Ctrl+R to stop.",
            );
        }
        CloneTier::Premium => {
            text.push_str(
                "For premium quality, read continuously for 5+ minutes.\n\n\
                 Suggestions:\n\
                 • Read a README or documentation file from this project aloud\n\
                 • Narrate what you're working on — explain your code\n\
                 • Read a blog post or article that interests you\n\
                 • Just talk naturally about anything\n\n\
                 The longer and more varied your speech, the better the clone.\n\n\
                 Press Ctrl+R to start recording. Press Ctrl+R again when done (aim for 5+ minutes).",
            );
        }
    }
    text
}

/// Save a recorded WAV file as the voice clone sample.
/// Copies from the temp recording location to the voice clone directory.
pub async fn save_voice_clone(tier: CloneTier) -> Result<String> {
    let src = temp_wav_path();
    if !src.exists() {
        return Err(anyhow!("No recording found. Record with Ctrl+R first."));
    }

    // Validate minimum duration
    let size = tokio::fs::metadata(&src).await?.len();
    let est_secs = size / 44100; // rough estimate for 16-bit mono 22050Hz
    let min_secs = match tier {
        CloneTier::Quick => 3,
        CloneTier::Recommended => 20,
        CloneTier::Premium => 120,
    };
    if est_secs < min_secs {
        return Err(anyhow!(
            "Recording too short (~{}s). {} tier needs at least {}s. Try again.",
            est_secs,
            tier.label(),
            min_secs,
        ));
    }

    let dest_dir = voice_clone_dir().ok_or_else(|| anyhow!("Cannot determine home directory"))?;
    tokio::fs::create_dir_all(&dest_dir).await?;

    let dest = dest_dir.join("my-voice.wav");
    tokio::fs::copy(&src, &dest).await?;

    // Also save the tier info
    let meta = dest_dir.join("meta.txt");
    tokio::fs::write(&meta, format!("tier={}\nsize={}\n", tier.label(), size)).await?;

    Ok(format!(
        "Voice clone saved ({} tier, ~{}s).\n\
         Location: {}\n\n\
         TTS will now use XTTS v2 with your voice.\n\
         Use /voice clone remove to revert to the default XTTS v2 speaker.",
        tier.label(),
        est_secs,
        dest.display(),
    ))
}

/// Remove the voice clone sample, reverting to XTTS v2 default speaker.
pub async fn remove_voice_clone() -> Result<String> {
    let dir = voice_clone_dir().ok_or_else(|| anyhow!("Cannot determine home directory"))?;
    let sample = dir.join("my-voice.wav");
    if sample.exists() {
        tokio::fs::remove_file(&sample).await?;
        let meta = dir.join("meta.txt");
        let _ = tokio::fs::remove_file(&meta).await;
        Ok(format!(
            "Voice clone removed. TTS reverted to XTTS v2 default speaker ({XTTS_DEFAULT_SPEAKER})."
        ))
    } else {
        Ok("No voice clone configured.".into())
    }
}

/// Synthesise `text` using XTTS v2 with the user's cloned voice.
pub async fn speak_cloned(
    text: &str,
    clone_wav: &std::path::Path,
    stop_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<bool> {
    let clean = strip_for_speech(text);
    if clean.is_empty() {
        return Ok(false);
    }

    let words: Vec<&str> = clean.split_whitespace().collect();
    let truncated = words.len() > TTS_WORD_LIMIT;
    let speech_text = if truncated {
        words[..TTS_WORD_LIMIT].join(" ") + ". Response trimmed."
    } else {
        clean
    };

    let wav_out = std::env::temp_dir().join("rustyclaw-xtts.wav");
    let wav_out_str = wav_out.display().to_string();
    tokio::pin!(stop_rx);

    let clone_str = clone_wav.display().to_string();
    let mut cli_args = vec![
        "--model_name",
        "tts_models/multilingual/multi-dataset/xtts_v2",
        "--speaker_wav",
        &clone_str,
        "--language_idx",
        "en",
        "--out_path",
        &wav_out_str,
        "--text",
        &speech_text,
    ];
    if cuda_available() {
        cli_args.extend(["--use_cuda", "true"]);
    }
    let mut tts_proc = Command::new("tts")
        .args(&cli_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    tokio::select! {
        biased;
        _ = &mut stop_rx => {
            let _ = tts_proc.kill().await;
            let _ = tokio::fs::remove_file(&wav_out).await;
            return Ok(truncated);
        }
        status = tts_proc.wait() => {
            if !status?.success() {
                return Err(anyhow!(
                    "XTTS v2 synthesis failed.\n\
                     Install:  uv tool install TTS --python 3.11 --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'"
                ));
            }
        }
    }

    play_wav(&wav_out, stop_rx).await?;
    Ok(truncated)
}
