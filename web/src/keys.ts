/// Touch key row. Keys act on pointerdown with preventDefault so they never
/// steal focus from the terminal (which would dismiss the mobile keyboard).
/// Arrow keys repeat while held. "ctrl" arms a sticky modifier that
/// transforms the next typed character into a control code.

let ctrlArmed = false;

const KEY_BYTES: Record<string, string> = {
  esc: "\x1b",
  tab: "\t",
  "ctrl-c": "\x03",
  up: "\x1b[A",
  down: "\x1b[B",
  left: "\x1b[D",
  right: "\x1b[C",
  dash: "-",
  pipe: "|",
  slash: "/",
};

const REPEATABLE = new Set(["up", "down", "left", "right"]);
const REPEAT_DELAY_MS = 350;
const REPEAT_INTERVAL_MS = 70;

function haptic(): void {
  (navigator as any).vibrate?.(8); // Android; harmless no-op on iOS
}

export function setupKeyRow(sendInput: (data: string) => void): void {
  const row = document.getElementById("keyrow")!;
  row.hidden = false;

  row.querySelectorAll<HTMLButtonElement>(".key[data-key]").forEach((btn) => {
    const key = btn.dataset.key!;
    let delayTimer: number | undefined;
    let repeatTimer: number | undefined;

    const fire = () => {
      const data = KEY_BYTES[key];
      if (data) sendInput(data);
    };
    const stop = () => {
      clearTimeout(delayTimer);
      clearInterval(repeatTimer);
    };

    btn.addEventListener("pointerdown", (ev) => {
      ev.preventDefault(); // keep terminal focus + kill double-tap zoom
      haptic();
      fire();
      if (REPEATABLE.has(key)) {
        delayTimer = window.setTimeout(() => {
          repeatTimer = window.setInterval(fire, REPEAT_INTERVAL_MS);
        }, REPEAT_DELAY_MS);
      }
    });
    btn.addEventListener("pointerup", stop);
    btn.addEventListener("pointercancel", stop);
    btn.addEventListener("pointerleave", stop);
  });

  const ctrlBtn = document.getElementById("ctrl-key")!;
  ctrlBtn.addEventListener("pointerdown", (ev) => {
    ev.preventDefault();
    haptic();
    ctrlArmed = !ctrlArmed;
    ctrlBtn.classList.toggle("armed", ctrlArmed);
  });
}

/// Apply the sticky Ctrl modifier to terminal input. Returns the (possibly
/// transformed) data to send.
export function applyCtrl(data: string): string {
  if (!ctrlArmed || data.length !== 1) return data;
  const code = data.toLowerCase().charCodeAt(0);
  if (code >= 97 && code <= 122) {
    ctrlArmed = false;
    document.getElementById("ctrl-key")?.classList.remove("armed");
    return String.fromCharCode(code & 0x1f);
  }
  return data;
}
