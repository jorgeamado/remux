import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

export interface TermHandle {
  term: Terminal;
  fit: () => void;
  onResize: (cb: (cols: number, rows: number) => void) => void;
  size: () => { cols: number; rows: number };
  setFontSize: (px: number) => void;
  /// Clip the bottom terminal row (the tmux status line) out of view.
  setHideStatusRow: (hide: boolean) => void;
  /// Allow/forbid typing straight into the terminal. When off, taps never
  /// focus xterm's hidden textarea (no on-screen keyboard); the composer and
  /// key row remain the input surfaces. Mouse/touch reports are unaffected.
  setDirectInput: (enabled: boolean) => void;
}

export function createTerminal(
  container: HTMLElement,
  fontSize = 14,
  hideStatusRow = true
): TermHandle {
  const term = new Terminal({
    cursorBlink: true,
    fontSize,
    fontFamily: "ui-monospace, Menlo, Consolas, monospace",
    scrollback: 2000,
    allowProposedApi: false,
    theme: {
      background: "#0d1117",
      foreground: "#e6edf3",
      cursor: "#e6edf3",
    },
  });
  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);

  // xterm opens inside an inner box; when the tmux status row is hidden the
  // box is exactly one cell-row taller than the container, and the container
  // (overflow: hidden) clips that last row. tmux always renders its status
  // line on the client's bottom row, so the clip only ever removes it.
  const box = document.createElement("div");
  box.id = "termbox";
  container.appendChild(box);
  term.open(box);

  let resizeCb: ((cols: number, rows: number) => void) | null = null;
  term.onResize(({ cols, rows }) => resizeCb?.(cols, rows));

  const cellHeight = (): number => {
    const rowsEl = box.querySelector<HTMLElement>(".xterm-rows");
    return rowsEl && term.rows > 0 ? rowsEl.clientHeight / term.rows : 0;
  };

  const innerHeight = (): number => {
    const cs = getComputedStyle(container);
    return (
      container.clientHeight -
      parseFloat(cs.paddingTop) -
      parseFloat(cs.paddingBottom)
    );
  };

  const fit = () => {
    const inner = innerHeight();
    box.style.height = `${inner}px`;
    try {
      fitAddon.fit();
    } catch {
      return; /* container not laid out yet */
    }
    if (hideStatusRow) {
      const ch = cellHeight();
      if (ch > 0) {
        box.style.height = `${inner + ch}px`;
        try {
          fitAddon.fit();
        } catch {
          /* ignore */
        }
      }
    }
  };

  // Refit whenever the terminal area changes: window resizes, the iOS
  // software keyboard opens (visualViewport), rotation, container changes.
  const app = document.getElementById("app")!;
  const vv = window.visualViewport;
  const onViewport = () => {
    if (vv) {
      // iOS Safari: keyboard overlays the page; shrink the app to the
      // visible viewport so the terminal stays fully on screen.
      app.style.height = `${vv.height}px`;
      window.scrollTo(0, 0);
    }
    fit();
  };
  vv?.addEventListener("resize", onViewport);
  vv?.addEventListener("scroll", onViewport);
  window.addEventListener("orientationchange", () => setTimeout(onViewport, 50));
  new ResizeObserver(fit).observe(container);

  fit();

  return {
    term,
    fit,
    onResize: (cb) => (resizeCb = cb),
    size: () => ({ cols: term.cols, rows: term.rows }),
    setFontSize: (px) => {
      term.options.fontSize = px;
      fit();
    },
    setHideStatusRow: (hide) => {
      hideStatusRow = hide;
      fit();
    },
    setDirectInput: (enabled) => {
      const ta = term.textarea;
      if (!ta) return;
      if (enabled) {
        ta.removeAttribute("inert");
      } else {
        ta.setAttribute("inert", ""); // focus() becomes a no-op
        ta.blur();
      }
    },
  };
}
