// The `active` flag of a transcript (TER-44): it follows the daemon's
// status frames both ways (`ended` while the agent does not run, its status
// once it runs again), and `reopen` reads the session again.
import { createRoot } from "solid-js";
import { describe, expect, it } from "vitest";
import type { TranscriptStream } from "../../../api/sse";
import type { TranscriptFrame } from "../../../api/types";
import type { Session, TranscriptPage } from "../../../api/types";
import { createTranscript } from "./transcript";

const tick = () => new Promise((r) => setTimeout(r, 0));

function fakeStream() {
  const subs: ((f: TranscriptFrame) => void)[] = [];
  const s = {
    subscribe: (fn: (f: TranscriptFrame) => void) => (subs.push(fn), () => {}),
    onState: () => () => {},
    close: () => {},
  } as unknown as TranscriptStream;
  return { s, push: (f: TranscriptFrame) => subs.forEach((fn) => fn(f)) };
}

describe("the active flag of a transcript", () => {
  it("goes false at `ended` and true again at a live status, and reopen reads the page again", async () => {
    let pages = 0;
    let active = true;
    const streams: ReturnType<typeof fakeStream>[] = [];
    const session = { id: "S1", current: true, blocks: 0 } as unknown as Session;
    await createRoot(async (dispose) => {
      const t = createTranscript({
        agentId: () => "A1",
        session: () => session,
        fetch: async (): Promise<TranscriptPage> => {
          pages++;
          return { session: "S1", harness: "claude", active, blocks: [], next_after: 0, complete: true } as unknown as TranscriptPage;
        },
        stream: () => {
          const f = fakeStream();
          streams.push(f);
          return f.s;
        },
      });
      await tick();
      await tick();
      expect(t.active()).toBe(true);
      streams[0]!.push({ type: "status", status: "ended" });
      expect(t.active()).toBe(false);
      streams[0]!.push({ type: "status", status: "idle" });
      expect(t.active()).toBe(true);
      // Reopen: a new page and a new stream.
      active = false;
      t.reopen();
      await tick();
      await tick();
      expect(pages).toBe(2);
      expect(t.active()).toBe(false);
      dispose();
    });
  });
});
