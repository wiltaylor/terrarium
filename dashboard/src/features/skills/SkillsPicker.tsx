// The skills of an agent or a template: a "Use default skills" toggle, a
// checkbox for each skill of `GET /api/skills`, and one for `*` (all
// skills). A default-on skill shows checked with a "default" badge; a clear
// of its box puts it in `skills_off`. A daemon with no config repo (`501`)
// has no skills: the picker shows a short note. A description is plain
// text.

import { createResource, For, Show } from "solid-js";
import { api } from "../../stores";
import type { SkillSummary } from "../../api/types";
import { ALL_SKILLS, byDefault, DEFAULT_NOTE, defaultOnNames, effectiveLabel, effectiveOf, NO_SKILLS_NOTE, noSkills, pickSkill, skillOn, skillOptions, toggleSkill, unknownSkills, type SkillSelection } from "./model";

export function SkillsPicker(props: { value: SkillSelection; onChange: (v: SkillSelection) => void; disabled?: boolean; legend?: string }) {
  const [list] = createResource(() => api.skills.list());
  const byName = () => new Map((list() ?? []).map((s) => [s.name, s] as [string, SkillSummary]));
  const defaultOn = () => defaultOnNames(list() ?? []);
  const all = () => props.value.skills.includes(ALL_SKILLS);
  const missing = () => (list() ? unknownSkills(props.value.skills, list()!) : []);
  const options = () => skillOptions(list() ?? [], [...props.value.skills, ...props.value.skills_off]);

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
            <input type="checkbox" checked={props.value.default_skills} disabled={props.disabled} onChange={(e) => props.onChange({ ...props.value, default_skills: e.currentTarget.checked })} /> Use default skills
          </label>
          <label class="row small">
            <input type="checkbox" checked={all()} disabled={props.disabled} onChange={(e) => props.onChange({ ...props.value, skills: toggleSkill(props.value.skills, ALL_SKILLS, e.currentTarget.checked) })} /> All skills (*)
          </label>
          <Show when={options().length} fallback={<span class="muted small">There are no skills. Make one on the Skills tab of Settings.</span>}>
            <div class="stack" style={{ gap: "2px" }}>
              <For each={options()}>
                {(name) => (
                  <label class="row small" style={{ "align-items": "baseline" }}>
                    <input
                      type="checkbox"
                      data-skill={name}
                      checked={skillOn(props.value, name, defaultOn())}
                      disabled={props.disabled}
                      onChange={(e) => props.onChange(pickSkill(props.value, name, e.currentTarget.checked, defaultOn()))}
                    />{" "}
                    <span class="mono">{name}</span>
                    <Show when={defaultOn().has(name)}>
                      <span class={byDefault(props.value, name, defaultOn()) || !props.value.default_skills ? "chip" : "chip ok"} title="A default skill: each agent gets it unless it turns it off.">
                        default
                      </span>
                    </Show>
                    <Show when={props.value.skills_off.includes(name)}>
                      <span class="chip warn" title="In skills_off: the agent does not get this skill.">
                        off
                      </span>
                    </Show>
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
          <span class="small" data-testid="effective-skills">
            The agent gets: <span class="mono">{effectiveLabel(effectiveOf(props.value, list() ?? []))}</span>
          </span>
          <span class="muted small">{DEFAULT_NOTE} All skills gives the agent every skill, also the skills made later. An agent gets its skills at the start.</span>
        </Show>
      </Show>
    </fieldset>
  );
}
