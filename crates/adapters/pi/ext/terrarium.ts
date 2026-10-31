// Terrarium event extension for pi (tested on pi 0.85.1).
//
// terrarium-adapter-pi ships this file. `materialize_config` writes it to
// `<agent dir>/pi/terrarium.ts`, and `launch` loads it with
// `pi --no-extensions -e <path>` (explicit -e paths still load).
//
// Each pi event becomes one line on the runner socket, in the runner's hook
// message shape (docs/runner.md):
//
//   {"type":"hook","hook":"<pi event>","payload":{
//     "hook_event_name":"<pi event>","ts":<ms>,"pid":<n>,
//     "session_id":"…","session_file":"…","cwd":"…","model":"<provider>/<id>",
//     …event fields}}
//
// Env:
//   TERRARIUM_SOCKET     The runner socket (a Unix socket). The runner sets it.
//   TERRARIUM_EVENT_LOG  Optional. Also append each line to this file.
//   TERRARIUM_TRUST      Optional "yes" | "no". Answer pi's project_trust
//                        question, so that no trust dialog shows.
//
// The extension never blocks pi. Writes are fire-and-forget. While the
// socket connects, up to 1000 lines wait in memory. A lost connection
// opens again at the next event. No handler returns a decision, except
// `input` ("continue") and `project_trust` (only when TERRARIUM_TRUST is set).
import * as fs from "node:fs";
import * as net from "node:net";

type Any = Record<string, any>;

const SOCK = process.env.TERRARIUM_SOCKET ?? "";
const LOG = process.env.TERRARIUM_EVENT_LOG ?? "";
const TRUST = process.env.TERRARIUM_TRUST ?? "";
const BACKLOG_MAX = 1000;
// Long strings in tool input (for example the content of a `write` call)
// are cut to this length. The dashboard needs only a summary.
const INPUT_STRING_MAX = 2000;
// An `input` that starts no agent run (for example: no credential for the
// provider, so pi shows "No API key found" and stops) gives no
// `agent_settled`. After this time with no run and an idle pi, the
// extension sends a synthetic `agent_settled`, so the turn closes.
const NO_RUN_SETTLE_MS = 1500;

let conn: net.Socket | null = null;
let pendingNoRun: ReturnType<typeof setTimeout> | null = null;
let connecting = false;
const backlog: string[] = [];

function connect(): void {
  if (!SOCK || conn || connecting) return;
  connecting = true;
  const c = net.createConnection(SOCK);
  c.on("connect", () => {
    connecting = false;
    conn = c;
    while (backlog.length > 0) c.write(backlog.shift()!);
  });
  const drop = () => {
    connecting = false;
    if (conn === c) conn = null;
  };
  c.on("error", drop);
  c.on("close", drop);
  // The socket must not keep pi alive after the user quits.
  c.unref();
}

function safe<T>(f: () => T): T | undefined {
  try {
    return f();
  } catch {
    return undefined;
  }
}

function modelName(m: Any | undefined): string | undefined {
  return m ? `${m.provider}/${m.id}` : undefined;
}

function send(hook: string, ctx: Any | undefined, fields: Any = {}): void {
  const sm = ctx?.sessionManager;
  const payload = {
    hook_event_name: hook,
    ts: Date.now(),
    pid: process.pid,
    session_id: safe(() => sm?.getSessionId?.()),
    session_file: safe(() => sm?.getSessionFile?.()),
    cwd: ctx?.cwd ?? process.cwd(),
    model: modelName(ctx?.model),
    ...fields,
  };
  let line: string;
  try {
    line = JSON.stringify({ type: "hook", hook, payload }) + "\n";
  } catch {
    return;
  }
  if (LOG) {
    try {
      fs.appendFileSync(LOG, line);
    } catch {
      // A log that cannot be written is not an error for pi.
    }
  }
  if (!SOCK) return;
  if (conn) {
    conn.write(line);
    return;
  }
  if (backlog.length < BACKLOG_MAX) backlog.push(line);
  connect();
}

function clearNoRun(): void {
  if (pendingNoRun !== null) clearTimeout(pendingNoRun);
  pendingNoRun = null;
}

function text(content: unknown): string | undefined {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .filter((c) => c?.type === "text")
      .map((c) => c.text)
      .join("");
  }
  return undefined;
}

function cut(v: unknown, depth = 0): unknown {
  if (typeof v === "string") {
    return v.length > INPUT_STRING_MAX ? v.slice(0, INPUT_STRING_MAX) + "…" : v;
  }
  if (depth > 4 || v === null || typeof v !== "object") return v;
  if (Array.isArray(v)) return v.map((x) => cut(x, depth + 1));
  const out: Any = {};
  for (const [k, x] of Object.entries(v as Any)) out[k] = cut(x, depth + 1);
  return out;
}

export default function (pi: Any) {
  connect();

  // Start-up and session lifecycle.
  pi.on("project_trust", async (e: Any, ctx: Any) => {
    send("project_trust", ctx, { project_cwd: e?.cwd });
    if (TRUST === "yes" || TRUST === "no") return { trusted: TRUST, remember: false };
    return { trusted: "undecided" };
  });
  pi.on("session_start", async (e: Any, ctx: Any) =>
    send("session_start", ctx, {
      reason: e?.reason,
      previous_session_file: e?.previousSessionFile,
    }),
  );
  pi.on("session_shutdown", async (e: Any, ctx: Any) =>
    send("session_shutdown", ctx, {
      reason: e?.reason,
      target_session_file: e?.targetSessionFile,
    }),
  );
  // A compaction (docs/compat.md, "Compaction"): `/compact` (reason
  // "manual"), a full context ("threshold"), or an overflow. The handler of
  // `session_before_compact` returns nothing, so it never cancels the
  // compaction. `session_compact_failed` also fires for "Nothing to compact".
  pi.on("session_before_compact", async (e: Any, ctx: Any) => {
    send("session_before_compact", ctx, { reason: e?.reason });
  });
  pi.on("session_compact", async (e: Any, ctx: Any) =>
    send("session_compact", ctx, { reason: e?.reason }),
  );
  pi.on("session_compact_failed", async (e: Any, ctx: Any) => {
    send("session_compact_failed", ctx, {
      reason: e?.reason,
      error_message: e?.errorMessage,
      aborted: e?.aborted,
    });
  });
  pi.on("model_select", async (e: Any, ctx: Any) =>
    send("model_select", ctx, { new_model: modelName(e?.model), source: e?.source }),
  );

  // "Prompt submitted". `input` fires for each user line, also while a turn
  // runs (streaming_behavior "steer" or "followUp").
  pi.on("input", async (e: Any, ctx: Any) => {
    const behavior = e?.streamingBehavior ?? null;
    send("input", ctx, {
      prompt: e?.text,
      source: e?.source,
      streaming_behavior: behavior,
    });
    // A steer or follow-up joins the run that is open. It needs no check.
    if (behavior === null) {
      clearNoRun();
      pendingNoRun = setTimeout(() => {
        pendingNoRun = null;
        if (safe(() => ctx.isIdle()) === true) {
          send("agent_settled", ctx, { idle: true, synthetic: "no_agent_run" });
        }
      }, NO_RUN_SETTLE_MS);
      pendingNoRun.unref?.();
    }
    return { action: "continue" };
  });
  pi.on("before_agent_start", async (e: Any, ctx: Any) => {
    clearNoRun();
    send("before_agent_start", ctx, {
      prompt: e?.prompt,
      system_prompt_len: typeof e?.systemPrompt === "string" ? e.systemPrompt.length : undefined,
    });
  });

  // Agent runs. `agent_end` fires once per run, also for each automatic
  // retry. `agent_settled` is "turn ended": pi does not continue alone.
  pi.on("agent_start", async (_e: Any, ctx: Any) => {
    clearNoRun();
    send("agent_start", ctx);
  });
  pi.on("agent_end", async (e: Any, ctx: Any) =>
    send("agent_end", ctx, { message_count: e?.messages?.length }),
  );
  pi.on("agent_settled", async (_e: Any, ctx: Any) =>
    send("agent_settled", ctx, { idle: safe(() => ctx.isIdle()) }),
  );
  pi.on("turn_start", async (e: Any, ctx: Any) =>
    send("turn_start", ctx, { turn_index: e?.turnIndex }),
  );
  pi.on("turn_end", async (e: Any, ctx: Any) =>
    send("turn_end", ctx, {
      turn_index: e?.turnIndex,
      stop_reason: e?.message?.stopReason,
      tool_results: e?.toolResults?.length,
    }),
  );
  pi.on("message_end", async (e: Any, ctx: Any) => {
    const m = e?.message ?? {};
    if (m.role !== "assistant") return;
    send("message_end", ctx, {
      role: m.role,
      provider: m.provider,
      message_model: m.model,
      usage: m.usage,
      stop_reason: m.stopReason,
      error_message: m.errorMessage,
      last_assistant_message: text(m.content),
    });
  });

  // Tools. pi never asks before a tool.
  pi.on("tool_execution_start", async (e: Any, ctx: Any) =>
    send("tool_execution_start", ctx, {
      tool_name: e?.toolName,
      tool_call_id: e?.toolCallId,
      tool_input: cut(e?.args),
    }),
  );
  pi.on("tool_execution_end", async (e: Any, ctx: Any) =>
    send("tool_execution_end", ctx, {
      tool_name: e?.toolName,
      tool_call_id: e?.toolCallId,
      is_error: e?.isError,
    }),
  );

  // "Needs input": a blocking extension UI prompt is open.
  pi.on("ui_prompt_start", async (e: Any, ctx: Any) =>
    send("ui_prompt_start", ctx, { kind: e?.kind, title: e?.title }),
  );
  pi.on("ui_prompt_end", async (e: Any, ctx: Any) =>
    send("ui_prompt_end", ctx, { kind: e?.kind }),
  );
}
