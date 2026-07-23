//! remux-intentd — the local voice-intent translator worker (docs/voice.md).
//!
//! A separate binary because whisper-rs (in the daemon) and llama-cpp-2 both
//! bundle ggml statically and cannot share one link, and because a worker
//! keeps C++ inference crashes out of the daemon. One request per process:
//! model path in argv[1], request JSON on stdin, the model's raw (grammar-
//! constrained) JSON on stdout. The daemon validates the output; nothing
//! here touches the network or the filesystem beyond reading the model.

use std::io::Read;

#[derive(serde::Deserialize)]
struct Request {
    transcript: String,
    #[serde(default)]
    recent: Vec<String>,
    #[serde(default)]
    heads: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let model = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: remux-intentd <model.gguf> < request.json"))?;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let req: Request = serde_json::from_str(&input)?;
    let heads: Vec<&str> = req.heads.iter().map(|s| s.as_str()).collect();
    let out = remux::intent::local::generate(
        std::path::Path::new(&model),
        &req.transcript,
        &req.recent,
        &heads,
    )?;
    println!("{out}");
    Ok(())
}
