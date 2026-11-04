// Settings, Software tab: the build of the running daemon
// (`GET /api/system/version`, docs/api.md §4): the version, the git ref,
// the build time, and the host agents built from another ref
// (`stale` in `GET /api/hosts`). Self-update (#19) extends this tab.

import { createResource, For, Show } from "solid-js";
import { A } from "@solidjs/router";
import { ErrorBanner, PageHead } from "../../components/ui";
import { staleHosts } from "../../lib/build-version";
import { api } from "../../stores";

export default function SoftwarePage() {
  const [version, { refetch }] = createResource(() => api.systemVersion());
  // A caller that cannot list the hosts sees no host line.
  const [hosts] = createResource(() => api.hosts.list().catch(() => null));
  const stale = () => staleHosts(hosts());

  return (
    <>
      <PageHead title="Software" level={2}>
        <button onClick={() => void refetch()}>Refresh</button>
      </PageHead>
      <ErrorBanner error={version.error} />
      <section class="card-box" aria-label="Daemon version">
        <Show when={version()} fallback={<p class="muted">{version.loading ? "Loading…" : "The daemon did not report its version."}</p>}>
          {(v) => (
            <dl class="kv">
              <dt>Version</dt>
              <dd class="mono">{v().version}</dd>
              <dt>Git ref</dt>
              <dd class="mono">
                {v().git_ref}
                <Show when={v().dirty}>
                  {" "}
                  <span class="chip warn" title="The build tree had changes to tracked files">dirty</span>
                </Show>
                <Show when={v().git_ref === "unknown"}>
                  {" "}
                  <span class="muted small">the build did not know its commit</span>
                </Show>
              </dd>
              <dt>Built</dt>
              <dd class="mono">{v().build_time}</dd>
              <dt>Host agents</dt>
              <dd>
                <Show when={hosts()} fallback={<span class="muted">—</span>}>
                  <Show when={stale().length} fallback={<span class="chip ok">all on the daemon's build</span>}>
                    <span class="chip warn" aria-label="Stale host agents">
                      {stale().length} stale
                    </span>{" "}
                    <For each={stale()}>{(n, i) => <><span class="mono">{n}</span>{i() < stale().length - 1 ? ", " : ""}</>}</For>
                    {" · "}
                    <A href="/settings/hosts">Hosts</A>
                  </Show>
                </Show>
              </dd>
            </dl>
          )}
        </Show>
      </section>
    </>
  );
}
