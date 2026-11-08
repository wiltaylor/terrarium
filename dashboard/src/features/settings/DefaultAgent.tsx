import { createResource, createSignal, For, Show } from "solid-js";
import { ErrorBanner } from "../../components/ui";
import { agents, api } from "../../stores";

export function DefaultAgent() {
  const [preferences, { mutate }] = createResource(() => api.account.preferences());
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const choices = () => [...new Set([
    ...agents.state.agents.map((a) => `${a.project}/${a.name}`),
    ...(preferences()?.default_agent ? [preferences()!.default_agent!] : []),
  ])].sort();
  const save = async (value: string) => {
    setBusy(true);
    setError(null);
    try {
      mutate(await api.account.setPreferences({ default_agent: value || null }));
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };
  return <section class="card-box stack" aria-label="Default agent">
    <h2>Default agent</h2>
    <p>The preselected recipient for Ask an agent and Message an agent, on every device. You can pick another agent in either dialog.</p>
    <ErrorBanner error={error() ?? preferences.error} />
    <Show when={!preferences.loading && !preferences.error}>
      <label>Agent
        <select aria-label="Default agent" value={preferences()?.default_agent ?? ""} disabled={busy()} onChange={(e) => void save(e.currentTarget.value)}>
          <option value="">No default</option>
          <For each={choices()}>{(name) => <option value={name} selected={preferences()?.default_agent === name}>{name}</option>}</For>
        </select>
      </label>
    </Show>
  </section>;
}
