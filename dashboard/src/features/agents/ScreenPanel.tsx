import { createEffect, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { AgentScreen } from "../../api/types";
import { ErrorBanner, useNow } from "../../components/ui";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import { AFTER_INPUT_MS, KEY_BAR, rawText, SCREEN_REFRESH_MS, screenText, screenTitle, shouldAutoRefresh } from "./screen";

/**
 * A text snapshot of the agent's harness screen (docs/api.md §6.5). It does
 * not attach to the PTY. The text goes into a <pre> as a text node, never as
 * HTML. Under it, a key bar and a "send raw" field (D3) let the operator close
 * a dialog, leave a wrong mode, or run a harness command such as `/compact`.
 * The dashboard user is the operator, who has the rights of the PTY attach.
 */
export function ScreenPanel(props: { agentId: string; running: boolean }) {
  const [snap, setSnap] = createSignal<AgentScreen | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [loading, setLoading] = createSignal(false);
  const [auto, setAuto] = createSignal(true);
  const [pageVisible, setPageVisible] = createSignal(document.visibilityState !== "hidden");
  const now = useNow(5000);

  const load = async () => {
    if (loading()) return;
    setLoading(true);
    try {
      setSnap(await api.agents.screen(props.agentId));
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      setLoading(false);
    }
  };

  onMount(() => {
    const onVisibility = () => setPageVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", onVisibility);
    onCleanup(() => document.removeEventListener("visibilitychange", onVisibility));
  });

  // A load at the start and when the agent starts. A stopped agent has no screen (409).
  createEffect(() => {
    if (props.running) void load();
  });

  createEffect(() => {
    if (!shouldAutoRefresh({ auto: auto(), pageVisible: pageVisible(), running: props.running })) return;
    const t = setInterval(() => void load(), SCREEN_REFRESH_MS);
    onCleanup(() => clearInterval(t));
  });

  const [raw, setRaw] = createSignal("");
  const [sending, setSending] = createSignal(false);
  const [inputError, setInputError] = createSignal<unknown>(null);

  const afterInput = () => setTimeout(() => void load(), AFTER_INPUT_MS);

  const sendKey = async (key: string) => {
    setSending(true);
    try {
      await api.agents.keys(props.agentId, [key]);
      setInputError(null);
      afterInput();
    } catch (e) {
      setInputError(e);
    } finally {
      setSending(false);
    }
  };

  const sendRaw = async (e: SubmitEvent) => {
    e.preventDefault();
    const text = rawText(raw());
    if (text === null) return;
    setSending(true);
    try {
      await api.agents.sendRaw(props.agentId, text);
      setRaw("");
      setInputError(null);
      afterInput();
    } catch (err) {
      setInputError(err);
    } finally {
      setSending(false);
    }
  };

  return (
    <div class="stack" style={{ gap: "8px" }}>
      <div class="row" style={{ gap: "8px", "align-items": "center", "flex-wrap": "wrap" }}>
        <button onClick={() => void load()} disabled={loading()}>
          Refresh
        </button>
        <label class="row" style={{ gap: "4px" }}>
          <input type="checkbox" checked={auto()} onChange={(e) => setAuto(e.currentTarget.checked)} />
          Every 5 s
        </label>
        <Show when={snap()}>
          {(s) => (
            <span class="muted small">
              {screenTitle(s())} · taken {ago(s().at, now())}
            </span>
          )}
        </Show>
      </div>
      <ErrorBanner error={error()} />
      <Show when={!props.running}>
        <p class="muted">The agent is not running. Start it to see its screen.{snap() ? " The last snapshot is below." : ""}</p>
      </Show>
      <Show when={snap()}>
        {(s) => <pre class="mono screen-snapshot">{screenText(s())}</pre>}
      </Show>
      <Show when={props.running}>
        <div class="row" style={{ gap: "4px", "align-items": "center", "flex-wrap": "wrap" }} aria-label="Keys">
          <span class="muted small">Keys:</span>
          <For each={KEY_BAR}>
            {(k) => (
              <button type="button" class="small" title={k.title} disabled={sending()} onClick={() => void sendKey(k.key)}>
                {k.label}
              </button>
            )}
          </For>
        </div>
        <form class="row" style={{ gap: "8px", "align-items": "center", "flex-wrap": "wrap" }} onSubmit={(e) => void sendRaw(e)}>
          <input
            class="mono"
            style={{ flex: "1 1 16em", "min-width": "0" }}
            placeholder="Send raw, for example /compact"
            aria-label="Send raw text"
            value={raw()}
            onInput={(e) => setRaw(e.currentTarget.value)}
          />
          <button type="submit" disabled={sending() || rawText(raw()) === null} title="Type the text as-is, at once: a harness command runs">
            Send raw
          </button>
        </form>
        <p class="muted small">
          Keys and raw text go to the harness at once. Read the screen first. Text that starts with / or ! runs as a harness command.
        </p>
        <ErrorBanner error={inputError()} />
      </Show>
    </div>
  );
}
