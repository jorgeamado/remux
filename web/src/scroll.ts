/// Touch scrollback: xterm.js translates real wheel events into terminal
/// mouse reports (tmux has `mouse on`, so wheel-up enters copy-mode and
/// scrolls history), but it does NOT do this for touch drags. This module
/// converts vertical swipes into SGR mouse wheel reports aimed at the cell
/// under the finger. Scrolling back to the bottom exits copy-mode (tmux
/// binds wheel to `copy-mode -e`), so swiping down again resumes live view.

import type { Terminal } from "@xterm/xterm";

export function setupTouchScroll(
  container: HTMLElement,
  term: Terminal,
  sendInput: (data: string) => void
): void {
  let lastY: number | null = null;

  const cellAt = (touch: Touch): { col: number; row: number } | null => {
    const screen = container.querySelector(".xterm-screen");
    if (!screen) return null;
    const rect = screen.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return null;
    const col = Math.min(
      term.cols,
      Math.max(1, Math.floor(((touch.clientX - rect.left) / rect.width) * term.cols) + 1)
    );
    const row = Math.min(
      term.rows,
      Math.max(1, Math.floor(((touch.clientY - rect.top) / rect.height) * term.rows) + 1)
    );
    return { col, row };
  };

  const cellHeight = () => {
    const screen = container.querySelector(".xterm-screen");
    return screen ? screen.getBoundingClientRect().height / term.rows : 20;
  };

  container.addEventListener(
    "touchstart",
    (ev) => {
      if (ev.touches.length === 1) {
        lastY = ev.touches[0].clientY;
      }
    },
    { passive: true }
  );

  container.addEventListener(
    "touchmove",
    (ev) => {
      if (lastY === null || ev.touches.length !== 1) return;
      const touch = ev.touches[0];
      const dy = touch.clientY - lastY;
      const step = cellHeight();
      const ticks = Math.trunc(dy / step);
      if (ticks !== 0) {
        const cell = cellAt(touch);
        if (cell) {
          // Finger down = view earlier content = wheel up (button 64).
          const button = ticks > 0 ? 64 : 65;
          const report = `\x1b[<${button};${cell.col};${cell.row}M`;
          sendInput(report.repeat(Math.min(Math.abs(ticks), 10)));
        }
        lastY = touch.clientY;
      }
      // Stop xterm.js/Safari from also scrolling or rubber-banding.
      if (ev.cancelable) ev.preventDefault();
    },
    { passive: false }
  );

  container.addEventListener("touchend", () => {
    // A plain tap (no movement) falls through to xterm.js for focus.
    lastY = null;
  });
}
