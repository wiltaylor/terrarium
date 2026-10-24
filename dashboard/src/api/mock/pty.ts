// A fake PTY that behaves like a WebSocket. It speaks the protocol in ../sockets:
// it parses JSON input and resize frames, echoes keystrokes, and answers each line.

import { parsePtyControl, type PtySocket, type RawSocket } from "../sockets";

type Listener = (ev: { data?: unknown; code?: number }) => void;

export interface FakePtyOptions {
  banner?: string[];
  prompt?: string;
  /** Answer one submitted line. Return the lines to print. */
  onLine?: (line: string) => string[] | void;
  /** Called on a timer. Return a line to print, or null. */
  ticker?: () => string | null;
  tickMs?: number;
  /** Refuse the upgrade, as the daemon does for an agent that is not running (409). */
  refuse?: boolean;
  /**
   * The subprotocol the "daemon" echoes (see ./ws). `null`: the handshake
   * failed auth, so the upgrade is refused. Absent: no auth check (unit tests).
   */
  protocol?: string | null;
}

const enc = new TextEncoder();

export class FakePty implements PtySocket, RawSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  readyState: number = FakePty.CONNECTING;
  binaryType: BinaryType = "arraybuffer";
  /** The subprotocol the server picked, as on a real `WebSocket`. */
  readonly protocol: string;
  /** Last size the client sent. Tests and the banner read it. */
  size = { cols: 80, rows: 24 };
  /** Every control frame the client sent, for tests. */
  readonly received: string[] = [];

  private listeners = new Map<string, Set<Listener>>();
  private line = "";
  private timer: ReturnType<typeof setInterval> | undefined;
  private readonly prompt: string;

  constructor(private readonly opts: FakePtyOptions = {}) {
    this.prompt = opts.prompt ?? "\x1b[32m❯\x1b[0m ";
    this.protocol = opts.protocol ?? "";
    queueMicrotask(() => {
      if (this.readyState !== FakePty.CONNECTING) return;
      if (opts.refuse || opts.protocol === null) {
        // A refused upgrade: the browser sees error, then close 1006.
        this.readyState = FakePty.CLOSED;
        this.emit("error", {});
        this.emit("close", { code: 1006 });
        return;
      }
      this.readyState = FakePty.OPEN;
      this.emit("open", {});
      for (const l of opts.banner ?? []) this.writeLine(l);
      this.write(this.prompt);
      if (opts.ticker) {
        this.timer = setInterval(() => {
          const out = opts.ticker!();
          if (out) {
            this.write(`\r\x1b[2K${out}\r\n${this.prompt}${this.line}`);
          }
        }, opts.tickMs ?? 2500);
      }
    });
  }

  addEventListener(type: string, fn: Listener): void {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(fn);
  }

  removeEventListener(type: string, fn: Listener): void {
    this.listeners.get(type)?.delete(fn);
  }

  send(raw: string): void {
    if (this.readyState !== FakePty.OPEN) return;
    this.received.push(raw);
    const msg = parsePtyControl(raw);
    if (!msg) return;
    if (msg.type === "resize") {
      this.size = { cols: msg.cols, rows: msg.rows };
      return;
    }
    for (const ch of msg.data) this.key(ch);
  }

  close(): void {
    if (this.readyState === FakePty.CLOSED) return;
    clearInterval(this.timer);
    this.readyState = FakePty.CLOSED;
    this.emit("close", { code: 1000 });
  }

  /** Print a line from the "server" side. */
  writeLine(s: string): void {
    this.write(`${s}\r\n`);
  }

  private key(ch: string): void {
    if (ch === "\r" || ch === "\n") {
      const line = this.line;
      this.line = "";
      this.write("\r\n");
      const out = this.opts.onLine?.(line) ?? (line ? [`\x1b[2m(mock) received: ${line}\x1b[0m`] : []);
      for (const l of out) this.writeLine(l);
      this.write(this.prompt);
    } else if (ch === "\x7f" || ch === "\b") {
      if (this.line.length) {
        this.line = this.line.slice(0, -1);
        this.write("\b \b");
      }
    } else if (ch === "\x03") {
      this.line = "";
      this.write(`^C\r\n${this.prompt}`);
    } else if (ch >= " ") {
      this.line += ch;
      this.write(ch);
    }
  }

  private write(s: string): void {
    if (this.readyState !== FakePty.OPEN) return;
    const bytes = enc.encode(s);
    const buf = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    this.emit("message", { data: buf });
  }

  private emit(type: string, ev: { data?: unknown; code?: number }): void {
    for (const fn of this.listeners.get(type) ?? []) fn(ev);
  }
}
