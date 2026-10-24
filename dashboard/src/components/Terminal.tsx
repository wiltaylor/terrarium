// xterm.js attached to a PTY socket (§17 Terminal view).
// Output arrives through @xterm/addon-attach. Input and resize go out as JSON
// control frames (see src/api/sockets.ts). Read-only by default. On a drop it
// reconnects with backoff, and the daemon's first frame redraws the screen.

import { AttachAddon } from "@xterm/addon-attach";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { createEffect, createSignal, on, onCleanup, onMount, Show } from "solid-js";
import { backoffDelay, ptyInput, ptyResize, type PtySocket } from "../api/sockets";

export interface TerminalProps {
  /** Open a socket. Called on mount and on every reconnect. */
  connect: () => PtySocket;
  title?: string;
  /** Start with input enabled. Default false. */
  interactive?: boolean;
  /** Hide the read-only toggle; input stays as `interactive` says. */
  lockMode?: boolean;
  short?: boolean;
  /**
   * Whether a connect can succeed now (for an agent: it is running). When it
   * turns true, the terminal reconnects at once. Default: always.
   */
  available?: boolean;
  /** Retry after a drop. Default true. */
  reconnect?: boolean;
}

const OPEN = 1;

export function Terminal(props: TerminalProps) {
  let host!: HTMLDivElement;
  const [readOnly, setReadOnly] = createSignal(!props.interactive);
  const [state, setState] = createSignal<"connecting" | "open" | "closed" | "waiting">("connecting");
  const [size, setSize] = createSignal({ cols: 80, rows: 24 });
  const [retryIn, setRetryIn] = createSignal<number | null>(null);
  let term: XTerm | undefined;
  let socket: PtySocket | undefined;
  let attach: AttachAddon | undefined;
  let attempt = 0;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;

  const available = () => props.available ?? true;

  const sendResize = () => {
    if (term && socket?.readyState === OPEN) socket.send(ptyResize(term.cols, term.rows));
  };

  const scheduleRetry = () => {
    clearTimeout(retryTimer);
    if (disposed || props.reconnect === false) return;
    if (!available()) {
      setState("waiting");
      setRetryIn(null);
      return; // the effect below reconnects when it becomes available
    }
    const delay = backoffDelay(attempt++, 500, 10_000);
    setRetryIn(Date.now() + delay);
    retryTimer = setTimeout(open, delay);
  };

  function open() {
    if (disposed || !term) return;
    clearTimeout(retryTimer);
    setRetryIn(null);
    attach?.dispose();
    const old = socket;
    socket = undefined;
    old?.close();
    setState("connecting");
    const ws = props.connect();
    socket = ws;
    ws.binaryType = "arraybuffer";
    let first = true;
    // The first frame after a connect is a fresh snapshot of the screen
    // (docs/api.md §6.5). Clear the old screen so it does not draw twice.
    ws.addEventListener("message", () => {
      if (first && socket === ws) {
        first = false;
        term?.reset();
      }
    });
    attach = new AttachAddon(ws as unknown as WebSocket, { bidirectional: false });
    term.loadAddon(attach);
    ws.addEventListener("open", () => {
      if (socket !== ws) return;
      attempt = 0;
      setState("open");
      sendResize();
    });
    let lost = false;
    const onLost = () => {
      if (lost || socket !== ws) return;
      lost = true;
      setState("closed");
      scheduleRetry();
    };
    ws.addEventListener("close", onLost);
    ws.addEventListener("error", onLost);
    if (ws.readyState === OPEN) setState("open");
  }

  // Reconnect at once when the agent becomes available again.
  createEffect(
    on(available, (ok, was) => {
      if (ok && was === false && state() !== "open" && state() !== "connecting") {
        attempt = 0;
        open();
      }
    }),
  );

  onMount(() => {
    term = new XTerm({
      fontFamily: getComputedStyle(document.documentElement).getPropertyValue("--mono") || "monospace",
      fontSize: 13,
      cursorBlink: true,
      disableStdin: readOnly(),
      scrollback: 5000,
      theme: { background: "#0f1412", foreground: "#d8e3dc", cursor: "#7fd1a3" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    try {
      fit.fit();
    } catch {
      // host not laid out yet
    }

    term.onData((data) => {
      if (!readOnly() && socket?.readyState === OPEN) socket.send(ptyInput(data));
    });
    term.onResize(({ cols, rows }) => {
      setSize({ cols, rows });
      sendResize();
    });
    setSize({ cols: term.cols, rows: term.rows });
    if (available()) open();
    else setState("waiting");

    let pending: ReturnType<typeof setTimeout> | undefined;
    const ro = new ResizeObserver(() => {
      clearTimeout(pending);
      pending = setTimeout(() => {
        try {
          fit.fit();
        } catch {
          // ignore
        }
      }, 80);
    });
    ro.observe(host);

    onCleanup(() => {
      disposed = true;
      clearTimeout(pending);
      clearTimeout(retryTimer);
      ro.disconnect();
      socket?.close();
      term?.dispose();
    });
  });

  const toggle = () => {
    const next = !readOnly();
    setReadOnly(next);
    if (term) {
      term.options.disableStdin = next;
      if (!next) term.focus();
    }
  };

  const label = () => {
    const s = state();
    if (s === "waiting") return "not running";
    if (s === "closed" && retryIn()) return "disconnected, retrying";
    return s;
  };

  return (
    <div class="terminal-wrap">
      <div class="terminal-bar">
        <span class="row">
          <strong>{props.title ?? "Terminal"}</strong>
          <span class={`chip ${state() === "open" ? "ok" : state() === "connecting" ? "info" : "bad"}`}>{label()}</span>
          <span class="muted mono small">
            {size().cols}×{size().rows}
          </span>
          <Show when={state() === "closed" || state() === "waiting"}>
            <button
              class="small"
              onClick={() => {
                attempt = 0;
                open();
              }}
            >
              Reconnect
            </button>
          </Show>
        </span>
        <Show when={!props.lockMode}>
          <label class="row" style={{ display: "flex", color: "var(--text)" }}>
            <input type="checkbox" checked={!readOnly()} onChange={toggle} />
            Input {readOnly() ? "off (read-only)" : "on"}
          </label>
        </Show>
      </div>
      <div class={`terminal-host${props.short ? " short" : ""}`} ref={host} />
    </div>
  );
}
