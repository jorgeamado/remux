/// Touch key row: fixed keys send bytes directly; "ctrl" arms a sticky
/// modifier that transforms the next typed character into a control code.

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

export function setupKeyRow(sendInput: (data: string) => void): void {
  const row = document.getElementById("keyrow")!;
  row.hidden = false;

  row.querySelectorAll<HTMLButtonElement>(".key[data-key]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const data = KEY_BYTES[btn.dataset.key!];
      if (data) sendInput(data);
    });
  });

  const ctrlBtn = document.getElementById("ctrl-key")!;
  ctrlBtn.addEventListener("click", () => {
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
