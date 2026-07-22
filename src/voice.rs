//! Voice input for the composer (opt-in): host-side speech-to-text.
//!
//! The phone streams raw PCM over the authenticated control WebSocket and the
//! daemon transcribes it with whisper.cpp, biased towards this session's
//! recent shell commands. Inference runs on the daemon host — the machine
//! that already sees every keystroke and all terminal output — so audio never
//! leaves the user's own trust boundary (no third-party ASR, no cloud).
//!
//! Privacy rules (same class as the command feed, see feed.rs):
//! - audio, transcripts and the bias prompt are memory-only; never logged,
//!   never written to disk (the *model* file on disk is a public artifact);
//! - the transcript is returned only to the connection that sent the audio;
//! - a transcript only ever fills the composer on the client — dictation
//!   never types into the PTY and never presses Enter.
//!
//! Compiled out by default: the `voice` cargo feature pulls in whisper-rs
//! (a C/C++ build). Without it every entry point reports "unavailable" and
//! the client never shows a mic button.

use std::path::{Path, PathBuf};

/// Wire format: little-endian mono i16 PCM at this rate.
pub const SAMPLE_RATE: usize = 16_000;
/// Hard cap on one utterance — bounds the per-connection audio buffer
/// (60 s * 16 kHz * 2 B = ~1.9 MiB).
pub const MAX_UTTERANCE_SECS: usize = 60;
/// Byte budget for the whisper initial_prompt (soft vocabulary bias). Whisper
/// only keeps ~224 tokens of prompt; past that it's wasted work.
const PROMPT_MAX_BYTES: usize = 700;

/// Models `remux voice download` accepts, and the order `resolve_model`
/// prefers when several are installed (best accuracy first).
pub const KNOWN_MODELS: &[&str] = &[
    "small.en",
    "base.en",
    "tiny.en",
    "small",
    "base",
    "tiny",
    "medium.en",
    "medium",
    "large-v3-turbo",
];

pub fn models_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("models")
}

pub fn model_file(state_dir: &Path, model: &str) -> PathBuf {
    models_dir(state_dir).join(format!("ggml-{model}.bin"))
}

/// Pick the model to serve with: an explicit `--voice-model` path wins (None
/// if it doesn't exist — caller warns); otherwise the best installed model
/// from `KNOWN_MODELS` in the state dir.
pub fn resolve_model(explicit: Option<PathBuf>, state_dir: &Path) -> Option<PathBuf> {
    match explicit {
        Some(p) => p.exists().then_some(p),
        None => KNOWN_MODELS
            .iter()
            .map(|m| model_file(state_dir, m))
            .find(|p| p.exists()),
    }
}

/// Host-side transcription handle, stored on `App`. Disabled (the default)
/// when the feature is compiled out or no model is installed.
#[derive(Default)]
pub struct Voice {
    model_path: Option<PathBuf>,
    #[cfg(feature = "voice")]
    ctx: std::sync::OnceLock<Option<whisper_rs::WhisperContext>>,
}

impl Voice {
    pub fn new(model_path: Option<PathBuf>) -> Self {
        Self {
            model_path,
            #[cfg(feature = "voice")]
            ctx: std::sync::OnceLock::new(),
        }
    }

    /// Advertised to clients in the status frame; gates the mic button.
    pub fn available(&self) -> bool {
        cfg!(feature = "voice") && self.model_path.is_some()
    }

    /// Transcribe one utterance. Blocking (model load on first use, then
    /// inference) — call from `spawn_blocking`. `prompt` is the soft
    /// vocabulary bias; it conditions spelling, it cannot inject text.
    #[cfg(feature = "voice")]
    pub fn transcribe(&self, pcm: &[i16], prompt: &str) -> anyhow::Result<String> {
        use anyhow::Context;
        let path = self
            .model_path
            .as_ref()
            .context("no voice model configured")?;
        let ctx = self
            .ctx
            .get_or_init(|| {
                whisper_rs::WhisperContext::new_with_params(
                    &path.to_string_lossy(),
                    whisper_rs::WhisperContextParameters::default(),
                )
                .map_err(|e| tracing::warn!("voice model failed to load: {e}"))
                .ok()
            })
            .as_ref()
            .context("voice model failed to load")?;

        let mut audio = vec![0.0f32; pcm.len()];
        whisper_rs::convert_integer_to_float_audio(pcm, &mut audio)
            .map_err(|e| anyhow::anyhow!("pcm convert failed: {e}"))?;

        let mut params =
            whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        if !prompt.is_empty() {
            params.set_initial_prompt(prompt);
        }

        let mut state = ctx.create_state().context("create whisper state")?;
        state.full(params, &audio).context("whisper inference")?;
        let mut out = String::new();
        for i in 0..state.full_n_segments().context("segment count")? {
            out.push_str(
                &state
                    .full_get_segment_text_lossy(i)
                    .context("segment text")?,
            );
        }
        Ok(normalize_transcript(&out))
    }

    #[cfg(not(feature = "voice"))]
    pub fn transcribe(&self, _pcm: &[i16], _prompt: &str) -> anyhow::Result<String> {
        anyhow::bail!("this remux build does not include voice support")
    }
}

/// Decode a base64 chunk of little-endian mono i16 PCM. None = malformed.
pub fn decode_pcm(b64: &str) -> Option<Vec<i16>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

/// Build the whisper initial_prompt from this session's recent commands
/// (newest first, memory-only). Command-shaped text biases the decoder
/// towards the vocabulary actually in play: tool names, flags, paths.
pub fn build_prompt(recent: &[String]) -> String {
    let mut out = String::new();
    for cmd in recent {
        let cmd = cmd.trim();
        if cmd.is_empty() || cmd.len() > 200 {
            continue;
        }
        if out.len() + cmd.len() + 2 > PROMPT_MAX_BYTES {
            break;
        }
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(cmd);
    }
    out
}

/// Conservative cleanup of whisper's prose-shaped output for a command box.
/// Only two rules, both reviewable by the user before send:
/// - strip ONE trailing `.` or `,` (whisper ends utterances like sentences);
/// - join spoken dashes: `dash dash verbose` → `--verbose`, `dash i` → `-i`.
///
/// Everything else is preserved byte-for-byte — no rewriting, no guessing.
pub fn normalize_transcript(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    // A whole-utterance bracketed annotation is whisper marking non-speech
    // ("[BLANK_AUDIO]", "(silence)") — never a command. Report as empty.
    if (text.starts_with('[') && text.ends_with(']'))
        || (text.starts_with('(') && text.ends_with(')'))
    {
        return String::new();
    }
    if (text.ends_with('.') && !text.ends_with("..")) || text.ends_with(',') {
        text.pop();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0;
    let is_dash = |w: &str| w.eq_ignore_ascii_case("dash");
    let joinable = |w: &str| w.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    while i < words.len() {
        if is_dash(words[i])
            && i + 2 < words.len()
            && is_dash(words[i + 1])
            && joinable(words[i + 2])
        {
            out.push(format!("--{}", words[i + 2]));
            i += 3;
        } else if is_dash(words[i])
            && i + 1 < words.len()
            && !is_dash(words[i + 1])
            && joinable(words[i + 1])
        {
            out.push(format!("-{}", words[i + 1]));
            i += 2;
        } else {
            out.push(words[i].to_string());
            i += 1;
        }
    }
    out.join(" ")
}

/// `remux voice download`: fetch a ggml model from the official whisper.cpp
/// repository on Hugging Face into the state dir. The model is a public
/// artifact — writing it to disk carries no secrets.
pub async fn download(state_dir: &Path, model: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    if !KNOWN_MODELS.contains(&model) {
        anyhow::bail!(
            "unknown model {model:?} — one of: {}",
            KNOWN_MODELS.join(", ")
        );
    }
    let dest = model_file(state_dir, model);
    if dest.exists() {
        println!("already installed: {}", dest.display());
        return Ok(());
    }
    std::fs::create_dir_all(models_dir(state_dir))?;
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin");
    println!("downloading {url}");
    let resp = reqwest::get(&url).await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);

    // Download to a temp name, rename on success — a killed download never
    // leaves a truncated file that resolve_model would happily serve.
    let tmp = dest.with_extension("bin.part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    let mut last_pct: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download interrupted")?;
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        if let Some(pct) = (done * 100).checked_div(total) {
            if pct >= last_pct + 10 {
                last_pct = pct;
                println!("  {pct}% ({done}/{total} bytes)");
            }
        }
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, &dest).await?;
    println!("installed: {}", dest.display());
    if cfg!(feature = "voice") {
        println!("restart the daemon to enable dictation");
    } else {
        println!(
            "NOTE: this remux binary was built WITHOUT the `voice` feature — \
             rebuild with `cargo build --release --features voice` (or voice-metal)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_pcm_roundtrip() {
        use base64::Engine;
        let samples: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 12345];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert_eq!(decode_pcm(&b64).unwrap(), samples);
    }

    #[test]
    fn decode_pcm_rejects_garbage() {
        assert!(decode_pcm("not base64!!!").is_none());
        // odd byte count can't be i16 samples
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        assert!(decode_pcm(&b64).is_none());
    }

    #[test]
    fn prompt_joins_and_caps() {
        let recent = vec![
            "git status".to_string(),
            "cargo test --workspace".to_string(),
        ];
        assert_eq!(build_prompt(&recent), "git status; cargo test --workspace");
        // stays under the byte budget
        let many: Vec<String> = (0..100)
            .map(|i| format!("command-number-{i} --flag"))
            .collect();
        assert!(build_prompt(&many).len() <= 700);
        assert!(build_prompt(&[]).is_empty());
    }

    #[test]
    fn normalize_strips_one_trailing_period() {
        assert_eq!(normalize_transcript("git status."), "git status");
        assert_eq!(normalize_transcript("git status"), "git status");
        assert_eq!(normalize_transcript("ls .."), "ls ..");
    }

    #[test]
    fn normalize_drops_non_speech_annotations() {
        assert_eq!(normalize_transcript(" [BLANK_AUDIO]"), "");
        assert_eq!(normalize_transcript("(wind blowing)"), "");
        assert_eq!(normalize_transcript("ls (maybe)"), "ls (maybe)");
    }

    #[test]
    fn normalize_joins_spoken_dashes() {
        assert_eq!(
            normalize_transcript("cargo test dash dash workspace"),
            "cargo test --workspace"
        );
        assert_eq!(
            normalize_transcript("git rebase dash i HEAD~3"),
            "git rebase -i HEAD~3"
        );
        // a lone trailing "dash" is preserved, not guessed at
        assert_eq!(normalize_transcript("echo dash"), "echo dash");
        // "dash dash" with nothing joinable after stays literal-ish
        assert_eq!(normalize_transcript("dash dash"), "dash dash");
    }

    #[test]
    fn resolve_prefers_better_models() {
        let dir = std::env::temp_dir().join(format!("remux-voice-test-{}", std::process::id()));
        let models = models_dir(&dir);
        std::fs::create_dir_all(&models).unwrap();
        assert_eq!(resolve_model(None, &dir), None);
        std::fs::write(model_file(&dir, "tiny.en"), b"x").unwrap();
        assert_eq!(resolve_model(None, &dir), Some(model_file(&dir, "tiny.en")));
        std::fs::write(model_file(&dir, "base.en"), b"x").unwrap();
        assert_eq!(resolve_model(None, &dir), Some(model_file(&dir, "base.en")));
        // explicit path that doesn't exist → disabled, not a silent fallback
        assert_eq!(resolve_model(Some(dir.join("nope.bin")), &dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
