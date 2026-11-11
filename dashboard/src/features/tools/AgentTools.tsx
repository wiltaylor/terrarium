import { useInstance } from "../../instance";
// The Tools section of the agent page's Details tab (design D7): the tools
// that the agent may run, its own grants and its project's, each with its
// target and command. The links show in its sandbox at
// /opt/terrarium/agent-tools/bin. Settings > Tools grants and revokes.

import { A } from "../../instance/navigation";
import { createResource, For, Show } from "solid-js";
import type { Agent } from "../../api/types";

import { commandLine, toolsOf } from "./model";

export function AgentTools(props: { agent: Agent }) {
  const { api } = useInstance();
  const [tools] = createResource(
    () => props.agent.id,
    () => api.tools.list().catch(() => null),
  );
  const mine = () => toolsOf(tools() ?? [], props.agent);
  return (
    <section class="card-box stack" style={{ gap: "6px" }} aria-label="Tools">
      <div class="spread">
        <h3 style={{ margin: 0 }}>Tools</h3>
        <A class="small" href="/settings/tools">
          Tools in Settings
        </A>
      </div>
      <Show when={tools() !== null} fallback={<p class="muted small" style={{ margin: 0 }}>The tool list did not load.</p>}>
        <Show when={mine().length} fallback={<p class="muted small" style={{ margin: 0 }}>{tools.loading ? "Loading…" : "No tools: the agent has no grant."}</p>}>
          <ul class="small" style={{ margin: 0, "padding-left": "1.2em" }}>
            <For each={mine()}>
              {(x) => (
                <li>
                  <span class="mono">{x.tool.name}</span> <span class="chip">{x.via === "agent" ? "own grant" : `project ${props.agent.project}`}</span> on{" "}
                  <span class="mono">{x.tool.target}</span>: <span class="mono muted">{commandLine(x.tool.command)}</span>
                </li>
              )}
            </For>
          </ul>
          <p class="muted small" style={{ margin: 0 }}>
            In its sandbox each tool is a command in /opt/terrarium/agent-tools/bin, last on PATH.
          </p>
        </Show>
      </Show>
    </section>
  );
}
