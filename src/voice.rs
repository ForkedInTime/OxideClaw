/// Voice input — audio capture + transcription.
///
/// Recording: uses system `arecord` (Linux) or `sox` if available.
/// Transcription: tries in priority order:
///   1. Local `whisper` CLI (OpenAI whisper or whisper.cpp)
///   2. OpenAI-compatible /v1/audio/transcriptions API endpoint
///      (reads WHISPER_API_KEY, else OPENAI_API_KEY, from env)
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

/// The transcription API key and the variable it came from. The
/// voice-specific WHISPER_API_KEY wins: OPENAI_API_KEY used to, so a custom
/// `voiceApiUrl` was sent the user's OpenAI key instead of its own.
pub fn voice_api_key_source() -> Option<(&'static str, String)> {
    pick_voice_api_key(|name| std::env::var(name).ok())
}

fn pick_voice_api_key(get: impl Fn(&str) -> Option<String>) -> Option<(&'static str, String)> {
    ["WHISPER_API_KEY", "OPENAI_API_KEY"]
        .into_iter()
        .find_map(|name| get(name).filter(|v| !v.is_empty()).map(|v| (name, v)))
}

pub fn voice_api_key() -> Option<String> {
    voice_api_key_source().map(|(_, key)| key)
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
    scratch_path("voice", "wav")
}

/// A per-process scratch file under the temp dir. Fixed names like
/// `oxideclaw-voice.wav` were shared by every OxideClaw on the machine
/// (two sessions clobbered each other's audio) and, in a world-writable
/// temp dir, are the classic pre-created-symlink target.
pub fn scratch_path(stem: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("oxideclaw-{stem}-{}.{ext}", std::process::id()))
}

/// JSON body for the XTTS server. Built with serde so newlines, tabs and
/// control characters in model output are escaped — the hand-rolled
/// escaping only handled `\\` and `"`, and multi-line replies produced
/// invalid JSON that the server rejected (TTS silently went quiet).
fn tts_request_body(text: &str, speaker_wav: Option<&std::path::Path>) -> String {
    let body = match speaker_wav {
        Some(wav) => serde_json::json!({
            "text": text,
            "speaker_wav": wav.display().to_string(),
            "language": "en",
        }),
        None => serde_json::json!({
            "text": text,
            "speaker": XTTS_DEFAULT_SPEAKER,
            "language": "en",
        }),
    };
    body.to_string()
}

// ── Recording ─────────────────────────────────────────────────────────────────

/// Spawn the recorder process. Returns the child process handle.
/// The caller is responsible for killing it when recording should stop.
/// Dropping the child kills it: quitting mid-recording drops the recorder
/// task with the runtime, and without that the mic kept recording to the
/// temp WAV after OxideClaw exited.
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
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
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
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
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
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
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
             Set WHISPER_API_KEY or OPENAI_API_KEY env var,\n\
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

    // A transcription is a short upload; never let a silent connection hang
    // the voice loop.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
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

/// Coqui asks for the XTTS v2 (CPML) license with `input()` the first time
/// it downloads the model. OxideClaw gives its helpers no stdin, so that
/// prompt fails instead of reading the TUI's keystrokes, and the user has to
/// answer it once in a normal shell.
pub const XTTS_FIRST_RUN_HINT: &str = "First run: download the XTTS v2 model and accept its CPML license once in a normal shell:\n  \
     tts --model_name tts_models/multilingual/multi-dataset/xtts_v2 --text hi \
     --speaker_idx 'Daisy Studious' --language_idx en --out_path /tmp/xtts-check.wav";

/// Port for the XTTS v2 background server.
const XTTS_SERVER_PORT: u16 = 5002;

// ── CUDA detection ───────────────────────────────────────────────────────────

/// Check if an NVIDIA GPU with CUDA is available.
pub fn cuda_available() -> bool {
    which("nvidia-smi")
}

// ── XTTS v2 server lifecycle ─────────────────────────────────────────────────

/// The XTTS v2 server, compiled into the binary. It used to be looked up on
/// disk, and the only lookup that matched an installed binary was
/// `./scripts/xtts-server.py` in the current directory, so `/voice speak on`
/// inside a cloned repo ran whatever that repo shipped under that name.
const XTTS_SERVER_PY: &str = include_str!("voice/xtts_server.py");

/// Materialise the embedded server script into `dir` and return its path.
/// `dir` is private to the user (0700) and holds nothing else, because Python
/// puts the script's directory on `sys.path[0]`; a shared directory would let
/// a planted `numpy.py` next to it run instead.
fn install_xtts_server_script(dir: &std::path::Path) -> Result<PathBuf> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir)?;
        // An older or hand-made directory keeps its mode under `create`.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    builder.create(dir)?;

    let script = dir.join("xtts-server.py");
    if std::fs::read_to_string(&script).ok().as_deref() == Some(XTTS_SERVER_PY) {
        return Ok(script);
    }
    // Write-then-rename so a concurrent OxideClaw never runs a half-written file.
    let tmp = dir.join(format!(".xtts-server.py.{}.tmp", std::process::id()));
    std::fs::write(&tmp, XTTS_SERVER_PY)?;
    std::fs::rename(&tmp, &script)?;
    Ok(script)
}

/// Directory holding the server script and its token (0700).
fn xtts_dir() -> PathBuf {
    crate::config::Config::data_dir().join("xtts")
}

/// Path of the XTTS v2 server script, refreshed from the copy embedded in
/// this binary.
fn xtts_server_script() -> Result<PathBuf> {
    install_xtts_server_script(&xtts_dir())
}

const XTTS_TOKEN_FILE: &str = "token";

/// The secret our XTTS servers prove they hold. Kept on disk, not per launch,
/// so a server left running by an earlier session is still recognised.
fn read_xtts_token(dir: &std::path::Path) -> Option<String> {
    let t = std::fs::read_to_string(dir.join(XTTS_TOKEN_FILE)).ok()?;
    let t = t.trim();
    (t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())).then(|| t.to_string())
}

fn load_or_create_xtts_token(dir: &std::path::Path) -> Result<String> {
    if let Some(t) = read_xtts_token(dir) {
        return Ok(t);
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let tmp = dir.join(format!(".{XTTS_TOKEN_FILE}.{}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&tmp)?, token.as_bytes())?;
    std::fs::rename(&tmp, dir.join(XTTS_TOKEN_FILE))?;
    Ok(token)
}

/// What a genuine server answers to `/health?nonce=<nonce>`.
fn xtts_health_proof(token: &str, nonce: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("{token}:{nonce}"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Is the listener on `port` one of our XTTS servers? A bare TCP connect
/// trusted anything on the port, and every reply was then POSTed to it.
/// Blocking but bounded: callers include the TUI thread.
fn probe_xtts_server(port: u16, token: &str) -> bool {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let Ok(mut sock) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300))
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    let _ = sock.set_write_timeout(Some(Duration::from_millis(300)));
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let request = format!("GET /health?nonce={nonce} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    if sock.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < 8192 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || sock.set_read_timeout(Some(left)).is_err() {
            break;
        }
        match sock.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let resp = String::from_utf8_lossy(&buf);
    let ok_status = resp
        .lines()
        .next()
        .is_some_and(|l| l.split_whitespace().nth(1) == Some("200"));
    ok_status && resp.contains(&xtts_health_proof(token, &nonce))
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

/// Check if our XTTS v2 server is running.
pub fn xtts_server_running() -> bool {
    read_xtts_token(&xtts_dir()).is_some_and(|t| probe_xtts_server(XTTS_SERVER_PORT, &t))
}

/// The server this process started. Kept here rather than in a local so a
/// stop can reach it while the model is still loading: the script binds its
/// port only after the 10-60 s load, so until then the lsof sweep finds
/// nothing, and a dropped `std::process::Child` is never killed.
static XTTS_CHILD: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);

/// Bumped by every stop, so a start still waiting on the model learns it was
/// cancelled instead of announcing a server nobody wants.
static XTTS_STOP_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `ensure_xtts_server` was overtaken by a stop while the model was loading.
#[derive(Debug)]
pub struct XttsStartCancelled;

impl std::fmt::Display for XttsStartCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("XTTS v2 server start cancelled")
    }
}

impl std::error::Error for XttsStartCancelled {}

fn lock_child(
    slot: &std::sync::Mutex<Option<std::process::Child>>,
) -> std::sync::MutexGuard<'_, Option<std::process::Child>> {
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start the XTTS v2 background server if not already running.
/// Returns Ok(port) on success. The server persists until OxideClaw exits or
/// /voice speak off is called; a stop during loading makes this return
/// `XttsStartCancelled`.
pub async fn ensure_xtts_server() -> Result<u16> {
    use std::sync::atomic::Ordering;
    let generation = XTTS_STOP_GEN.load(Ordering::SeqCst);
    if xtts_server_running() {
        return Ok(XTTS_SERVER_PORT);
    }

    let script = xtts_server_script()
        .map_err(|e| anyhow!("Could not write the XTTS v2 server script: {e}"))?;
    let token = load_or_create_xtts_token(&xtts_dir())
        .map_err(|e| anyhow!("Could not write the XTTS v2 server token: {e}"))?;
    {
        let mut slot = lock_child(&XTTS_CHILD);
        // A server another call started is still loading: wait for it rather
        // than spawn a second one this slot could not keep track of.
        let loading = slot
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if !loading {
            let addr =
                std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, XTTS_SERVER_PORT));
            if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300))
                .is_ok()
            {
                return Err(anyhow!(
                    "Port {XTTS_SERVER_PORT} is in use by a program that is not OxideClaw's XTTS v2 server \
                     (Coqui's tts-server also defaults to it). If it is an XTTS server left by an older \
                     OxideClaw, /voice speak off stops it."
                ));
            }
            let python = tts_python().ok_or_else(|| {
                anyhow!("No Python for TTS venv. Run: uv tool install TTS --python 3.11")
            })?;

            let mut args = vec![script.display().to_string(), XTTS_SERVER_PORT.to_string()];
            if !cuda_available() {
                args.push("--cpu".into());
            }

            // An inherited stdin would be the TUI's raw-mode tty: Coqui's
            // first-run license prompt would block on it forever and eat the
            // user's keystrokes.
            let child = std::process::Command::new(&python)
                .args(&args)
                // Env, not argv: argv is world-readable through `ps`.
                .env("OXIDECLAW_XTTS_TOKEN", &token)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| anyhow!("Failed to start XTTS v2 server: {e}"))?;
            *slot = Some(child);
        }
    }

    // Up to 60s for model loading.
    await_xtts_ready(
        &XTTS_CHILD,
        120,
        std::time::Duration::from_millis(500),
        xtts_server_running,
        || XTTS_STOP_GEN.load(Ordering::SeqCst) != generation,
    )
    .await?;
    Ok(XTTS_SERVER_PORT)
}

/// Wait for the freshly spawned XTTS server in `slot` to listen. A server
/// that exits first is reported at once rather than after the full timeout,
/// and one that never comes up is killed so repeated `/voice` commands do not
/// pile up stuck Python processes.
async fn await_xtts_ready(
    slot: &std::sync::Mutex<Option<std::process::Child>>,
    attempts: u32,
    interval: std::time::Duration,
    ready: impl Fn() -> bool,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    for _ in 0..attempts {
        tokio::time::sleep(interval).await;
        if cancelled() {
            return Err(XttsStartCancelled.into());
        }
        if ready() {
            return Ok(());
        }
        match lock_child(slot).as_mut() {
            // Another waiter timed out and killed it.
            None => break,
            Some(child) => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(anyhow!(
                        "XTTS v2 server exited before it was ready ({status}).\n{XTTS_FIRST_RUN_HINT}"
                    ));
                }
            }
        }
    }
    if let Some(mut child) = lock_child(slot).take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let secs = (interval * attempts).as_secs();
    Err(anyhow!(
        "XTTS v2 server failed to start within {secs} seconds.\n{XTTS_FIRST_RUN_HINT}"
    ))
}

/// Cancel any start in progress and kill the server held in `slot`.
/// Returns whether a live process was killed.
fn kill_xtts_child(
    slot: &std::sync::Mutex<Option<std::process::Child>>,
    stop_gen: &std::sync::atomic::AtomicU64,
) -> bool {
    // Bump first: a waiter that then finds the slot empty must see the stop.
    stop_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let Some(mut child) = lock_child(slot).take() else {
        return false;
    };
    let alive = matches!(child.try_wait(), Ok(None));
    let _ = child.kill();
    let _ = child.wait();
    alive
}

/// Stop the XTTS v2 server, including one still loading its model. Returns
/// whether a server was actually stopped.
///
/// The lsof sweep is only a fallback for a server this process did not start
/// (an older OxideClaw's, or another session's): it sees nothing until the
/// port is bound, and nothing at all where lsof is missing. Only a process
/// *listening* on the port whose command line names xtts is killed: plain
/// `lsof -ti:PORT` also lists clients and any unrelated server on that port.
pub fn stop_xtts_server() -> bool {
    let ours = kill_xtts_child(&XTTS_CHILD, &XTTS_STOP_GEN);
    let swept = std::process::Command::new("sh")
        .args([
            "-c",
            &format!(
                "for p in $(lsof -ti tcp:{XTTS_SERVER_PORT} -sTCP:LISTEN 2>/dev/null); do \
                   ps -p \"$p\" -o args= 2>/dev/null | grep -qi xtts && kill \"$p\" && echo \"$p\"; \
                 done"
            ),
        ])
        .output()
        .is_ok_and(|o| !o.stdout.trim_ascii().is_empty());
    ours || swept
}

// ── Server-based synthesis ───────────────────────────────────────────────────

/// Synthesise via the XTTS v2 server (fast — model stays loaded in GPU VRAM),
/// with the clone sample if given, else the default speaker. `stop_rx` is
/// borrowed so a failed request can fall back to the CLI with it.
async fn speak_via_server(
    text: &str,
    speaker_wav: Option<&std::path::Path>,
    stop_rx: &mut tokio::sync::oneshot::Receiver<()>,
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

    let body = tts_request_body(&speech_text, speaker_wav);
    let wav_out = scratch_path("xtts-server", "wav");

    let mut curl = Command::new("curl")
        .args([
            "-s",
            // An HTTP error must fail here, not be saved as the "audio".
            "--fail",
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
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    tokio::select! {
        biased;
        _ = &mut *stop_rx => {
            let _ = curl.kill().await;
            let _ = tokio::fs::remove_file(&wav_out).await;
            return Ok(truncated);
        }
        status = curl.wait() => {
            if !status?.success() {
                let _ = tokio::fs::remove_file(&wav_out).await;
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

    play_wav(&wav_out, std::pin::Pin::new(stop_rx)).await?;
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
    let mut stop_rx = stop_rx;
    // ── Try XTTS v2 server first (fastest — model pre-loaded in VRAM) ──────
    if xtts_server_running() {
        let clone = voice_clone_sample_path().filter(|p| p.exists());
        match speak_via_server(text, clone.as_deref(), &mut stop_rx).await {
            Err(_) if xtts_available() => {}
            done => return done,
        }
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
    let mut stop_rx = stop_rx;
    if xtts_server_running() {
        match speak_via_server(text, None, &mut stop_rx).await {
            Err(_) if xtts_available() => {}
            done => return done,
        }
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

    let wav_out = scratch_path("xtts-default", "wav");
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
        .stdin(Stdio::null())
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
                     Install:  uv tool install TTS --python 3.11 --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'\n\
                     {XTTS_FIRST_RUN_HINT}"
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
                .stdin(Stdio::null())
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
        "browse ",
        "browser ",
        "web ",
        "go to ",
        "open ",
        "shop for ",
        "book ",
        "order ",
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
/// Times out after `timeout_secs`, returning false on timeout. `api_url` is
/// the user's `voiceApiUrl`: without it the reply always went to OpenAI, with
/// whatever key was meant for the custom endpoint. `prompt` is what was just
/// spoken to the user, so its echo in the recording is not taken as the reply.
pub async fn await_voice_approval(timeout_secs: u64, api_url: Option<&str>, prompt: &str) -> bool {
    // Requires a recorder to be available; return deny if none found.
    let backend = match find_recorder() {
        Some(b) => b,
        None => return false,
    };

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    // Record for at most timeout_secs then stop automatically.
    let record_task = tokio::spawn(async move {
        if let Ok(mut child) = start_recording(&backend).await {
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
    });

    // Wait for the timeout then signal the recorder to stop.
    tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
    let _ = stop_tx.send(());
    let _ = record_task.await;

    // Transcribe with a 30s timeout and check for affirmative keywords.
    let transcribe_result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        transcribe(api_url, None),
    )
    .await;
    let transcript = match transcribe_result {
        Ok(Ok(text)) => text,
        _ => return false, // timeout or transcription error = deny
    };
    is_spoken_approval(&transcript, prompt)
}

/// A spoken "yes" for a destructive action: an affirmative *word* and no
/// negation. Substring matching approved "not okay", "don't book it"
/// ("bo-ok") and "no, I don't approve". An affirmative that also appears in
/// `prompt` does not count: the mic can pick up the speaker reading the prompt,
/// and an echoed "confirm" is not the user's answer. Echoed negations still
/// deny, which fails safe.
pub fn is_spoken_approval(transcript: &str, prompt: &str) -> bool {
    fn words(s: &str) -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '\'')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    }
    const NEGATIONS: &[&str] = &[
        "no", "not", "don't", "dont", "never", "stop", "cancel", "deny", "wait", "nope",
    ];
    const YES: &[&str] = &[
        "yes",
        "yeah",
        "yep",
        "confirm",
        "confirmed",
        "approve",
        "approved",
        "ok",
        "okay",
    ];
    let heard = words(transcript);
    let echoed = words(prompt);
    !heard.iter().any(|w| NEGATIONS.contains(&w.as_str()))
        && heard
            .iter()
            .any(|w| YES.contains(&w.as_str()) && !echoed.contains(w))
}

#[cfg(test)]
mod spoken_approval_tests {
    use super::is_spoken_approval;

    fn yes(transcript: &str) -> bool {
        is_spoken_approval(transcript, "")
    }

    /// Played while the mic was open, the old gate announcement read the
    /// page's button label aloud and approved a purchase by itself.
    #[test]
    fn the_prompt_echoed_back_is_not_an_approval() {
        let prompt = "Approval needed for browser_click — button_text: Confirm purchase";
        assert!(yes(prompt), "echo without the prompt filter approves");
        assert!(!is_spoken_approval(prompt, prompt));
        assert!(!is_spoken_approval(&format!("{prompt} okay, no"), prompt));
        assert!(is_spoken_approval(&format!("{prompt} yes"), prompt));
    }

    #[test]
    fn only_unnegated_affirmatives_approve() {
        assert!(yes("Yes."));
        assert!(yes("okay, confirm"));
        assert!(yes("yes, do it"));
        assert!(!yes("do not confirm"));
        assert!(!yes("not okay"));
        assert!(!yes("don't book it"));
        assert!(!yes("no, I don't approve"));
        assert!(!yes("cookie"));
        assert!(!yes(""));
    }
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
        "✓ XTTS v2 available (server starts with TTS on: /voice speak on or at launch)"
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
            out.push_str("\n\n  ");
            out.push_str(XTTS_FIRST_RUN_HINT);
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
    dirs::home_dir().map(|h| h.join(".local/share/oxideclaw/voice-clone"))
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
    let (dest, backup) = install_clone_sample(&src, &dest_dir, tier, size).await?;
    let backup_note = backup
        .map(|b| format!("Previous sample kept at {}\n", b.display()))
        .unwrap_or_default();

    Ok(format!(
        "Voice clone saved ({} tier, ~{}s).\n\
         Location: {}\n{}\n\
         TTS will now use XTTS v2 with your voice.\n\
         Use /voice clone remove to revert to the default XTTS v2 speaker.",
        tier.label(),
        est_secs,
        dest.display(),
        backup_note,
    ))
}

/// Copy `src` in as the active sample. A sample that is already there moves
/// to `my-voice.prev.wav` first: a recording that was meant as dictation (or
/// a worse take) must not silently destroy a long premium recording.
/// Returns the new sample path and the backup path, if one was made.
async fn install_clone_sample(
    src: &std::path::Path,
    dest_dir: &std::path::Path,
    tier: CloneTier,
    size: u64,
) -> Result<(PathBuf, Option<PathBuf>)> {
    tokio::fs::create_dir_all(dest_dir).await?;

    let dest = dest_dir.join("my-voice.wav");
    let meta = dest_dir.join("meta.txt");
    let mut backup = None;
    if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
        let prev = dest_dir.join("my-voice.prev.wav");
        tokio::fs::rename(&dest, &prev).await?;
        let _ = tokio::fs::rename(&meta, dest_dir.join("meta.prev.txt")).await;
        backup = Some(prev);
    }
    tokio::fs::copy(src, &dest).await?;

    // Also save the tier info
    tokio::fs::write(&meta, format!("tier={}\nsize={}\n", tier.label(), size)).await?;
    Ok((dest, backup))
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

    let wav_out = scratch_path("xtts", "wav");
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
        .stdin(Stdio::null())
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
                     Install:  uv tool install TTS --python 3.11 --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'\n\
                     {XTTS_FIRST_RUN_HINT}"
                ));
            }
        }
    }

    play_wav(&wav_out, stop_rx).await?;
    Ok(truncated)
}

#[cfg(test)]
mod scratch_and_body_tests {
    use super::{scratch_path, tts_request_body};

    #[test]
    fn scratch_paths_are_per_process_and_distinct_per_use() {
        let a = scratch_path("voice", "wav");
        let b = scratch_path("xtts", "wav");
        let pid = std::process::id().to_string();
        assert!(a.to_string_lossy().contains(&pid), "{a:?}");
        assert_ne!(a, b);
        assert!(a.starts_with(std::env::temp_dir()));
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("wav"));
    }

    #[test]
    fn tts_body_is_valid_json_for_multiline_text() {
        let text = "line one\nline two\t\"quoted\" back\\slash \u{1}ctl 日本語";
        let body = tts_request_body(text, None);
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["text"].as_str(), Some(text));
        assert!(v["speaker"].is_string());
        assert_eq!(v["language"], "en");

        let with_clone = tts_request_body("hi", Some(std::path::Path::new("/tmp/me \"x\".wav")));
        let v: serde_json::Value = serde_json::from_str(&with_clone).unwrap();
        assert_eq!(v["speaker_wav"].as_str(), Some("/tmp/me \"x\".wav"));
        assert!(v.get("speaker").is_none());
    }
}

#[cfg(test)]
mod xtts_server_script_tests {
    use super::{XTTS_SERVER_PY, install_xtts_server_script};

    #[test]
    fn server_script_comes_from_the_binary_not_the_project() {
        let project = tempfile::tempdir().unwrap();
        let planted = project.path().join("scripts");
        std::fs::create_dir_all(&planted).unwrap();
        std::fs::write(planted.join("xtts-server.py"), "import os; os.system('id')").unwrap();

        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("xtts");
        let script = install_xtts_server_script(&dir).unwrap();
        assert!(script.starts_with(&dir), "{script:?}");
        assert_eq!(std::fs::read_to_string(&script).unwrap(), XTTS_SERVER_PY);
        assert!(XTTS_SERVER_PY.contains("xtts_v2"));

        // A tampered copy is replaced on the next start.
        std::fs::write(&script, "print('stale')").unwrap();
        install_xtts_server_script(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&script).unwrap(), XTTS_SERVER_PY);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    /// XTTS v2 synthesises at 24 kHz; a hard-coded 22,050 Hz header made
    /// every server reply play ~8% slow and 1.5 semitones low.
    #[test]
    fn server_wav_header_uses_the_model_sample_rate() {
        assert!(!XTTS_SERVER_PY.contains("22050"));
        assert!(XTTS_SERVER_PY.contains("\"output_sample_rate\""));
        assert!(XTTS_SERVER_PY.contains("wav_bytes(samples, SAMPLE_RATE)"));
    }
}

#[cfg(all(test, unix))]
mod xtts_ready_tests {
    use super::{XttsStartCancelled, await_xtts_ready, kill_xtts_child};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn slot_with(cmd: &str, args: &[&str]) -> Mutex<Option<std::process::Child>> {
        let child = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        Mutex::new(Some(child))
    }

    #[tokio::test]
    async fn a_server_that_exits_on_its_license_prompt_is_reported_at_once() {
        // Stand-in for Coqui's first-run `input()`: with stdin at EOF the read
        // fails and the process exits, exactly like the real prompt.
        let slot = slot_with("sh", &["-c", "read answer || exit 3; sleep 30"]);
        let started = std::time::Instant::now();
        let err = await_xtts_ready(&slot, 200, Duration::from_millis(50), || false, || false)
            .await
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(5), "{err}");
        assert!(err.contains("exited before it was ready"), "{err}");
        assert!(err.contains("CPML license"), "{err}");
    }

    #[tokio::test]
    async fn a_server_that_never_listens_is_killed_on_timeout() {
        let slot = slot_with("sleep", &["30"]);
        let pid = slot.lock().unwrap().as_ref().unwrap().id();
        let err = await_xtts_ready(&slot, 3, Duration::from_millis(20), || false, || false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to start"), "{err}");
        assert!(slot.lock().unwrap().is_none());
        assert!(!pid_alive(pid), "server left running");
    }

    /// `/voice speak off` (or quitting) while the model loads: the lsof
    /// sweep cannot see a server that has not bound its port yet, so the
    /// process has to be killed through the handle, and the waiting start
    /// must not go on to announce "responses will be spoken".
    #[tokio::test]
    async fn a_stop_while_loading_kills_the_server_and_cancels_the_start() {
        let slot = slot_with("sleep", &["30"]);
        let pid = slot.lock().unwrap().as_ref().unwrap().id();
        let stop_gen = AtomicU64::new(0);
        let generation = stop_gen.load(Ordering::SeqCst);

        let wait = await_xtts_ready(
            &slot,
            200,
            Duration::from_millis(20),
            || false,
            || stop_gen.load(Ordering::SeqCst) != generation,
        );
        let stop = async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            kill_xtts_child(&slot, &stop_gen)
        };
        let (res, stopped) = tokio::join!(wait, stop);

        assert!(stopped, "a loading server counts as stopped");
        assert!(!pid_alive(pid), "server left running");
        let err = res.unwrap_err();
        assert!(err.is::<XttsStartCancelled>(), "{err}");
        // Nothing left to stop: the caller must not claim it stopped one.
        assert!(!kill_xtts_child(&slot, &stop_gen));
    }

    fn pid_alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}

#[cfg(test)]
mod xtts_probe_tests {
    use super::{load_or_create_xtts_token, probe_xtts_server, read_xtts_token, xtts_health_proof};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve one connection: `respond` maps the request line to a response.
    fn serve_once(respond: impl FnOnce(&str) -> Option<String> + Send + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            match respond(req.lines().next().unwrap_or("")) {
                Some(resp) => {
                    let _ = sock.write_all(resp.as_bytes());
                }
                // Hold the connection open without answering.
                None => std::thread::sleep(std::time::Duration::from_secs(5)),
            }
        });
        port
    }

    fn ok(body: &str) -> String {
        format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}")
    }

    /// Same digest Python's `hashlib.sha256(f"{token}:{nonce}".encode())`
    /// gives, so the embedded server and the probe agree.
    #[test]
    fn proof_matches_the_server_script_formula() {
        assert_eq!(
            xtts_health_proof("tok", "abc"),
            "7bda1c65a4b6804545292196a701b7aedc1a4c480b8cb09c04a884dd01980b1c"
        );
        assert!(super::XTTS_SERVER_PY.contains(r#"hashlib.sha256(f"{TOKEN}:{nonce}".encode())"#));
    }

    /// Coqui's tts-server (also on 5002) and anything else that answers
    /// /health without the proof used to count as our server.
    #[test]
    fn a_foreign_listener_is_not_our_server() {
        let port = serve_once(|_| Some(ok(r#"{"status":"ok","gpu":false}"#)));
        assert!(!probe_xtts_server(port, &"a".repeat(64)));

        let port = serve_once(|_| Some("HTTP/1.0 404 Not Found\r\n\r\n".into()));
        assert!(!probe_xtts_server(port, &"a".repeat(64)));

        let started = std::time::Instant::now();
        let port = serve_once(|_| None);
        assert!(!probe_xtts_server(port, &"a".repeat(64)));
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[test]
    fn a_server_holding_the_token_is_recognised() {
        let token = "b".repeat(64);
        let t = token.clone();
        let port = serve_once(move |line| {
            let nonce = line.split("nonce=").nth(1)?.split_whitespace().next()?;
            let proof = xtts_health_proof(&t, nonce);
            Some(ok(&format!(r#"{{"status":"ok","proof":"{proof}"}}"#)))
        });
        assert!(probe_xtts_server(port, &token));
    }

    #[test]
    fn token_is_private_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_xtts_token(dir.path()).is_none());
        let a = load_or_create_xtts_token(dir.path()).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(load_or_create_xtts_token(dir.path()).unwrap(), a);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}

#[cfg(test)]
mod voice_api_key_tests {
    use super::pick_voice_api_key;

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    /// With both set, the OpenAI key used to go to the custom voiceApiUrl.
    #[test]
    fn whisper_key_outranks_the_openai_key() {
        let both = env(&[
            ("OPENAI_API_KEY", "sk-openai"),
            ("WHISPER_API_KEY", "gsk-whisper"),
        ]);
        assert_eq!(
            pick_voice_api_key(both),
            Some(("WHISPER_API_KEY", "gsk-whisper".to_string()))
        );
        let openai_only = env(&[("OPENAI_API_KEY", "sk-openai"), ("WHISPER_API_KEY", "")]);
        assert_eq!(
            pick_voice_api_key(openai_only),
            Some(("OPENAI_API_KEY", "sk-openai".to_string()))
        );
        assert_eq!(pick_voice_api_key(env(&[])), None);
    }
}

#[cfg(test)]
mod clone_sample_tests {
    use super::*;

    #[tokio::test]
    async fn new_sample_keeps_the_previous_one_as_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let clone_dir = dir.path().join("voice-clone");
        let first = dir.path().join("first.wav");
        let second = dir.path().join("second.wav");
        std::fs::write(&first, b"premium take").unwrap();
        std::fs::write(&second, b"stray dictation").unwrap();

        let (_, backup) = install_clone_sample(&first, &clone_dir, CloneTier::Premium, 12)
            .await
            .unwrap();
        assert!(backup.is_none());

        let (dest, backup) = install_clone_sample(&second, &clone_dir, CloneTier::Quick, 15)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"stray dictation");
        let backup = backup.expect("old sample was overwritten without a backup");
        assert_eq!(std::fs::read(&backup).unwrap(), b"premium take");
        let prev_meta = std::fs::read_to_string(clone_dir.join("meta.prev.txt")).unwrap();
        assert!(prev_meta.contains(CloneTier::Premium.label()));
        let meta = std::fs::read_to_string(clone_dir.join("meta.txt")).unwrap();
        assert!(meta.contains(CloneTier::Quick.label()));
    }
}
