// Voice dictation capture worklet (docs/voice.md). Served as a real file —
// the daemon's CSP (default-src 'self', no blob:) forbids blob: module URLs.
// It ships raw Float32 input blocks to the main thread, which resamples to
// 16 kHz and frames them for the control WebSocket.
registerProcessor(
  "remux-pcm",
  class extends AudioWorkletProcessor {
    process(inputs) {
      const ch = inputs[0] && inputs[0][0];
      if (ch) this.port.postMessage(ch.slice(0));
      return true;
    }
  }
);
