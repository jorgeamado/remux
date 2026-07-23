# Voice input for the composer (opt-in)

Dictate commands into the phone composer. The phone captures audio and the
**daemon host** transcribes it with whisper.cpp, biased towards this
session's recent shell commands. The transcript lands in the composer for
review — dictation never types into the terminal and never presses Enter.

## Why host-side ASR

"On-device vs. cloud" is a false dichotomy here: the daemon runs on the
user's own machine, which already sees every keystroke and all terminal
output. Transcribing there stays inside the existing trust boundary (no
third-party ASR, no audio leaves the user's machines) while getting real
CPU/GPU, no 100+ MB model download to a Safari PWA, and direct access to the
vocabulary that makes domain-adapted recognition work: the session's recent
commands (and later: PATH, completion specs, visible pane text). remux is
unusable without a connection to the host, so offline phone dictation is not
a real scenario.

## Enabling it

Everything is off by default — no build dependency, no model, no mic button.

1. Build with the feature (needs cmake + a C/C++ toolchain):

   ```sh
   cargo build --release --features voice        # CPU inference
   cargo build --release --features voice-metal  # + Apple GPU on macOS
   ```

2. Install a model (a public artifact, stored in remux's state dir):

   ```sh
   remux voice download                  # base.en (~142 MB, fast)
   remux voice download --model small.en # more accurate
   remux voice status                    # what this build/host can do
   ```

3. Restart the daemon. It logs `voice dictation enabled (model …)` and
   advertises `voice: true` in the status frame; the PWA then shows a mic
   button in the composer. `--voice-model <path>` overrides the model file.

## Using it

Tap the mic (🎙) to record, tap again to transcribe. Tap-to-toggle, not
press-and-hold — iOS pointer semantics make hold gestures fragile. The
button pulses red while recording, dims while the host transcribes, and the
text is inserted at the composer cursor for editing before send.

Speak punctuation for flags: "git rebase **dash i** HEAD tilde three" —
vocabulary bias improves word choice but cannot reliably invent unspoken
structure. A conservative post-pass joins spoken dashes — "dash", "minus"
and "hyphen" all work (`minus h` → `-h`, `dash dash workspace` →
`--workspace`) — and strips whisper's trailing period;
everything else is preserved byte-for-byte. Hidden page / dropped socket
cancels the utterance rather than transcribing a truncated tail.

## Protocol

JSON text frames on the existing authenticated WS (binary frames stay
terminal-only):

- `voice_start` → begins an utterance (rejected with `voice_error
  voice_unavailable` when the host offers no voice).
- `voice_chunk {data}` → base64 of little-endian 16 kHz mono i16 PCM.
  ~0.5 s per chunk keeps frames under the 64 KiB cap and the text-message
  rate budget. Buffer capped at 60 s (`voice_too_long`).
- `voice_end` → transcribe; the daemon answers `voice_result {text}` or
  `voice_error {code, message}` — only ever to the connection that sent the
  audio. Inference runs off the receive loop, so keystrokes keep flowing.
- `voice_cancel` → drop the buffer.

Capture path: `getUserMedia` → AudioWorklet ships raw Float32 blocks →
main thread linear-resamples to 16 kHz (Web Speech API is not used: broken
in standalone iOS PWAs, no vocabulary control, and it ships audio to Apple).

## Vocabulary biasing

The whisper `initial_prompt` is rebuilt per utterance from
`Feed::recent_commands` for this session (memory-only, newest first, capped
at ~700 bytes) — command-shaped text that biases the decoder towards the
tools, flags and paths actually in play. Deliberately NOT included in V1:
executing installed programs with `--help` to harvest flags (arbitrary
executables can hang or have side effects). Follow-ups, in rough order:
visible-pane tokens, PATH basenames from the shell hook, completion specs,
and a lattice-constrained correction pass over low-confidence spans.

## Privacy rules

Same class as the command feed (memory-only), extended:

- No third-party ASR; audio never leaves the phone→daemon connection.
- Audio, transcripts and the bias prompt are memory-only: never logged,
  never written to disk, no temp WAV files. (The model file on disk is a
  public artifact.)
- The per-connection audio buffer is bounded (60 s ≈ 1.9 MiB) and dropped
  on end/cancel/disconnect.
- The transcript returns only to the connection that streamed the audio.
- Voice only edits the composer. It never takes control, never types into
  the PTY, never presses Enter.
- Mic capture stops (tracks closed, AudioContext closed) the moment an
  utterance ends or is cancelled; a hidden page cancels recording.
