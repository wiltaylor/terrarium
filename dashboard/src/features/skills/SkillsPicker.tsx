// The skills of an agent or a template: a checkbox for each skill of
// `GET /api/skills`, and one for `*` (all skills). A daemon with no config
// repo (`501`) has no skills: the picker shows a short note. A description
// is plain text.

import { createResource, For, Show } from "solid-js";
import { api } from "../../stores";
import type { SkillSummary } from "../../api/types";
import { ALL_SKILLS, NO_SKILLS_NOTE, noSkills, skillOptions, toggleSkill, unknownSkills } from "./model";

export function SkillsPicker(props: { value: string[]; onChange: (v: string[]) => void; disabled?: boolean; legend?: string }) {
  const [list] = createResource(() => api.skills.list());
  const byName = () => new Map((list() ?? []).map((s) => [s.name, s] as [string, SkillSummary]));
  const all = () => props.value.includes(ALL_SKILLS);
  const missing = () => (list() ? unknownSkills(props.value, list()!) : []);

  return (
    <fieldset class="stack" style={{ gap: "4px" }}>
      <legend>{props.legend ?? "Skills"}</legend>
      <Show
        when={!list.error}
        fallback={
          <span class="muted small" role="status">
            {noSkills(list.error) ? NO_SKILLS_NOTE : "The skill list did not load. The skills of this form do not change."}
          </span>
        }
      >
        <Show when={!list.loading} fallback={<span class="muted small">Loading…</span>}>
          <label class="row small">
            <input type="checkbox" checked={all()} disabled={props.disabled} onChange={(e) => props.onChange(toggleSkill(props.value, ALL_SKILLS, e.currentTarget.checked))} /> All skills (*)
          </label>
          <Show when={skillOptions(list() ?? [], props.value).length} fallback={<span class="muted small">There are no skills. Make one on the Skills tab of Settings.</span>}>
            <div class="stack" style={{ gap: "2px" }}>
              <For each={skillOptions(list() ?? [], props.value)}>
                {(name) => (
                  <label class="row small" style={{ "align-items": "baseline" }}>
                    <input
                      type="checkbox"
                      checked={all() || props.value.includes(name)}
                      disabled={props.disabled || all()}
                      onChange={(e) => props.onChange(toggleSkill(props.value, name, e.currentTarget.checked))}
                    />{" "}
                    <span class="mono">{name}</span>
                    <Show when={byName().get(name)?.errors?.length}>
                      <span class="chip warn" title="The skill fails the checks. Agents do not get it.">
                        problems
                      </span>
                    </Show>
                    <Show when={byName().get(name)?.description}>{(d) => <span class="muted ellipsis">{d()}</span>}</Show>
                  </label>
                )}
              </For>
            </div>
          </Show>
          <Show when={missing().length}>
            <span class="small" style={{ color: "var(--danger)" }}>
              No skill {missing().join(", ")}. Remove it, or the daemon refuses the save.
            </span>
          </Show>
          <span class="muted small">All skills gives the agent every skill, also the skills made later. An agent gets its skills at the start.</span>
        </Show>
      </Show>
    </fieldset>
  );
}
