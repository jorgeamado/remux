//! Dev smoke test for the voice pipeline — exercises Voice::transcribe with a
//! real model, off the daemon. Not built without the feature.
//!
//! ```sh
//! say --data-format=LEI16@16000 -o /tmp/cmd.wav "git status"   # macOS
//! cargo run --example voice_smoke --features voice -- \
//!     ~/Library/Application\ Support/remux/models/ggml-tiny.en.bin \
//!     /tmp/cmd.wav "git status; cargo build"
//! ```

#[cfg(not(feature = "voice"))]
fn main() {
    eprintln!("rebuild with --features voice");
}

#[cfg(feature = "voice")]
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let model = args.next().expect("usage: voice_smoke <model> <wav> [prompt]");
    let wav = args.next().expect("usage: voice_smoke <model> <wav> [prompt]");
    let prompt = args.next().unwrap_or_default();

    let pcm = read_wav_lei16_mono16k(&std::fs::read(wav)?)?;
    println!("{} samples ({:.1}s)", pcm.len(), pcm.len() as f64 / 16000.0);
    let voice = remux::voice::Voice::new(Some(model.into()));
    let start = std::time::Instant::now();
    let text = voice.transcribe(&pcm, &prompt)?;
    println!("transcript ({}ms): {text:?}", start.elapsed().as_millis());
    Ok(())
}

/// Minimal RIFF/WAVE reader for the exact format `say --data-format=LEI16@16000`
/// produces: PCM s16le, mono, 16 kHz. Walks chunks to find `data`.
#[cfg(feature = "voice")]
fn read_wav_lei16_mono16k(bytes: &[u8]) -> anyhow::Result<Vec<i16>> {
    anyhow::ensure!(bytes.len() > 44 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE");
    let mut off = 12;
    while off + 8 <= bytes.len() {
        let id = &bytes[off..off + 4];
        let len = u32::from_le_bytes(bytes[off + 4..off + 8].try_into()?) as usize;
        if id == b"data" {
            let data = &bytes[off + 8..(off + 8 + len).min(bytes.len())];
            return Ok(data
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect());
        }
        off += 8 + len + (len & 1);
    }
    anyhow::bail!("no data chunk")
}
