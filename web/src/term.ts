import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";

export interface TermHandle {
  term: Terminal;
  fit: () => void;
  onResize: (cb: (cols: number, rows: number) => void) => void;
  size: () => { cols: number; rows: number };
}

export function createTerminal(container: HTMLElement): TermHandle {
  const term = new Terminal({
    cursorBlink: true,
    fontSize: 14,
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
  term.open(container);

  let resizeCb: ((cols: number, rows: number) => void) | null = null;
  term.onResize(({ cols, rows }) => resizeCb?.(cols, rows));

  const fit = () => {
    try {
      fitAddon.fit();
    } catch {
      /* container not laid out yet */
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
  };
}
