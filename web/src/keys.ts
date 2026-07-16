/// Touch key row. Keys act on pointerdown with preventDefault so they never
/// steal focus from the terminal (which would dismiss the mobile keyboard).
/// Arrow and page keys repeat while held. "ctrl" arms a sticky modifier that
/// transforms the next typed character into a control code.
///
/// The panel is a deck with three rows: the agent row (Claude Code chords),
/// the "more" row (nav + symbols, toggled by …), and the primary row. On
/// touch devices the deck auto-expands when the on-screen keyboard is down —
/// that reclaimed space is for steering, not typing — and collapses back to
/// the primary row while the keyboard is up. A menu setting overrides auto.

let ctrlArmed = false;

const KEY_BYTES: Record<string, string> = {
  esc: "\x1b",
  tab: "\t",
  enter: "\r",
  "ctrl-c": "\x03",
  "ctrl-r": "\x12",
  "ctrl-l": "\x0c",
  "shift-tab": "\x1b[Z",
  "esc-esc": "\x1b\x1b",
  up: "\x1b[A",
  down: "\x1b[B",
  left: "\x1b[D",
  right: "\x1b[C",
  home: "\x1b[H",
  end: "\x1b[F",
  pgup: "\x1b[5~",
  pgdn: "\x1b[6~",
  dash: "-",
  underscore: "_",
  pipe: "|",
  slash: "/",
  tilde: "~",
  colon: ":",
  quote: "'",
  dquote: '"',
};

const REPEATABLE = new Set(["up", "down", "left", "right", "pgup", "pgdn"]);
const REPEAT_DELAY_MS = 350;
const REPEAT_INTERVAL_MS = 70;

const MORE_KEY = "remux.keymore";
const DECK_KEY = "remux.keydeck";

export type DeckMode = "auto" | "expanded" | "compact";

// Deck state. `userMore` is the …-toggle (persisted); `kbOpen` tracks the
// on-screen keyboard via visualViewport; `deckMode` is the menu override.
let deckMode: DeckMode = (() => {
  const stored = localStorage.getItem(DECK_KEY);
  return stored === "expanded" || stored === "compact" ? stored : "auto";
})();
let userMore = localStorage.getItem(MORE_KEY) === "on";
let kbOpen = false;

// Only touch devices have an on-screen keyboard; on desktop "auto" behaves
// like today's compact row (a hardware keyboard already has these keys).
const coarse = matchMedia("(pointer: coarse)").matches;

function haptic(): void {
  (navigator as any).vibrate?.(8); // Android; harmless no-op on iOS
}

function deckExpanded(): boolean {
  if (deckMode === "expanded") return true;
  if (deckMode === "compact") return false;
  return coarse && !kbOpen;
}

function applyDeck(): void {
  const agent = document.getElementById("keyrow-agent")!;
  const more = document.getElementById("keyrow-more")!;
  const moreBtn = document.getElementById("more-key")!;
  const expanded = deckExpanded();
  agent.hidden = !expanded;
  more.hidden = !(expanded || userMore);
  moreBtn.classList.toggle("armed", !more.hidden);
  // Terminal refit happens via term.ts's ResizeObserver on the flex box.
}

export function keyDeckMode(): DeckMode {
  return deckMode;
}

export function setKeyDeckMode(mode: DeckMode): void {
  deckMode = mode;
  localStorage.setItem(DECK_KEY, mode);
  applyDeck();
}

/// Keyboard visibility with hysteresis. The obscured height is measured
/// against the largest viewport seen since the last orientation change —
/// iOS overlays the keyboard (innerHeight stays put), Android with
/// resizes-content shrinks the layout viewport, and the baseline makes both
/// read the same. Open past ~140px, closed under ~70px; the gap keeps
/// browser-chrome resizes from flapping the deck.
const KB_OPEN_PX = 140;
const KB_CLOSE_PX = 70;
const EXPAND_DELAY_MS = 200;

function setupKeyboardWatch(): void {
  const vv = window.visualViewport;
  if (!vv || !coarse) return;

  let baseline = Math.max(window.innerHeight, vv.height);
  let expandTimer: number | undefined;

  const setOpen = (open: boolean) => {
    clearTimeout(expandTimer);
    if (open === kbOpen) return;
    if (open) {
      // Collapse immediately, before the keyboard animation eats the space.
      kbOpen = true;
      applyDeck();
    } else {
      // Expand only once the keyboard's close animation is done — swapping
      // rows mid-animation double-resizes the terminal.
      expandTimer = window.setTimeout(() => {
        kbOpen = false;
        applyDeck();
      }, EXPAND_DELAY_MS);
    }
  };

  const update = () => {
    baseline = Math.max(baseline, window.innerHeight, vv.height);
    const obscured = baseline - vv.height - vv.offsetTop;
    if (!kbOpen && obscured > KB_OPEN_PX) setOpen(true);
    else if (kbOpen && obscured < KB_CLOSE_PX) setOpen(false);
  };

  vv.addEventListener("resize", update);
  window.addEventListener("orientationchange", () => {
    baseline = 0; // re-learn: the old height would read as a stuck keyboard
    setTimeout(update, 120);
  });
  // Focusing any editable surface (composer, pairing field, xterm textarea)
  // is about to open the keyboard: collapse now rather than a resize later —
  // update() corrects if no keyboard actually appears (hardware keyboard).
  document.addEventListener("focusin", (ev) => {
    const t = ev.target;
    if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement) {
      setOpen(true);
      setTimeout(update, 450);
    }
  });
  document.addEventListener("focusout", () => setTimeout(update, 450));
}

export function setupKeyRow(
  sendInput: (data: string) => void,
  // Optional per-key intercept: return true to consume the key (don't send it
  // to the terminal). Used so e.g. left-arrow opens a picker in the chat view.
  intercept?: (key: string) => boolean
): void {
  const row = document.getElementById("keyrow")!;
  row.hidden = false;

  // "…" toggles the nav/symbols row while the deck is compact (when the deck
  // is expanded the row is already up and the toggle only flips the sticky
  // preference for later).
  const moreBtn = document.getElementById("more-key")!;
  moreBtn.addEventListener("pointerdown", (ev) => {
    ev.preventDefault();
    haptic();
    userMore = !userMore;
    localStorage.setItem(MORE_KEY, userMore ? "on" : "off");
    applyDeck();
  });

  document.querySelectorAll<HTMLButtonElement>(
    "#keyrow .key[data-key], #keyrow-more .key[data-key], #keyrow-agent .key[data-key]"
  ).forEach((btn) => {
    const key = btn.dataset.key!;
    let delayTimer: number | undefined;
    let repeatTimer: number | undefined;

    const fire = (): boolean => {
      if (intercept?.(key)) return true; // consumed (e.g. opened a picker)
      const data = KEY_BYTES[key];
      if (data) sendInput(data);
      return false;
    };
    const stop = () => {
      clearTimeout(delayTimer);
      clearInterval(repeatTimer);
    };

    btn.addEventListener("pointerdown", (ev) => {
      ev.preventDefault(); // keep terminal focus + kill double-tap zoom
      haptic();
      const consumed = fire();
      if (!consumed && REPEATABLE.has(key)) {
        // Capture so a small finger drift off the key doesn't cancel repeat.
        btn.setPointerCapture(ev.pointerId);
        delayTimer = window.setTimeout(() => {
          repeatTimer = window.setInterval(fire, REPEAT_INTERVAL_MS);
        }, REPEAT_DELAY_MS);
      }
    });
    btn.addEventListener("pointerup", stop);
    btn.addEventListener("pointercancel", stop);
    btn.addEventListener("lostpointercapture", stop);
    btn.addEventListener("pointerleave", stop);
  });

  const ctrlBtn = document.getElementById("ctrl-key")!;
  ctrlBtn.addEventListener("pointerdown", (ev) => {
    ev.preventDefault();
    haptic();
    ctrlArmed = !ctrlArmed;
    ctrlBtn.classList.toggle("armed", ctrlArmed);
  });

  setupKeyboardWatch();
  applyDeck();
}

/// Drop the sticky Ctrl without consuming it (e.g. a hardware Ctrl+letter
/// already produced the control code).
export function disarmCtrl(): void {
  ctrlArmed = false;
  document.getElementById("ctrl-key")?.classList.remove("armed");
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
