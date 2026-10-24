// xterm.js attached to a PTY socket (§17 Terminal view).
// Output arrives through @xterm/addon-attach. Input and resize go out as JSON
// control frames (see src/api/sockets.ts). Read-only by default.

import { AttachAddon } from "@xterm/addon-attach";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { ptyInput, ptyResize, type PtySocket } from "../api/sockets";

export interface TerminalProps {
  /** Open the socket. Called once on mount. */
  connect: () => PtySocket;
  title?: string;
  /** Start with input enabled. Default false. */
  interactive?: boolean;
  /** Hide the read-only toggle; input stays as `interactive` says. */
  lockMode?: boolean;
  short?: boolean;
}

const OPEN = 1;

export function Terminal(props: TerminalProps) {
  let host!: HTMLDivElement;
   
  const [readOnly, setReadOnly] = createSignal(!props.interactive);
  const [state, setState] = createSignal<"connecting" | "open" | "closed">("connecting");
  const [size, setSize] = createSignal({ cols: 80, rows: 24 });
  let term: XTerm | undefined;
  let socket: PtySocket | undefined;

  const sendResize = () => {
    if (term && socket?.readyState === OPEN) socket.send(ptyResize(term.cols, term.rows));
  };

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

    socket = props.connect();
    socket.binaryType = "arraybuffer";
    // The addon only reads; our own handler writes JSON input frames.
    term.loadAddon(new AttachAddon(socket as unknown as WebSocket, { bidirectional: false }));
    socket.addEventListener("open", () => {
      setState("open");
      sendResize();
    });
    socket.addEventListener("close", () => setState("closed"));
    socket.addEventListener("error", () => setState("closed"));
    if (socket.readyState === OPEN) setState("open");

    term.onData((data) => {
      if (!readOnly() && socket?.readyState === OPEN) socket.send(ptyInput(data));
    });
    term.onResize(({ cols, rows }) => {
      setSize({ cols, rows });
      sendResize();
    });
    setSize({ cols: term.cols, rows: term.rows });

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
      clearTimeout(pending);
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

  return (
    <div class="terminal-wrap">
      <div class="terminal-bar">
        <span class="row">
          <strong>{props.title ?? "Terminal"}</strong>
          <span class={`chip ${state() === "open" ? "ok" : state() === "closed" ? "bad" : "info"}`}>{state()}</span>
          <span class="muted mono small">
            {size().cols}×{size().rows}
          </span>
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
