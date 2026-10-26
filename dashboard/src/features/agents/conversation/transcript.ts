// The signals of one open transcript: the window of blocks, "load earlier",
// and the live stream. The logic is in ./model; this file only holds state.

import { createEffect, createMemo, createSignal, on, onCleanup } from "solid-js";
import type { TranscriptStream, TranscriptStreamState } from "../../../api/sse";
import { isEndStatus } from "../../../api/sse";
import type { Block, Session, TranscriptPage } from "../../../api/types";
import { loadEarlier, loadNewest, mergeBlocks, streamAfter, WINDOW } from "./model";

export type Change = { kind: "reset" | "append" | "update" | "prepend"; count: number };

export interface TranscriptSource {
  agentId: () => string | undefined;
  session: () => Session | undefined;
  fetch: (agentId: string, session: string, after: number, limit: number) => Promise<TranscriptPage>;
  stream: (agentId: string, session: string, after: number) => TranscriptStream;
  /** After the blocks change, for the scroll logic. */
  onChange?: (c: Change) => void;
  size?: number;
}

export function createTranscript(src: TranscriptSource) {
  const size = src.size ?? WINDOW;
  const [blocks, setBlocks] = createSignal<Block[]>([]);
  const [floor, setFloor] = createSignal(0);
  const [active, setActive] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [loadingEarlier, setLoadingEarlier] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [streamState, setStreamState] = createSignal<TranscriptStreamState | "idle">("idle");
  const [streamError, setStreamError] = createSignal<unknown>(null);
  const [lastStatus, setLastStatus] = createSignal<string | null>(null);
  let stream: TranscriptStream | null = null;
  let generation = 0;

  const key = createMemo(() => {
    const a = src.agentId();
    const s = src.session()?.id;
    return a && s ? `${a}\u0000${s}` : null;
  });

  const closeStream = () => {
    stream?.close();
    stream = null;
  };

  const open = async (gen: number, agentId: string, session: Session) => {
    setLoading(true);
    try {
      const w = await loadNewest((after, limit) => src.fetch(agentId, session.id, after, limit), session.blocks, size);
      if (gen !== generation) return;
      setBlocks(w.blocks);
      setFloor(w.floor);
      setActive(w.active);
      setError(null);
      src.onChange?.({ kind: "reset", count: w.blocks.length });
      // A session that has ended does not grow; there is nothing to stream.
      if (!w.active && session.ended_at) return;
      const s = src.stream(agentId, session.id, streamAfter(w));
      stream = s;
      s.onState((st, e) => {
        if (gen !== generation) return;
        setStreamState(st);
        setStreamError(st === "failed" ? e : null);
      });
      s.subscribe((f) => {
        if (gen !== generation) return;
        if (f.type === "status") {
          setLastStatus(f.status);
          if (isEndStatus(f.status)) setActive(false);
          return;
        }
        if (f.block.seq <= floor()) return; // before the window
        const had = blocks().some((b) => b.seq === f.block.seq);
        setBlocks((cur) => mergeBlocks(cur, [f.block]));
        src.onChange?.({ kind: had ? "update" : "append", count: had ? 0 : 1 });
      });
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  createEffect(
    on(key, () => {
      const gen = ++generation;
      closeStream();
      setBlocks([]);
      setFloor(0);
      setActive(false);
      setError(null);
      setStreamState("idle");
      setStreamError(null);
      setLastStatus(null);
      const a = src.agentId();
      const s = src.session();
      if (a && s) void open(gen, a, s);
    }),
  );
  onCleanup(() => {
    generation++;
    closeStream();
  });

  const earlier = async () => {
    const a = src.agentId();
    const s = src.session();
    if (!a || !s || floor() <= 0 || loadingEarlier()) return;
    const gen = generation;
    setLoadingEarlier(true);
    try {
      const r = await loadEarlier((after, limit) => src.fetch(a, s.id, after, limit), floor(), size);
      if (gen !== generation) return;
      setBlocks((cur) => mergeBlocks(cur, r.blocks));
      setFloor(r.floor);
      src.onChange?.({ kind: "prepend", count: r.blocks.length });
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoadingEarlier(false);
    }
  };

  return {
    blocks,
    active,
    loading,
    loadingEarlier,
    error,
    streamState,
    streamError,
    lastStatus,
    hasEarlier: () => floor() > 0,
    loadEarlier: earlier,
  };
}
