// "New agent" (docs/api.md §6.1, §21.3). The dialog opens on a chooser: a
// card for each template of the project (its own, then the global ones)
// and a Custom card. A template card opens a short form: the name and a
// summary of the template. "Change settings…" shows the agent fields,
// filled from the template. The create sends `template` and only the
// fields that the user changed. Custom opens the full form with no
// template. "Back" goes to the chooser and keeps the name. With no
// template, the dialog opens on the full form.

import { A } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, on, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Agent, AgentTemplate } from "../../api/types";
import { agents, api } from "../../stores";
import { emptyAgentForm, validateAgentForm, type AgentFormValues } from "./form";
import { AgentFields } from "./AgentFields";
import { AgentFormFields } from "./AgentForm";
import { chooserList, showSearch, startStep, templateSummary, type NewAgentStep } from "./chooser";
import { formToCreate, resolvedToForm, scopeLabel, useKey, type TemplatePick } from "../templates/model";

/** Move the focus among the buttons of a group with the arrow keys, Home, and End. */
function arrowKeys(e: KeyboardEvent) {
  const list = [...(e.currentTarget as HTMLElement).querySelectorAll<HTMLButtonElement>("button")];
  const i = list.indexOf(document.activeElement as HTMLButtonElement);
  if (i < 0) return;
  const to = { ArrowRight: i + 1, ArrowDown: i + 1, ArrowLeft: i - 1, ArrowUp: i - 1, Home: 0, End: list.length - 1 }[e.key];
  if (to === undefined) return;
  e.preventDefault();
  list[Math.max(0, Math.min(list.length - 1, to))]?.focus();
}

export function NewAgentDialog(props: { project: string; onClose: () => void; onSaved: (a: Agent) => void }) {
  const [step, setStep] = createSignal<NewAgentStep | null>(null);
  // The name stays across the steps.
  const [name, setName] = createSignal("");
  const [custom, setCustom] = createSignal<AgentFormValues>(emptyAgentForm(props.project));
  const [pick, setPick] = createSignal<(TemplatePick & { template: AgentTemplate }) | null>(null);
  const [fromTemplate, setFromTemplate] = createSignal<AgentFormValues>(emptyAgentForm(props.project));
  const [expanded, setExpanded] = createSignal(false);
  const [query, setQuery] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);

  const [templates] = createResource(
    () => props.project,
    (p) =>
      api.templates
        .list({ project: p })
        .then((list) => chooserList(list, p))
        .catch(() => [] as AgentTemplate[]),
  );
  createEffect(on(templates, (list) => list && step() === null && setStep(startStep(list))));
  const shown = () => chooserList(templates() ?? [], props.project, query());

  // The values of a step, with the shared name.
  const values = () => ({ ...(step() === "template" ? fromTemplate() : custom()), name: name() });
  const change = (v: AgentFormValues) => {
    setName(v.name);
    (step() === "template" ? setFromTemplate : setCustom)(v);
  };
  const problems = () => validateAgentForm(values());
  const body = () => formToCreate(values(), step() === "template" ? pick() : null);
  /** The number of fields that the create changes from the template. */
  const overrides = () => Object.keys(body()).filter((k) => !["name", "project", "template"].includes(k)).length;

  // The focus: the search box or the first card on the chooser, the name on a form.
  let chooser: HTMLDivElement | undefined;
  let search: HTMLInputElement | undefined;
  let nameInput: HTMLInputElement | undefined;
  createEffect(
    on(step, (s) =>
      queueMicrotask(() => {
        if (s === "choose") (search ?? chooser?.querySelector("button"))?.focus();
        else if (s) nameInput?.focus();
      }),
    ),
  );

  const choose = async (t: AgentTemplate) => {
    const key = useKey(t);
    try {
      const base = resolvedToForm(await api.templates.resolve(key, props.project), "");
      setPick({ key, base, template: t });
      setFromTemplate(base);
      setExpanded(false);
      setError(null);
      setStep("template");
    } catch (err) {
      setError(err);
    }
  };
  const back = () => {
    setError(null);
    setStep("choose");
  };

  const submit = async (e: Event) => {
    e.preventDefault();
    if (problems().length) return setError(problems().join(" "));
    setBusy(true);
    try {
      const saved = await api.agents.create(body());
      agents.upsert(saved);
      props.onSaved(saved);
      props.onClose();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  const buttons = (label: string) => (
    <div class="row">
      <button class="primary" type="submit" disabled={busy() || problems().length > 0}>
        {label}
      </button>
      <Show when={templates()?.length}>
        <button type="button" onClick={back}>
          Back
        </button>
      </Show>
      <button type="button" onClick={() => props.onClose()}>
        Cancel
      </button>
    </div>
  );

  return (
    <Modal title={step() === "template" ? `New agent from ${pick()?.template.name}` : "New agent"} onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={step() === null}>
        <p class="muted">Loading templates…</p>
      </Show>

      <Show when={step() === "choose"}>
        <p class="muted small" style={{ margin: 0 }}>
          Start from a template of {props.project} or a global one, or set every field yourself.
        </p>
        <Show when={showSearch(templates() ?? [])}>
          <input ref={search} type="search" value={query()} onInput={(e) => setQuery(e.currentTarget.value)} placeholder="Search templates" aria-label="Search templates" />
        </Show>
        <div ref={chooser} class="tpl-choices" role="group" aria-label="Templates" onKeyDown={arrowKeys}>
          <For each={shown()}>
            {(t) => {
              const s = templateSummary(t);
              return (
                <button type="button" class="tpl-choice" onClick={() => void choose(t)}>
                  <span class="spread">
                    <strong class="ellipsis">{t.name}</strong>
                    <Show when={s.global}>
                      <span class="chip">global</span>
                    </Show>
                  </span>
                  <span class="row small">
                    <span class="chip">{s.harness}</span>
                    <span class="muted ellipsis">{s.model}</span>
                  </span>
                  <Show when={s.roles.length}>
                    <span class="row">
                      <For each={s.roles}>{(r) => <span class={`chip ${r === "sysadmin" ? "bad" : "info"}`}>{r}</span>}</For>
                    </span>
                  </Show>
                  <Show when={s.line}>
                    <span class="muted small tpl-line">{s.line}</span>
                  </Show>
                </button>
              );
            }}
          </For>
          <button type="button" class="tpl-choice tpl-custom" onClick={() => setStep("custom")}>
            <strong>Custom</strong>
            <span class="muted small">Start from an empty form and set every field.</span>
          </button>
        </div>
        <Show when={!shown().length}>
          <p class="muted small" style={{ margin: 0 }}>
            No template matches "{query()}".
          </p>
        </Show>
      </Show>

      <Show when={step() === "template" && pick()}>
        {(p) => {
          const s = () => templateSummary(p().template);
          const base = () => p().base;
          return (
            <form class="stack" onSubmit={submit}>
              <label>
                Name
                <input ref={nameInput} value={name()} onInput={(e) => setName(e.currentTarget.value)} required pattern="[a-z0-9][a-z0-9_\-]{0,62}" placeholder="worker" />
              </label>
              <dl class="kv card-box">
                <dt>Template</dt>
                <dd>
                  {p().template.name} <span class="chip">{scopeLabel(p().template)}</span>
                </dd>
                <dt>Project</dt>
                <dd>{props.project}</dd>
                <dt>Harness</dt>
                <dd>{s().harness}</dd>
                <dt>Model</dt>
                <dd>{s().model}</dd>
                <dt>Roles</dt>
                <dd>{s().roles.join(", ") || "member"}</dd>
                <Show when={base().prompt}>
                  <dt>Prompt</dt>
                  <dd>{base().prompt}</dd>
                </Show>
                <Show when={base().persona}>
                  <dt>Persona</dt>
                  <dd>{base().persona}</dd>
                </Show>
                <dt>Targets</dt>
                <dd>{base().targets.join(", ") || "none"}</dd>
                <dt>Mail to</dt>
                <dd>{base().mail_to}</dd>
                <Show when={base().skills.length}>
                  <dt>Skills</dt>
                  <dd>{base().skills.join(", ")}</dd>
                </Show>
                <Show when={base().skills_off.length}>
                  <dt>Skills off</dt>
                  <dd>{base().skills_off.join(", ")}</dd>
                </Show>
                <Show when={!base().default_skills}>
                  <dt>Default skills</dt>
                  <dd>off</dd>
                </Show>
                <dt>Restart</dt>
                <dd>{base().restart}</dd>
                <Show when={s().line}>
                  <dt>About</dt>
                  <dd>{s().line}</dd>
                </Show>
              </dl>
              <div class="row">
                <button type="button" aria-expanded={expanded()} onClick={() => setExpanded(!expanded())}>
                  {expanded() ? "Hide settings" : "Change settings…"}
                </button>
                <span class="muted small" role="status">
                  {overrides() ? `${overrides()} field${overrides() === 1 ? "" : "s"} changed from the template.` : "The agent gets the template's settings."}
                </span>
              </div>
              <Show when={expanded()}>
                <AgentFields value={values()} onChange={change} roles />
              </Show>
              <Show when={problems().length && name()}>
                <ul class="small" style={{ color: "var(--danger)", margin: 0 }}>
                  <For each={problems()}>{(x) => <li>{x}</li>}</For>
                </ul>
              </Show>
              {buttons("Create agent")}
            </form>
          );
        }}
      </Show>

      <Show when={step() === "custom"}>
        <Show when={!templates()?.length}>
          <p class="muted small" style={{ margin: 0 }}>
            This project has no templates. Make one in{" "}
            <A href={`/p/${encodeURIComponent(props.project)}/settings/templates`} onClick={() => props.onClose()}>
              Settings → Templates
            </A>{" "}
            to make agents with the same settings.
          </p>
        </Show>
        <form class="stack" onSubmit={submit}>
          <AgentFormFields value={values()} onChange={change} editing={false} nameRef={(el) => (nameInput = el)} />
          {buttons("Create agent")}
        </form>
      </Show>
    </Modal>
  );
}
