//! Voice intent mode (docs/voice.md): translate a naturally-spoken request
//! ("show processes sorted by memory") into ONE proposed shell command.
//!
//! The translator is an explicit, reviewed step — the antithesis of silently
//! rewriting a transcript: the client shows what was heard AND the proposal,
//! nothing runs without the user sending it from the composer. The LLM gets
//! the transcript plus advisory context (recent commands, known command
//! heads); it has no tools, no filesystem, no execution. Its output is
//! validated deterministically here: single line, bounded length, no control
//! bytes, and a rule-based risk lint the UI uses for warnings — the model
//! never grades its own safety.
//!
//! Two backends, preferred in order:
//! - **Local**: the `remux-intentd` worker binary (llama.cpp, built with the
//!   `intent` feature) running a small GGUF instruct model (Qwen2.5-Coder)
//!   with a GBNF grammar that makes anything but our JSON schema
//!   unrepresentable. Fully on-host — same trust boundary as the ASR, no
//!   cloud at all.
//! - **Cli**: shell out to the `claude` CLI (`-p`, non-interactive), reusing
//!   the host's existing Claude Code auth. Fallback when no local worker or
//!   model is installed.

use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Hard ceiling on a proposed command line.
const MAX_COMMAND_LEN: usize = 400;
/// Translator wall-clock budget. `claude -p` startup alone is seconds.
const TRANSLATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// A validated translation result, ready to serialize to the client.
#[derive(Debug, serde::Serialize)]
pub struct Proposal {
    /// `propose` | `clarify` | `refuse`
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// Deterministic lint verdict: `ok` | `unknown` (head not in dictionary)
    /// | `elevated` (destructive/privileged pattern — UI warns).
    pub risk: &'static str,
}

/// What the model is asked to return (before our validation).
#[derive(serde::Deserialize)]
struct RawProposal {
    status: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    explanation: Option<String>,
}

/// Which translator this host can run. Local wins: on-device and
/// grammar-constrained.
#[derive(Clone, Debug)]
pub enum Backend {
    /// The `remux-intentd` worker binary + this GGUF model.
    Local { worker: PathBuf, model: PathBuf },
    /// `claude -p` shell-out.
    Cli,
}

/// Pick the backend for this host. `intent_model` is the resolved GGUF path
/// (from `remux voice download --model qwen2.5-coder-1.5b`).
pub fn backend(intent_model: Option<&Path>) -> Option<Backend> {
    if let (Some(worker), Some(model)) = (worker_path(), intent_model) {
        return Some(Backend::Local {
            worker,
            model: model.to_path_buf(),
        });
    }
    cli_available().then_some(Backend::Cli)
}

/// Find `remux-intentd`: next to the running binary first (the normal
/// install layout), then PATH.
fn worker_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("remux-intentd");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join("remux-intentd"))
        .find(|p| p.is_file())
}

/// Is the `claude` CLI on PATH? Cached.
fn cli_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::split_paths(&path).any(|d| d.join("claude").is_file())
    })
}

/// Translate a transcript into a proposed command. Blocking (child process)
/// — call from `spawn_blocking`. Transcript and context are memory-only.
pub fn translate(
    backend: &Backend,
    transcript: &str,
    recent: &[String],
    heads: &[&str],
) -> anyhow::Result<Proposal> {
    let raw = match backend {
        Backend::Local { worker, model } => {
            let request = serde_json::json!({
                "transcript": transcript,
                "recent": recent,
                "heads": heads,
            })
            .to_string();
            let mut cmd = std::process::Command::new(worker);
            cmd.arg(model);
            run_with_timeout(cmd, Some(&request))?
        }
        Backend::Cli => {
            let prompt = format!(
                "{}\n\
                 - Respond with ONLY a JSON object, no markdown fences, no prose around it:\n\
                   {{\"status\":\"propose\"|\"clarify\"|\"refuse\",\"command\":string|null,\
                   \"question\":string|null,\"explanation\":string|null}}\n\
                 {}",
                rules(),
                context_block(transcript, recent, heads)
            );
            let mut cmd = std::process::Command::new("claude");
            cmd.args(["-p", "--model", "haiku"]).arg(&prompt);
            run_with_timeout(cmd, None)?
        }
    };
    parse_and_validate(&raw)
}

/// The shared rulebook both backends are prompted with. Written for a SMALL
/// model: explicit status semantics, defaults, and a worked example.
fn rules() -> &'static str {
    "You translate ONE spoken request into ONE shell command line for the user to review.\n\
     Choose status:\n\
     - \"propose\" (the default): you can name a command. Put it in \"command\" \
       (ONE line, no newlines; prefer common, portable forms) and set \
       \"question\" to null.\n\
     - \"clarify\": ONLY if the target, range, or destructive scope is genuinely \
       unclear. Put ONE short question in \"question\" and set \"command\" to null. \
       Never guess at anything destructive.\n\
     - \"refuse\": ONLY if the request is not about running a shell command. \
       \"command\" and \"question\" are null.\n\
     \"explanation\": one short sentence, no tutorials.\n\
     The context is advisory; you may use commands not listed.\n\
     Example — request: \"count lines in every rust file\" →\n\
     {\"status\":\"propose\",\"command\":\"find . -name '*.rs' | xargs wc -l\",\
     \"question\":null,\"explanation\":\"Counts lines across all .rs files.\"}"
}

fn context_block(transcript: &str, recent: &[String], heads: &[&str]) -> String {
    let recent_ctx = recent
        .iter()
        .take(10)
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Advisory context — recent commands (newest first): {recent_ctx}\n\
         Some available commands: {}\n\
         Spoken request: {transcript}",
        heads.join(" ")
    )
}

/// Run a translator child process: optional stdin payload, stdout captured,
/// killed at the deadline — a hung translator must not pin the blocking pool.
fn run_with_timeout(mut cmd: std::process::Command, input: Option<&str>) -> anyhow::Result<String> {
    use std::io::{Read, Write};
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(payload) = input {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(payload.as_bytes())?;
        drop(stdin); // EOF — the worker reads to end
    }
    // Drain stdout on a thread so a chatty child can't dead-lock on a full
    // pipe while we poll for exit.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + TRANSLATE_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => {
                anyhow::ensure!(status.success(), "translator exited with {status}");
                break;
            }
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("translator timed out");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    Ok(reader.join().unwrap_or_default())
}

/// Parse the model's output (tolerating markdown fences it was told not to
/// emit) and enforce the deterministic rules.
pub fn parse_and_validate(raw: &str) -> anyhow::Result<Proposal> {
    let start = raw
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON in translator output"))?;
    let end = raw
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("no JSON in translator output"))?;
    anyhow::ensure!(start < end, "malformed translator output");
    let parsed: RawProposal = serde_json::from_str(&raw[start..=end])?;

    let status = match parsed.status.as_str() {
        s @ ("propose" | "clarify" | "refuse") => s.to_string(),
        other => anyhow::bail!("translator returned unknown status {other:?}"),
    };
    let command = match (status.as_str(), parsed.command) {
        ("propose", Some(c)) => {
            let c = c.trim().to_string();
            anyhow::ensure!(!c.is_empty(), "empty command");
            anyhow::ensure!(c.len() <= MAX_COMMAND_LEN, "command too long");
            anyhow::ensure!(
                !c.chars().any(|ch| ch.is_control()),
                "command contains control characters"
            );
            Some(c)
        }
        ("propose", None) => anyhow::bail!("propose without a command"),
        _ => None,
    };
    Ok(Proposal {
        risk: command.as_deref().map(lint_risk).unwrap_or("ok"),
        status,
        command,
        question: parsed.question.filter(|q| !q.trim().is_empty()),
        explanation: parsed.explanation.filter(|e| !e.trim().is_empty()),
    })
}

/// Rule-based risk lint — controls the UI warning, independent of anything
/// the model claims. Deliberately coarse: false "elevated" costs one extra
/// glance; a false "ok" costs trust.
pub fn lint_risk(cmd: &str) -> &'static str {
    let lower = cmd.to_lowercase();
    let head = lower.split_whitespace().next().unwrap_or("");
    let elevated_head = matches!(
        head,
        "sudo" | "rm" | "dd" | "mkfs" | "shutdown" | "reboot" | "halt" | "chown" | "chmod"
    );
    let elevated_pattern = [
        "rm -",
        "--force",
        "--hard",
        "mkfs",
        " > /",
        "curl | sh",
        "| sh",
    ]
    .iter()
    .any(|p| lower.contains(p));
    if elevated_head || elevated_pattern {
        return "elevated";
    }
    let known = crate::voice::CORE_COMMANDS.contains(&head);
    if known {
        "ok"
    } else {
        "unknown"
    }
}

/// Local translator: llama.cpp with a GBNF grammar over our JSON schema —
/// the sampler cannot emit anything that doesn't parse. Lives in the
/// `remux-intentd` worker binary (see Cargo.toml for why it's separate);
/// one request per process.
#[cfg(feature = "intent")]
pub mod local {
    use std::num::NonZeroU32;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use anyhow::Context;
    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::model::{AddBos, LlamaModel};
    use llama_cpp_2::sampling::LlamaSampler;

    const N_CTX: u32 = 2048;
    const MAX_OUT_TOKENS: usize = 200;

    /// Only our schema is representable. Keys in fixed order; strings JSON-
    /// escaped; no nesting, no extra keys.
    const GRAMMAR: &str = r#"
root ::= "{" ws q "status" q ws ":" ws status ws "," ws q "command" q ws ":" ws sn ws "," ws q "question" q ws ":" ws sn ws "," ws q "explanation" q ws ":" ws sn ws "}"
status ::= q "propose" q | q "clarify" q | q "refuse" q
sn ::= string | "null"
string ::= q char* q
char ::= [^"\\\x00-\x1F] | "\\" (["\\/bfnrt] | "u" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F])
q ::= "\""
ws ::= [ ]?
"#;

    struct Engine {
        backend: LlamaBackend,
        model: LlamaModel,
    }
    // llama.cpp generation state is not thread-safe per model; a Mutex both
    // guards it and serializes concurrent utterances from different devices.
    static ENGINE: OnceLock<anyhow::Result<Mutex<Engine>>> = OnceLock::new();

    pub fn generate(
        model_path: &Path,
        transcript: &str,
        recent: &[String],
        heads: &[&str],
    ) -> anyhow::Result<String> {
        let engine = ENGINE
            .get_or_init(|| {
                let backend = LlamaBackend::init().context("llama backend")?;
                let model =
                    LlamaModel::load_from_file(&backend, model_path, &LlamaModelParams::default())
                        .context("load intent model")?;
                Ok(Mutex::new(Engine { backend, model }))
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("intent model failed to load: {e:#}"))?;
        let engine = engine.lock().unwrap();

        // Qwen instruct ChatML. The grammar enforces the output shape, so the
        // prompt only has to get the SEMANTICS right.
        let prompt = format!(
            "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n\
             <|im_start|>assistant\n",
            super::rules(),
            super::context_block(transcript, recent, heads),
        );

        let mut ctx = engine
            .model
            .new_context(
                &engine.backend,
                LlamaContextParams::default().with_n_ctx(NonZeroU32::new(N_CTX)),
            )
            .context("llama context")?;
        let tokens = engine
            .model
            .str_to_token(&prompt, AddBos::Always)
            .context("tokenize")?;
        anyhow::ensure!(
            tokens.len() < (N_CTX as usize) - MAX_OUT_TOKENS,
            "prompt too long for context"
        );

        let mut batch = LlamaBatch::new(N_CTX as usize, 1);
        let last = tokens.len() - 1;
        for (i, t) in tokens.iter().enumerate() {
            batch.add(*t, i as i32, &[0], i == last)?;
        }
        ctx.decode(&mut batch).context("prefill")?;

        let grammar = LlamaSampler::grammar(&engine.model, GRAMMAR, "root")
            .map_err(|e| anyhow::anyhow!("intent grammar rejected: {e}"))?;
        let mut sampler = LlamaSampler::chain_simple([grammar, LlamaSampler::greedy()]);
        let mut out = String::new();
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let n_prompt = tokens.len() as i32;
        for step in 0..MAX_OUT_TOKENS as i32 {
            // sample() also ACCEPTS the token into the chain (advancing the
            // grammar) — an explicit accept() here would advance it twice.
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            if engine.model.is_eog_token(token) {
                break;
            }
            let piece = engine
                .model
                .token_to_piece(token, &mut decoder, false, None)
                .map_err(|e| anyhow::anyhow!("detokenize: {e}"))?;
            out.push_str(&piece);
            batch.clear();
            batch.add(token, n_prompt + step, &[0], true)?;
            ctx.decode(&mut batch).context("decode")?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let raw = "```json\n{\"status\":\"propose\",\"command\":\"ps aux --sort=-%mem\",\
                   \"question\":null,\"explanation\":\"sorted by memory\"}\n```";
        let p = parse_and_validate(raw).unwrap();
        assert_eq!(p.status, "propose");
        assert_eq!(p.command.as_deref(), Some("ps aux --sort=-%mem"));
        assert_eq!(p.risk, "ok");
    }

    #[test]
    fn rejects_multiline_and_oversized_commands() {
        let nl = r#"{"status":"propose","command":"ls\nrm -rf /"}"#;
        assert!(parse_and_validate(nl).is_err());
        let long = format!(
            r#"{{"status":"propose","command":"echo {}"}}"#,
            "x".repeat(500)
        );
        assert!(parse_and_validate(&long).is_err());
        let none = r#"{"status":"propose"}"#;
        assert!(parse_and_validate(none).is_err());
        let bad_status = r#"{"status":"execute","command":"ls"}"#;
        assert!(parse_and_validate(bad_status).is_err());
    }

    #[test]
    fn clarify_and_refuse_pass_through() {
        let c = parse_and_validate(
            r#"{"status":"clarify","question":"Which branch should be rebased?"}"#,
        )
        .unwrap();
        assert_eq!(c.status, "clarify");
        assert!(c.command.is_none());
        let r = parse_and_validate(r#"{"status":"refuse","explanation":"not a command"}"#).unwrap();
        assert_eq!(r.status, "refuse");
    }

    #[test]
    fn risk_lint_flags_destructive_and_unknown() {
        assert_eq!(lint_risk("ps aux --sort=-%mem"), "ok");
        assert_eq!(lint_risk("git status"), "ok");
        assert_eq!(lint_risk("sudo systemctl restart nginx"), "elevated");
        assert_eq!(lint_risk("rm -rf ./target"), "elevated");
        assert_eq!(lint_risk("git push --force"), "elevated");
        assert_eq!(lint_risk("frobnicate --all"), "unknown");
    }
}
