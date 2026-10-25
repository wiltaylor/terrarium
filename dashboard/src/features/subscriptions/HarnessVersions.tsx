// A small "Harness versions" table: the pin, the tested range, the installed
// version, and an out-of-range badge (GET /api/harnesses).

import { createResource, For, Show } from "solid-js";
import { ErrorBanner } from "../../components/ui";
import { api } from "../../stores";
import { harnessBadge, offPin, outOfRange, policyNote } from "./harnesses";

export function HarnessVersions() {
  const [report, { refetch }] = createResource((_: unknown, info: { refetching?: unknown }) => api.harnesses(info.refetching === true));
  return (
    <section class="card-box" aria-label="Harness versions" style={{ "margin-bottom": "14px" }}>
      <div class="spread">
        <h3 style={{ margin: 0 }}>
          Harness versions
          <Show when={outOfRange(report.latest).length}>
            {(n) => <span class="chip bad" style={{ "margin-left": "8px" }}>{n()} out of range</span>}
          </Show>
        </h3>
        <button class="small" onClick={() => void refetch(true)} disabled={report.loading}>
          Check again
        </button>
      </div>
      <ErrorBanner error={report.error} />
      <Show when={report.latest}>
        {(r) => (
          <>
            <div class="table-wrap">
              <table aria-label="Harness versions">
                <thead>
                  <tr>
                    <th>Harness</th>
                    <th>Pinned</th>
                    <th>Tested range</th>
                    <th>Installed</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  <For each={r().harnesses}>
                    {(h) => {
                      const b = harnessBadge(h);
                      return (
                        <tr>
                          <td>
                            <strong>{h.harness}</strong>
                            <div class="small muted mono">{h.package}</div>
                          </td>
                          <td class="mono">{h.pinned}</td>
                          <td class="mono">{h.tested}</td>
                          <td class="mono" title={offPin(h) ? "Not the pinned version" : undefined}>
                            {h.installed ?? "—"}
                          </td>
                          <td>
                            <span class={`chip ${b.cls}`} title={b.title}>
                              {b.text}
                            </span>
                          </td>
                        </tr>
                      );
                    }}
                  </For>
                </tbody>
              </table>
            </div>
            <p class="muted small" style={{ margin: "6px 0 0" }}>
              {policyNote(r())} Pins live in harnesses.toml; see docs/getting-started.md, "Updating a harness".
            </p>
          </>
        )}
      </Show>
    </section>
  );
}
