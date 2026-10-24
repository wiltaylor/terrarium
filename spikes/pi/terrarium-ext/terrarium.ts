// Terrarium event extension for pi (tested on pi 0.85.1).
//
// Load with:  pi -e /path/to/terrarium.ts
// Env:
//   TERRARIUM_SOCKET     Unix socket path. Each event is one JSON line.
//   TERRARIUM_EVENT_LOG  Optional. Also append each line to this file.
//   TERRARIUM_TRUST      Optional "yes" | "no": answer pi's project_trust
//                        question so no trust prompt shows.
//   TERRARIUM_FULL_PROMPT Optional "1": put the full system prompt in
//                        before_agent_start (spikes only; it is large).
//
// Line shape (stable keys, Claude-hook-like):
//   {"hook_event_name": "<pi event>", "ts": <ms>, "pid": <n>,
//    "session_id": "...", "session_file": "...", "cwd": "...",
//    "model": "<provider>/<id>", ...event-specific fields}
//
// The extension never blocks pi: writes are fire-and-forget, a lost socket
// is retried on the next event, and no handler returns a decision except
// project_trust (only when TERRARIUM_TRUST is set).
import * as net from "node:net";
import * as fs from "node:fs";

type Any = Record<string, any>;

const SOCK = process.env.TERRARIUM_SOCKET || "";
const LOG = process.env.TERRARIUM_EVENT_LOG || "";
const TRUST = process.env.TERRARIUM_TRUST || "";
const FULL_PROMPT = process.env.TERRARIUM_FULL_PROMPT === "1";

let conn: net.Socket | null = null;
let connecting = false;
const backlog: string[] = [];

function connect(): void {
  if (!SOCK || conn || connecting) return;
  connecting = true;
  const c = net.createConnection(SOCK);
  c.on("connect", () => {
    connecting = false;
    conn = c;
    while (backlog.length) c.write(backlog.shift()!);
  });
  c.on("error", () => { connecting = false; conn = null; });
  c.on("close", () => { connecting = false; conn = null; });
  c.unref();
}

function send(obj: Any): void {
  let line: string;
  try { line = JSON.stringify(obj) + "\n"; } catch { return; }
  if (LOG) { try { fs.appendFileSync(LOG, line); } catch { /* ignore */ } }
  if (!SOCK) return;
  if (conn) { conn.write(line); return; }
  if (backlog.length < 1000) backlog.push(line);
  connect();
}

function base(name: string, ctx: Any | undefined): Any {
  const sm = ctx?.sessionManager;
  const m = ctx?.model;
  return {
    hook_event_name: name,
    ts: Date.now(),
    pid: process.pid,
    session_id: safe(() => sm?.getSessionId?.()),
    session_file: safe(() => sm?.getSessionFile?.()),
    cwd: ctx?.cwd ?? process.cwd(),
    model: m ? `${m.provider}/${m.id}` : undefined,
  };
}

function safe<T>(f: () => T): T | undefined {
  try { return f(); } catch { return undefined; }
}

function text(content: any): string | undefined {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content.filter((c) => c?.type === "text").map((c) => c.text).join("");
  }
  return undefined;
}

export default function (pi: Any) {
  connect();

  pi.on("project_trust", async (event: Any, ctx: Any) => {
    send({ ...base("project_trust", ctx), project_cwd: event?.cwd });
    if (TRUST === "yes" || TRUST === "no") return { trusted: TRUST, remember: false };
    return { trusted: "undecided" };
  });

  pi.on("session_start", async (e: Any, ctx: Any) =>
    send({ ...base("session_start", ctx), reason: e?.reason, previous_session_file: e?.previousSessionFile }));
  pi.on("session_shutdown", async (e: Any, ctx: Any) =>
    send({ ...base("session_shutdown", ctx), reason: e?.reason, target_session_file: e?.targetSessionFile }));
  pi.on("session_compact", async (e: Any, ctx: Any) =>
    send({ ...base("session_compact", ctx), reason: e?.reason }));

  // "prompt submitted": input fires for every user line, also while busy
  // (streaming_behavior "steer" or "followUp").
  pi.on("input", async (e: Any, ctx: Any) => {
    send({ ...base("input", ctx), prompt: e?.text, source: e?.source,
           streaming_behavior: e?.streamingBehavior ?? null });
    return { action: "continue" };
  });
  pi.on("before_agent_start", async (e: Any, ctx: Any) => {
    send({ ...base("before_agent_start", ctx), prompt: e?.prompt,
           system_prompt_len: typeof e?.systemPrompt === "string" ? e.systemPrompt.length : undefined,
           system_prompt_head: typeof e?.systemPrompt === "string" ? e.systemPrompt.slice(0, 200) : undefined,
           system_prompt: FULL_PROMPT ? e?.systemPrompt : undefined,
           append_system_prompt: FULL_PROMPT ? e?.systemPromptOptions?.appendSystemPrompt : undefined });
  });
  pi.on("agent_start", async (_e: Any, ctx: Any) => send(base("agent_start", ctx)));
  pi.on("agent_end", async (e: Any, ctx: Any) =>
    send({ ...base("agent_end", ctx), message_count: e?.messages?.length }));
  // "turn ended" for Terrarium: nothing more will run without new input.
  pi.on("agent_settled", async (_e: Any, ctx: Any) =>
    send({ ...base("agent_settled", ctx), idle: safe(() => ctx.isIdle()) }));
  pi.on("turn_start", async (e: Any, ctx: Any) =>
    send({ ...base("turn_start", ctx), turn_index: e?.turnIndex }));
  pi.on("turn_end", async (e: Any, ctx: Any) =>
    send({ ...base("turn_end", ctx), turn_index: e?.turnIndex,
           stop_reason: e?.message?.stopReason, tool_results: e?.toolResults?.length }));
  pi.on("message_end", async (e: Any, ctx: Any) => {
    const m = e?.message ?? {};
    if (m.role !== "assistant") return;
    send({ ...base("message_end", ctx), role: m.role, provider: m.provider, message_model: m.model,
           usage: m.usage, stop_reason: m.stopReason, last_assistant_message: text(m.content) });
  });
  pi.on("tool_execution_start", async (e: Any, ctx: Any) =>
    send({ ...base("tool_execution_start", ctx), tool_name: e?.toolName, tool_call_id: e?.toolCallId, tool_input: e?.args }));
  pi.on("tool_execution_end", async (e: Any, ctx: Any) =>
    send({ ...base("tool_execution_end", ctx), tool_name: e?.toolName, tool_call_id: e?.toolCallId, is_error: e?.isError }));
  // "needs input": a blocking extension UI prompt is open.
  pi.on("ui_prompt_start", async (e: Any, ctx: Any) =>
    send({ ...base("ui_prompt_start", ctx), kind: e?.kind, title: e?.title }));
  pi.on("ui_prompt_end", async (e: Any, ctx: Any) => send({ ...base("ui_prompt_end", ctx), kind: e?.kind }));
  pi.on("model_select", async (e: Any, ctx: Any) =>
    send({ ...base("model_select", ctx), new_model: e?.model ? `${e.model.provider}/${e.model.id}` : undefined, source: e?.source }));
}
