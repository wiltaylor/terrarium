// The conversation view's logic, with no UI (docs/api.md §6, transcripts).
//
// - `mergeBlocks`: one list by `seq`. A page, a live `block`, and an
//   `update` frame all go through it, so a block never shows twice.
// - `groupBlocks`: blocks to render items. A tool_call takes its tool_result
//   under it; each turn gets a footer with the model and the token usage.
// - `loadNewest` / `loadEarlier`: the window of the newest 500 blocks, and
//   pages before it.
// - `composerState`: when the operator may type a turn.
// - `tailOnScroll` / `tailOnAppend`: auto-scroll that pauses when the
//   operator scrolls up.

import type { AgentStatus, Block, BlockOrigin, BlockUsage, TranscriptPage } from "../../../api/types";
import { renderMarkdown } from "../../../lib/markdown";
import { compact } from "../../../lib/format";

/** How many blocks the view loads at first, and with each "load earlier". */
export const WINDOW = 500;

/** Merge blocks by `seq`, oldest first. A block in `incoming` replaces the one with the same `seq` (an `update`). */
export function mergeBlocks(current: Block[], incoming: Block[]): Block[] {
  if (!incoming.length) return current;
  const bySeq = new Map<number, Block>();
  for (const b of current) bySeq.set(b.seq, b);
  for (const b of incoming) bySeq.set(b.seq, b);
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq);
}

// ----- render items -----

export interface Chip {
  label: string;
  /** A `.chip` modifier: `ok`, `info`, `warn`, or empty. */
  tone: "" | "ok" | "info" | "warn";
  title: string;
}

/** The chip on a user turn: who typed it. */
export function originChip(origin: BlockOrigin | undefined): Chip | null {
  switch (origin) {
    case "operator":
      return { label: "you", tone: "ok", title: "The operator sent this turn (dashboard or CLI)." };
    case "agent_mail_pointer":
      return { label: "mail pointer", tone: "info", title: "The daemon typed a pointer to new mail, cards, or leases." };
    case "system":
      return { label: "system", tone: "warn", title: "The daemon typed this turn (a schedule, a recap, a prompt)." };
    case "user":
      return { label: "terminal", tone: "", title: "Keys typed straight into the terminal." };
    default:
      return null;
  }
}

export interface TurnFooter {
  model?: string;
  usage?: BlockUsage;
}

export type RenderItem =
  | { type: "user"; key: string; block: Block; chip: Chip | null }
  | { type: "text"; key: string; block: Block }
  | { type: "thinking"; key: string; block: Block }
  | { type: "tool"; key: string; call: Block; result?: Block }
  /** A result whose call is not loaded (it is before the window) or is missing. */
  | { type: "result"; key: string; block: Block }
  | { type: "system"; key: string; block: Block }
  | { type: "footer"; key: string; footer: TurnFooter };

const sameUsage = (a: BlockUsage, b: BlockUsage) => a.input === b.input && a.output === b.output && a.cache_read === b.cache_read && a.cache_write === b.cache_write;

/**
 * Sum the usage of a turn. Some harnesses repeat one model call's usage on
 * each block of that call (text and tool calls), so an equal usage on the
 * next block that has one counts once.
 */
export function turnUsage(blocks: Block[]): BlockUsage | undefined {
  let sum: BlockUsage | undefined;
  let prev: BlockUsage | undefined;
  for (const b of blocks) {
    const u = b.usage;
    if (!u) continue;
    if (!prev || !sameUsage(prev, u)) {
      sum = sum ?? { input: 0, output: 0, cache_read: 0, cache_write: 0 };
      sum.input += u.input;
      sum.output += u.output;
      sum.cache_read += u.cache_read;
      sum.cache_write += u.cache_write;
    }
    prev = u;
  }
  return sum;
}

/** A user turn: a user text block. A tool result has role `user` too, but it is not a turn. */
const startsTurn = (b: Block) => b.role === "user" && b.kind === "text";

/** Blocks, oldest first, to render items. */
export function groupBlocks(blocks: Block[]): RenderItem[] {
  const out: RenderItem[] = [];
  const calls = new Map<string, Extract<RenderItem, { type: "tool" }>>();
  let turn: Block[] = [];

  const closeTurn = () => {
    const assistant = turn.filter((b) => b.role === "assistant");
    if (assistant.length) {
      const model = [...assistant].reverse().find((b) => b.model)?.model;
      const usage = turnUsage(assistant);
      if (model || usage) out.push({ type: "footer", key: `footer-${assistant.at(-1)!.seq}`, footer: { model, usage } });
    }
    turn = [];
  };

  for (const b of blocks) {
    if (startsTurn(b)) {
      closeTurn();
      out.push({ type: "user", key: `b${b.seq}`, block: b, chip: originChip(b.origin) });
      continue;
    }
    turn.push(b);
    switch (b.kind) {
      case "thinking":
        out.push({ type: "thinking", key: `b${b.seq}`, block: b });
        break;
      case "tool_call": {
        const item = { type: "tool" as const, key: `b${b.seq}`, call: b };
        if (b.tool?.id) calls.set(b.tool.id, item);
        out.push(item);
        break;
      }
      case "tool_result": {
        const owner = b.result?.tool_id ? calls.get(b.result.tool_id) : undefined;
        if (owner && !owner.result) owner.result = b;
        else out.push({ type: "result", key: `b${b.seq}`, block: b });
        break;
      }
      case "system":
        out.push({ type: "system", key: `b${b.seq}`, block: b });
        break;
      default:
        // `text` from the assistant, or from the system role.
        out.push(b.role === "system" ? { type: "system", key: `b${b.seq}`, block: b } : { type: "text", key: `b${b.seq}`, block: b });
    }
  }
  closeTurn();
  return out;
}

/** The quiet footer line of a turn. */
export function footerLine(f: TurnFooter): string {
  const parts: string[] = [];
  if (f.model) parts.push(f.model);
  const u = f.usage;
  if (u) {
    parts.push(`in ${compact(u.input)}`, `out ${compact(u.output)}`);
    if (u.cache_read) parts.push(`cache read ${compact(u.cache_read)}`);
    if (u.cache_write) parts.push(`cache write ${compact(u.cache_write)}`);
  }
  return parts.join(" · ");
}

/** Assistant text to HTML. The view puts it into the DOM only through `setSafeHtml` (DOMPurify). */
export const assistantHtml = (text: string, project: string): string => renderMarkdown(text, project);

/** Tool input as readable JSON. */
export function prettyInput(input: unknown): string {
  if (typeof input === "string") return input;
  try {
    return JSON.stringify(input, null, 2) ?? String(input);
  } catch {
    return String(input);
  }
}

/** A result this long starts collapsed. */
export const isLongOutput = (s: string): boolean => s.length > 600 || s.split("\n").length > 12;

/** The first lines of a long output, for the collapsed view. */
export function outputPreview(s: string, lines = 4): string {
  const ls = s.split("\n");
  const head = ls.slice(0, lines).join("\n");
  return head.length > 400 ? `${head.slice(0, 400)}…` : head;
}

// ----- the composer -----

/** Statuses that take a turn. The daemon queues it until the agent is idle. */
export const SENDABLE: AgentStatus[] = ["idle", "working", "blocked"];

export interface ComposerState {
  enabled: boolean;
  /** Why the composer is off. */
  reason?: string;
  /** A note while on: the turn waits in the queue. */
  note?: string;
}

export function composerState(o: { agentStatus: AgentStatus | undefined; sessionActive: boolean }): ComposerState {
  const st = o.agentStatus ?? "stopped";
  if (st === "stopped" || st === "error") return { enabled: false, reason: `The agent is ${st}. Start it to send a turn.` };
  if (!SENDABLE.includes(st)) return { enabled: false, reason: `The agent is ${st}. Wait until it is ready.` };
  if (!o.sessionActive) return { enabled: false, reason: "This is not the active session. Pick the active session to send a turn." };
  if (st !== "idle") return { enabled: true, note: `queued — the agent is ${st}` };
  return { enabled: true };
}

/** Enter sends; Shift+Enter (and Enter while an IME composes) does not. */
export const isSendKey = (e: { key: string; shiftKey: boolean; isComposing?: boolean; altKey?: boolean; ctrlKey?: boolean; metaKey?: boolean }): boolean =>
  e.key === "Enter" && !e.shiftKey && !e.isComposing && !e.altKey;

// ----- auto-scroll -----

export interface ScrollMetrics {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}

export interface TailState {
  /** Scroll to the end when blocks come. */
  follow: boolean;
  /** Blocks that came while the operator read further up. */
  unseen: number;
}

export const TAIL_START: TailState = { follow: true, unseen: 0 };

/** Near the end: within `slack` pixels. */
export const nearBottom = (m: ScrollMetrics, slack = 48): boolean => m.scrollHeight - m.scrollTop - m.clientHeight <= slack;

/** After a scroll: at the end, follow again; above it, pause. */
export function tailOnScroll(s: TailState, m: ScrollMetrics): TailState {
  if (nearBottom(m)) return s.follow && s.unseen === 0 ? s : { follow: true, unseen: 0 };
  return s.follow ? { ...s, follow: false } : s;
}

/** New blocks came. While paused, count them for the "new messages" pill. */
export function tailOnAppend(s: TailState, added: number): TailState {
  if (added <= 0 || s.follow) return s;
  return { ...s, unseen: s.unseen + added };
}

// ----- paging -----

export type FetchTranscript = (after: number, limit: number) => Promise<TranscriptPage>;

export interface Window {
  blocks: Block[];
  /** Blocks with `seq <= floor` are not loaded. 0: the window starts at the first block. */
  floor: number;
  active: boolean;
}

/**
 * The newest `size` blocks. `total` is the session's `blocks` count from the
 * list; when it is stale or absent, the pages after it still come in, and
 * the window keeps the newest `size`.
 */
export async function loadNewest(fetch: FetchTranscript, total: number | undefined, size = WINDOW, maxPages = 50): Promise<Window> {
  let after = Math.max(0, (total ?? 0) - size);
  let floor = after;
  let blocks: Block[] = [];
  let active = false;
  for (let i = 0; i < maxPages; i++) {
    const p = await fetch(after, size);
    active = p.active;
    blocks = mergeBlocks(blocks, p.blocks.filter((b) => b.seq > floor));
    if (p.complete || !p.blocks.length || p.next_after <= after) break;
    after = p.next_after;
  }
  if (blocks.length > size) {
    const cut = blocks.length - size;
    floor = blocks[cut - 1]!.seq;
    blocks = blocks.slice(cut);
  }
  return { blocks, floor, active };
}

/** The `size` blocks before `floor`. Answers them and the new floor. */
export async function loadEarlier(fetch: FetchTranscript, floor: number, size = WINDOW, maxPages = 50): Promise<{ blocks: Block[]; floor: number }> {
  const start = Math.max(0, floor - size);
  let cursor = start;
  let got: Block[] = [];
  for (let i = 0; i < maxPages; i++) {
    const p = await fetch(cursor, size);
    got = mergeBlocks(got, p.blocks.filter((b) => b.seq <= floor));
    const last = p.blocks.at(-1);
    if (p.complete || !last || last.seq >= floor || p.next_after <= cursor) break;
    cursor = p.next_after;
  }
  return { blocks: got, floor: start };
}

/** The `after` for the live stream: the newest block loaded, else the floor. */
export const streamAfter = (w: { blocks: Block[]; floor: number }): number => w.blocks.at(-1)?.seq ?? w.floor;
