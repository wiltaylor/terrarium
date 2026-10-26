// Seed transcripts for mock mode (docs/api.md §6, transcripts): one
// conversation for each session, in the block shape the daemon reads from
// each harness's own transcript file. The four current sessions of lead
// (Claude), adapter-codex (Codex), reviewer (pi), and shop-lead (Copilot)
// show every kind of block, a failed tool result, redacted thinking, and a
// mail-pointer turn. runner-dev's session is long (paging). lead's old
// session carries hostile markdown (the XSS check).

import { XSS_DOCUMENT } from "../../lib/xss-payloads";
import type { Agent, Block, BlockUsage, Harness, Session } from "../types";

export type Draft = Omit<Block, "id" | "session" | "seq" | "source" | "at"> & { at?: string | null };

/** The transcript file of a harness session, as each harness names it. */
export function transcriptFile(harness: Harness, hsid: string): string {
  switch (harness) {
    case "claude":
      return `~/.claude/projects/-workspace/${hsid}.jsonl`;
    case "codex":
      return `~/.codex/sessions/2026/10/26/rollout-2026-10-26T09-12-44-${hsid}.jsonl`;
    case "copilot":
      return `~/.copilot/session-state/${hsid}.jsonl`;
    case "pi":
      return `~/.pi/agent/sessions/--workspace--/2026-10-26T09-12-44_${hsid}.jsonl`;
  }
}

/** Append a block to a transcript with the next `seq`, a block ID, and a source line. */
export function appendBlock(list: Block[], session: Session, harness: Harness, d: Draft, at: string | null): Block {
  const seq = (list.at(-1)?.seq ?? 0) + 1;
  const b: Block = {
    id: `${session.id}-b${seq}`,
    session: session.id,
    seq,
    ...d,
    at: d.at === undefined ? at : d.at,
    source: { harness, file: transcriptFile(harness, session.harness_session_id ?? session.id), line: seq * 2 - 1 },
  };
  list.push(b);
  return b;
}

const MODELS: Record<Harness, string> = { claude: "claude-sonnet-4-5", codex: "gpt-5-codex", copilot: "gpt-5", pi: "anthropic/claude-sonnet" };
const use = (input: number, output: number, cache_read = 0, cache_write = 0): BlockUsage => ({ input, output, cache_read, cache_write });

const user = (text: string, origin: Block["origin"] = "operator"): Draft => ({ role: "user", kind: "text", text, origin });
const say = (text: string, model: string, usage?: BlockUsage): Draft => ({ role: "assistant", kind: "text", text, model, ...(usage ? { usage } : {}) });
const think = (text: string, model: string): Draft => ({ role: "assistant", kind: "thinking", text, model });
const redactedThinking = (model: string): Draft => ({ role: "assistant", kind: "thinking", redacted: true, model });
const call = (id: string, name: string, input: unknown, summary: string, model: string, usage?: BlockUsage): Draft => ({
  role: "assistant",
  kind: "tool_call",
  tool: { id, name, input, summary },
  model,
  ...(usage ? { usage } : {}),
});
const result = (tool_id: string, output: string, is_error = false, truncated = false): Draft => ({
  role: "user",
  kind: "tool_result",
  result: { tool_id, output, is_error, ...(truncated ? { truncated } : {}) },
});
const note = (text: string): Draft => ({ role: "system", kind: "system", text });

const LONG_LOG = Array.from({ length: 60 }, (_, i) => `test runner::tests::case_${String(i).padStart(2, "0")} ... ok`).join("\n");

function claudeLead(m: string): Draft[] {
  return [
    note("Session resumed (claude --resume)."),
    user("New mail from operator (thread T-0007). Run `terrarium mail read --thread T-0007`.", "agent_mail_pointer"),
    think("The operator wants a plan for the transcript view. I should read the mail first, then the board.", m),
    call("toolu_01", "Bash", { command: "terrarium mail read --thread T-0007" }, "terrarium mail read --thread T-0007", m, use(4120, 96, 18000, 2100)),
    result("toolu_01", "From: operator\nSubject: Transcript view\n\nPlease split the transcript work into cards: daemon reader, API, dashboard view."),
    redactedThinking(m),
    call("toolu_02", "Bash", { command: "terrarium card create --title 'Transcript reader: claude, codex, copilot, pi' --column ready" }, "terrarium card create … --column ready", m, use(410, 120, 22000, 0)),
    result("toolu_02", "Created TER-21 in ready."),
    call("toolu_03", "Bash", { command: "terrarium card create --title 'Transcript API + SSE stream' --column ready" }, "terrarium card create … --column ready", m, use(380, 110, 22400, 0)),
    result("toolu_03", "error: card prefix TER is at its WIP limit for `ready` (4/4)\nhint: move a card out of ready first", true),
    say(
      "I made **TER-21** for the transcript reader. The second card failed: `ready` is at its WIP limit.\n\nNext steps:\n\n1. Move TER-15 to `in_progress` (adapter-codex has it).\n2. Make the API card again.\n\n| Card | State |\n|---|---|\n| TER-21 | ready |\n| API card | blocked by WIP limit |",
      m,
      use(520, 260, 22800, 0),
    ),
    user("Fine. Leave the API card for now, I will move TER-15 myself."),
    say("Understood. I will wait for your move, then make the API card. See [[designs/transcripts]] for the notes.", m, use(140, 38, 23100, 0)),
  ];
}

function codexAdapter(m: string): Draft[] {
  return [
    note("Session started. Model gpt-5-codex, reasoning effort medium."),
    user("Pick up TER-15: the codex adapter must read the rollout file."),
    think("**Plan.** Find the rollout reader, check the event types, add a test with a fixture.", m),
    call("call_a1", "shell", { command: ["bash", "-lc", "rg -n 'rollout' crates/"], workdir: "/workspace" }, "rg -n 'rollout' crates/", m, use(6100, 140, 0, 0)),
    result("call_a1", "crates/terrarium-adapters/src/codex.rs:88:    // TODO: rollout reader\ncrates/terrarium-adapters/src/codex.rs:212:fn rollout_path(home: &Path) -> PathBuf {"),
    call("call_a2", "apply_patch", { patch: "*** Begin Patch\n*** Update File: crates/terrarium-adapters/src/codex.rs\n@@\n-    // TODO: rollout reader\n+    let reader = RolloutReader::open(&path)?;\n*** End Patch" }, "apply_patch codex.rs (+1 −1)", m, use(820, 210, 5400, 0)),
    result("call_a2", "Success. Updated the following files:\nM crates/terrarium-adapters/src/codex.rs"),
    call("call_a3", "shell", { command: ["bash", "-lc", "cargo test -p terrarium-adapters codex"], workdir: "/workspace" }, "cargo test -p terrarium-adapters codex", m, use(300, 60, 6100, 0)),
    result("call_a3", `${LONG_LOG}\n\n… 412 more lines`, false, true),
    call("call_a4", "shell", { command: ["bash", "-lc", "cargo clippy -p terrarium-adapters -- -D warnings"], workdir: "/workspace" }, "cargo clippy -p terrarium-adapters -- -D warnings", m),
    result("call_a4", "error: unused variable: `reader`\n  --> crates/terrarium-adapters/src/codex.rs:88:9\n   |\n88 |     let reader = RolloutReader::open(&path)?;\n   |         ^^^^^^ help: if this is intentional, prefix it with an underscore: `_reader`\n\nerror: could not compile `terrarium-adapters` (lib) due to 1 previous error", true),
    say("Tests pass, but clippy fails on an unused `reader`. I am blocked on the reader API: `RolloutReader` has no `blocks()` yet. I asked the lead by mail.", m, use(940, 180, 6900, 0)),
    user("New mail from lead (thread T-0011). Run `terrarium mail read --thread T-0011`.", "agent_mail_pointer"),
  ];
}

function piReviewer(m: string): Draft[] {
  return [
    note("pi 1.4.0 · provider openrouter · model anthropic/claude-sonnet"),
    user("Review the diff of TER-12 and write your findings to the card.", "system"),
    think("Read the diff first. Then check the tests.", m),
    call("pi_1", "bash", { command: "git diff main...ter-12 --stat" }, "git diff main...ter-12 --stat", m, use(2400, 52, 0, 0)),
    result("pi_1", " crates/terrarium-runner/src/pty.rs | 48 +++++++++-----\n crates/terrarium-runner/src/lib.rs |  6 +-\n 2 files changed, 34 insertions(+), 20 deletions(-)"),
    call("pi_2", "read", { path: "crates/terrarium-runner/src/pty.rs", offset: 120, limit: 40 }, "read pty.rs:120-160", m, use(300, 40, 2400, 0)),
    result("pi_2", "120 pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {\n121     self.master.resize(PtySize { cols, rows, ..Default::default() })\n122 }"),
    say("Two findings:\n\n- `resize` does not clamp `cols` to at least 1. A zero size panics in `portable-pty`.\n- The new code has no test for the resize path.\n\nI will write these to the card.", m, use(1100, 210, 2700, 0)),
  ];
}

function copilotShop(m: string): Draft[] {
  return [
    note("GitHub Copilot CLI 1.0.103 · session resumed"),
    user("New mail from operator (thread T-0003). Run `terrarium mail read --thread T-0003`.", "agent_mail_pointer"),
    call("cp_1", "bash", { command: "terrarium mail read --thread T-0003", description: "Read the operator's mail" }, "Read the operator's mail", m, use(3900, 70, 0, 0)),
    result("cp_1", "From: operator\nSubject: Checkout\n\nThe checkout page 500s on an empty cart. Please fix before the demo."),
    think("Find the checkout handler and the empty-cart path.", m),
    call("cp_2", "view", { path: "src/routes/checkout.rs" }, "view src/routes/checkout.rs", m, use(220, 30, 3900, 0)),
    result("cp_2", "error: path does not exist: src/routes/checkout.rs", true),
    call("cp_3", "bash", { command: "rg -l 'fn checkout' src", description: "Find the checkout handler" }, "Find the checkout handler", m, use(260, 40, 4100, 0)),
    result("cp_3", "src/handlers/checkout.rs"),
    call("cp_4", "edit", { path: "src/handlers/checkout.rs", old_str: "let total = cart.items[0].price;", new_str: "let Some(first) = cart.items.first() else { return Ok(Redirect::to(\"/cart\")) };" }, "edit src/handlers/checkout.rs", m, use(400, 90, 4300, 0)),
    result("cp_4", "File src/handlers/checkout.rs updated."),
    say("Fixed: an empty cart now redirects to `/cart` instead of indexing `items[0]`. WEB-4 has the details.", m, use(500, 64, 4700, 0)),
  ];
}

/** A long working session: many small tool turns, for paging. */
function longSession(m: string, turns: number): Draft[] {
  const out: Draft[] = [note("Session started."), user("Work through TER-12: the runner PTY resize.")];
  for (let i = 1; i <= turns; i++) {
    out.push(call(`toolu_l${i}`, i % 3 === 0 ? "Edit" : i % 3 === 1 ? "Read" : "Bash", { step: i, path: `crates/terrarium-runner/src/file_${i % 17}.rs` }, `step ${i}: file_${i % 17}.rs`, m, use(200 + i, 20, 8000, 0)));
    out.push(result(`toolu_l${i}`, `ok (step ${i})`, i % 41 === 0));
    if (i % 25 === 0) out.push(say(`Progress: ${i} steps done.`, m, use(100, 12, 8000, 0)));
  }
  out.push(say("Still on it. The resize path now clamps to at least one column.", m, use(300, 30, 8100, 0)));
  return out;
}

function shortSession(m: string, agentName: string): Draft[] {
  return [
    note("Session started."),
    user(`Hello ${agentName}. Check your inbox and the board.`),
    call("t_1", "Bash", { command: "terrarium card ls --mine" }, "terrarium card ls --mine", m, use(1200, 30, 0, 0)),
    result("t_1", "No cards."),
    say("Nothing is assigned to me. I am idle.", m, use(200, 14, 1200, 0)),
  ];
}

/** Hostile markdown in an assistant turn. It must render inert (lib/sanitize). */
function xssSession(m: string): Draft[] {
  return [note("Session started."), user("Print the XSS test document."), say(`Here it is:\n\n${XSS_DOCUMENT}`, m, use(900, 1400, 0, 0))];
}

/** One transcript for each session. `at(i, n)` spreads the times of n blocks over the session. */
export function seedTranscripts(agents: Agent[], sessions: Session[]): Record<string, Block[]> {
  const out: Record<string, Block[]> = {};
  for (const s of sessions) {
    const a = agents.find((x) => x.id === s.agent);
    if (!a) continue;
    const m = a.model ?? MODELS[a.harness];
    let drafts: Draft[];
    if (s.current) {
      if (a.name === "lead") drafts = claudeLead(m);
      else if (a.name === "adapter-codex") drafts = codexAdapter(m);
      else if (a.name === "reviewer") drafts = piReviewer(m);
      else if (a.name === "shop-lead") drafts = copilotShop(m);
      else if (a.name === "runner-dev") drafts = longSession(m, 300);
      else drafts = shortSession(m, a.name);
    } else {
      drafts = a.name === "lead" ? xssSession(m) : shortSession(m, a.name);
    }
    const start = Date.parse(s.started_at);
    const end = s.ended_at ? Date.parse(s.ended_at) : start + 3 * 3600_000 - 60_000;
    const list: Block[] = [];
    drafts.forEach((d, i) => {
      // Codex rollout lines for the session note carry no time.
      const at = a.harness === "codex" && d.kind === "system" ? null : new Date(start + ((end - start) * i) / Math.max(drafts.length, 1)).toISOString();
      appendBlock(list, s, a.harness, d, at);
    });
    out[s.id] = list;
  }
  return out;
}

/** The title the daemon gives a session: its first user text, one line, 80 characters. */
export function titleOf(list: Block[] | undefined): string | null {
  const first = list?.find((b) => b.role === "user" && b.kind === "text" && b.text)?.text;
  if (!first) return null;
  const line = first.split("\n")[0]!.trim();
  return line.length > 80 ? `${line.slice(0, 79)}…` : line;
}

/** The fake harness's answer to an operator turn: thinking, a tool call, its result, and a reply that grows. */
export function fakeReply(text: string, model: string): { thinking: Draft; call: Draft; result: Draft; reply: Draft; full: string } {
  const id = `toolu_echo_${Math.random().toString(36).slice(2, 8)}`;
  const full = `You said: “${text.length > 200 ? `${text.slice(0, 200)}…` : text}”\n\nI checked the workspace. **Nothing else** needs doing for this turn.`;
  return {
    thinking: think("The operator typed a turn. Look at the workspace, then answer.", model),
    call: call(id, "Bash", { command: "git status --short" }, "git status --short", model, use(1500, 40, 9000, 0)),
    result: result(id, " M dashboard/src/api/client.ts\n?? notes.md"),
    reply: say(full.slice(0, 24), model, use(320, 64, 10500, 0)),
    full,
  };
}

export const mockModel = (a: Agent): string => a.model ?? MODELS[a.harness];
