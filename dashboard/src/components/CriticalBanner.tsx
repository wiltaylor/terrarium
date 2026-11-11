import { useInstance } from "../instance";
// The critical banner on every page, and the flashing browser tab, while an
// open item of the Inbox, or an open question inside a thread (TER-488), has
// the priority `critical` (docs/api.md §6.8). The inbox store follows the
// `inbox` and `question` events, so the banner comes and goes with no reload.
// The logic is in features/inbox/critical.

import { A } from "../instance/navigation";
import { createEffect, createMemo, createSignal, onCleanup, onMount, Show } from "solid-js";
import { announce, browserFlashEnv, createTitleFlasher, criticalBanner } from "../features/inbox/critical";


export function CriticalBanner() {
  const { inbox } = useInstance();
  const banner = createMemo(() => criticalBanner(inbox.open()));
  const id = () => banner()?.item.id ?? null;

  // role="alert" on the first appearance of an item only, not on each refresh.
  let seen = new Set<string>();
  const [alerting, setAlerting] = createSignal(false);
  createEffect(() => {
    const r = announce(seen, id());
    seen = r.seen;
    setAlerting(r.announce);
  });

  const [collapsed, setCollapsed] = createSignal(false);
  createEffect(() => {
    // A new newest item opens the banner again.
    void id();
    setCollapsed(false);
  });

  // The flashing tab: while a critical item is open and the page is hidden or has no focus.
  onMount(() => {
    const flasher = createTitleFlasher(browserFlashEnv());
    const attentive = () => document.visibilityState === "visible" && document.hasFocus();
    const sync = () => flasher.update({ critical: banner() !== null, attentive: attentive() });
    createEffect(sync);
    document.addEventListener("visibilitychange", sync);
    window.addEventListener("focus", sync);
    window.addEventListener("blur", sync);
    onCleanup(() => {
      document.removeEventListener("visibilitychange", sync);
      window.removeEventListener("focus", sync);
      window.removeEventListener("blur", sync);
      flasher.dispose();
    });
  });

  return (
    <Show when={banner()}>
      {(b) => (
        <div class="critical-banner" classList={{ collapsed: collapsed() }} role={alerting() ? "alert" : undefined} aria-label="Critical notification">
          <span class="critical-icon" aria-hidden="true">
            ⚠
          </span>
          <span class="critical-body">
            <strong>Critical notification</strong>
            <Show when={!collapsed()}>
              <span class="critical-from"> · {b().from}</span>
              {/* The agent's own words: a text node only. */}
              <span class="critical-text">{b().text}</span>
            </Show>
            <Show when={b().more > 0}>
              <span class="critical-more"> +{b().more} more</span>
            </Show>
          </span>
          <span class="row critical-actions">
            <A href={b().href} class="critical-open">
              Open
            </A>
            <button type="button" class="small ghost" aria-expanded={!collapsed()} onClick={() => setCollapsed(!collapsed())}>
              {collapsed() ? "Expand" : "Collapse"}
            </button>
          </span>
        </div>
      )}
    </Show>
  );
}
