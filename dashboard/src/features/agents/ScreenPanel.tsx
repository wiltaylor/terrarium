import { createEffect, createSignal, onCleanup, onMount, Show } from "solid-js";
import type { AgentScreen } from "../../api/types";
import { ErrorBanner, useNow } from "../../components/ui";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import { SCREEN_REFRESH_MS, screenText, screenTitle, shouldAutoRefresh } from "./screen";

/**
 * A read-only text snapshot of the agent's harness screen (docs/api.md §6.5).
 * It does not attach to the PTY. The text goes into a <pre> as a text node,
 * never as HTML.
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
    </div>
  );
}
