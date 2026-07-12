import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";

export interface TermHandle {
  term: Terminal;
  fit: () => void;
  /// Fires (debounced) with the settled grid size after layout changes.
  onResize: (cb: (cols: number, rows: number) => void) => void;
  size: () => { cols: number; rows: number };
  setFontSize: (px: number) => void;
  /// Sizing diagnostics — surfaced in the UI to debug device-specific grids.
  debug: () => string;
  /// Allow/forbid typing straight into the terminal. When off, taps never
  /// focus xterm's hidden textarea (no on-screen keyboard); the composer and
  /// key row remain the input surfaces. Mouse/touch reports are unaffected.
  setDirectInput: (enabled: boolean) => void;
}

export function createTerminal(container: HTMLElement, fontSize = 14): TermHandle {
  const term = new Terminal({
    cursorBlink: true,
    fontSize,
    // Concrete iOS-available monospace first: a font that resolves
    // differently between measurement and render skews the cell size.
    fontFamily: "Menlo, Monaco, 'Courier New', ui-monospace, monospace",
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

  // Open xterm in an inner wrapper that fills the container's *content* box
  // (100%/100%), NOT the padded card itself. FitAddon measures its parent, so
  // fitting against the padded #terminal would overestimate the grid by the
  // padding/border — tmux would then be told a larger size than is visible,
  // re-creating the very sizing mismatch we're eliminating.
  const box = document.createElement("div");
  box.id = "termbox"; // CSS: absolute inset:8px — fills the content box exactly
  container.appendChild(box);
  term.open(box);

  // The default DOM renderer positions per-character spans and, at small font
  // sizes with fractional cell widths, adjacent glyphs (especially htop/vim
  // box-drawing) visually overlap. The WebGL renderer draws each cell in a
  // fixed grid — no overlap, sharp at any size. Fall back silently to DOM if
  // the GL context is unavailable (e.g. headless CI) or lost.
  try {
    const webgl = new WebglAddon();
    webgl.onContextLoss(() => webgl.dispose());
    term.loadAddon(webgl);
  } catch {
    /* keep the DOM renderer */
  }

  // The grid xterm renders is exactly the grid we report to the daemon (and
  // thus tmux): no phantom rows, no clipping. A single source of truth for
  // the size is what keeps full-screen apps (htop/vim/Claude Code) from
  // redrawing against a stale geometry.
  let resizeCb: ((cols: number, rows: number) => void) | null = null;
  let notifyTimer: number | undefined;
  term.onResize(({ cols, rows }) => {
    // Debounce: a layout settle (keyboard, rotation, font change, container
    // resize) can emit several intermediate sizes in a burst. Sending each to
    // tmux hammers full-screen apps with redraws; only the final size matters.
    clearTimeout(notifyTimer);
    notifyTimer = window.setTimeout(() => resizeCb?.(cols, rows), 120);
  });

  const fit = () => {
    try {
      fitAddon.fit();
    } catch {
      /* container not laid out yet */
    }
  };

  // Coalesce rapid fit triggers (ResizeObserver can fire many times per
  // frame during a settle).
  let fitTimer: number | undefined;
  const scheduleFit = () => {
    clearTimeout(fitTimer);
    fitTimer = window.setTimeout(fit, 60);
  };

  const app = document.getElementById("app")!;
  const vv = window.visualViewport;
  const onViewportResize = () => {
    if (vv) {
      // iOS Safari: the keyboard overlays the page; shrink the app to the
      // visible viewport so the terminal stays fully on screen.
      app.style.height = `${vv.height}px`;
      window.scrollTo(0, 0);
    }
    scheduleFit();
  };
  vv?.addEventListener("resize", onViewportResize);
  // On scroll, only pin the page — never refit. iOS emits a stream of
  // visualViewport scroll events during momentum/rubber-band scrolling, and
  // refitting on each one makes the terminal flicker.
  vv?.addEventListener("scroll", () => window.scrollTo(0, 0));
  window.addEventListener("orientationchange", () => setTimeout(onViewportResize, 80));
  new ResizeObserver(scheduleFit).observe(container);
  new ResizeObserver(scheduleFit).observe(box);

  // Fit only after the font is actually loaded and laid out. Fitting before
  // the char-measure element has the real monospace metrics yields a wrong
  // (often far too small) cell size, hence a hugely oversized grid.
  const fitWhenReady = () => {
    const go = () => requestAnimationFrame(() => requestAnimationFrame(fit));
    if (document.fonts?.ready) {
      document.fonts.ready.then(go);
    } else {
      go();
    }
  };
  fitWhenReady();

  return {
    term,
    fit,
    onResize: (cb) => (resizeCb = cb),
    size: () => ({ cols: term.cols, rows: term.rows }),
    debug: () => {
      const b = box.getBoundingClientRect();
      const measure = document.querySelector(
        ".xterm-char-measure-element"
      ) as HTMLElement | null;
      const cellR = measure?.getBoundingClientRect();
      const xt = document.querySelector(".xterm") as HTMLElement | null;
      const xtFont = xt ? getComputedStyle(xt).fontSize : "?";
      const vv = window.visualViewport;
      return [
        `grid    ${term.cols}×${term.rows}  (fontSize opt ${term.options.fontSize})`,
        `box     ${b.width.toFixed(1)}×${b.height.toFixed(1)}px`,
        `cellmeas ${cellR ? `${cellR.width.toFixed(2)}×${cellR.height.toFixed(2)}` : "?"}px`,
        `.xterm  font ${xtFont}`,
        `dpr ${window.devicePixelRatio}  vv.scale ${vv?.scale ?? "?"}`,
        `vv ${vv ? `${vv.width.toFixed(0)}×${vv.height.toFixed(0)}` : "?"}  win ${window.innerWidth}×${window.innerHeight}`,
        `screen ${screen.width}×${screen.height}`,
      ].join("\n");
    },
    setFontSize: (px) => {
      term.options.fontSize = px;
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
