import { useInstance } from "../../instance";
// The one "System prompt" selector of an agent and a template (docs/api.md
// §28): by role, the harness default, an entry of the system prompt
// library, or a custom text. It writes the two fields `persona` and
// `system_prompt` (./systemPrompt.ts). Under Custom, "Save as new system
// prompt" adds the text to the library and selects the new entry. The
// texts are user content: they stay text nodes and textarea values.

import { createResource, createSignal, For, Show } from "solid-js";
import type { SystemPromptSummary } from "../../api/types";
import { errorText } from "../../lib/errors";

import { systemPromptProblem } from "./form";
import { afterSave, applyChoice, byRoleName, choiceOf, choiceValue, missingPersonaWarning, parseChoice, promptNameProblem, type PromptChoice, type PromptFields } from "./systemPrompt";

// The library, for every selector of the page. A save or a delete bumps the version.
const [libraryVersion, setLibraryVersion] = createSignal(0);

/** Load the library again: after a save, an edit, or a delete. */
export function refreshPromptLibrary() {
  setLibraryVersion((v) => v + 1);
}

/** The library entries, or undefined while they load (or when the daemon has no config repo). */
export function usePromptLibrary(): () => SystemPromptSummary[] | undefined {
  const { api } = useInstance();
  const [list] = createResource(libraryVersion, () => api.systemPrompts.list().catch(() => undefined));
  return () => list();
}

export const SYSTEM_PROMPT_SELECT_HELP = "The system prompt replaces the harness's default system prompt. Terrarium's protocol instructions are always added after it.";

/** The read-only text of a library entry. */
export function PromptPreview(props: { name: string }) {
  const { api } = useInstance();
  const [entry] = createResource(
    () => [props.name, libraryVersion()] as const,
    ([n]) => api.systemPrompts.get(n),
  );
  return (
    <Show when={!entry.error} fallback={<span class="small" style={{ color: "var(--danger)" }}>{errorText(entry.error)}</span>}>
      <pre class="mono small prompt-preview" aria-label={`The text of ${props.name}`}>
        {entry()?.text ?? "Loading…"}
      </pre>
    </Show>
  );
}

export function SystemPromptSelect(props: {
  value: PromptFields;
  /** The roles of the agent or the template, for "by role". */
  roles: readonly string[];
  onChange: (f: PromptFields) => void;
}) {
  const { api } = useInstance();
  const library = usePromptLibrary();
  // Custom stays picked while the text is still empty.
  const [custom, setCustom] = createSignal(!!props.value.system_prompt.trim());
  const [preview, setPreview] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [name, setName] = createSignal("");
  const [description, setDescription] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");

  const choice = () => choiceOf(props.value, custom());
  const names = () => library()?.map((e) => e.name);
  /** The library entry that the choice gives, for the preview. */
  const previewName = (): string | null => {
    const c = choice();
    if (c.kind === "library") return c.name;
    if (c.kind === "role") return byRoleName(props.roles);
    return null;
  };
  const missing = () => missingPersonaWarning(props.value.system_prompt.trim() ? "" : props.value.persona, names());
  const textProblem = () => systemPromptProblem(props.value.system_prompt);

  const pick = async (c: PromptChoice) => {
    setError("");
    setSaving(false);
    if (c.kind !== "custom") {
      setCustom(false);
      props.onChange(applyChoice(props.value, c));
      return;
    }
    // Custom starts from the text of the entry that was selected.
    const from = previewName();
    setCustom(true);
    setPreview(false);
    let seed = "";
    if (from && !props.value.system_prompt.trim() && names()?.includes(from)) {
      seed = await api.systemPrompts
        .get(from)
        .then((e) => e.text)
        .catch(() => "");
    }
    props.onChange(applyChoice(props.value, c, seed));
  };

  const save = async () => {
    const n = name().trim();
    const problem = promptNameProblem(n);
    if (problem) return setError(problem);
    setBusy(true);
    setError("");
    try {
      await api.systemPrompts.create({ name: n, text: props.value.system_prompt, ...(description().trim() ? { description: description().trim() } : {}) });
      refreshPromptLibrary();
      setCustom(false);
      setSaving(false);
      setName("");
      setDescription("");
      props.onChange(afterSave(n));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  // Enter in a field of the save row saves it, and never submits the outer form.
  const enter = (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void save();
    }
  };

  return (
    <fieldset class="stack" style={{ gap: "6px" }}>
      <legend>System prompt</legend>
      <div class="row" style={{ gap: "8px", "flex-wrap": "wrap", "align-items": "center" }}>
        <select aria-label="System prompt" value={choiceValue(choice())} onChange={(e) => void pick(parseChoice(e.currentTarget.value))} style={{ "flex": "1 1 16em" }}>
          <option value="@role" selected={choice().kind === "role"}>
            By role (leader or worker) · {byRoleName(props.roles)}
          </option>
          <option value="@none" selected={choice().kind === "none"}>
            Harness default (no system prompt)
          </option>
          <For each={library()}>
            {(e) => (
              <option value={`lib:${e.name}`} selected={choiceValue(choice()) === `lib:${e.name}`}>
                {e.description ? `${e.name} · ${e.description}` : e.name}
              </option>
            )}
          </For>
          {/* A persona that the library does not list still shows as picked. */}
          <Show when={choice().kind === "library" && !names()?.includes((choice() as { name: string }).name) ? (choice() as { name: string }).name : null}>
            {(n) => (
              <option value={`lib:${n()}`} selected>
                {n()} (not in the library)
              </option>
            )}
          </Show>
          <option value="@custom" selected={choice().kind === "custom"}>
            Custom…
          </option>
        </select>
        <Show when={previewName()}>
          <button type="button" class="small" aria-expanded={preview()} onClick={() => setPreview(!preview())}>
            {preview() ? "Hide preview" : "Preview"}
          </button>
        </Show>
      </div>
      <Show when={missing()}>
        {(m) => (
          <span class="notice small" role="alert">
            {m()}
          </span>
        )}
      </Show>
      <Show when={preview() && previewName()}>{(n) => <PromptPreview name={n()} />}</Show>
      <Show when={choice().kind === "custom"}>
        <textarea class="mono" rows={6} aria-label="Custom system prompt" value={props.value.system_prompt} onInput={(e) => props.onChange({ ...props.value, system_prompt: e.currentTarget.value })} placeholder="The system prompt text. It can use {{agent.name}}, {{project.name}}, and the other prompt variables." />
        <Show when={textProblem()}>
          {(p) => (
            <span class="small" style={{ color: "var(--danger)" }}>
              {p()}
            </span>
          )}
        </Show>
        <Show
          when={saving()}
          fallback={
            <span class="row">
              <button type="button" class="small" disabled={!props.value.system_prompt.trim() || !!textProblem()} onClick={() => setSaving(true)}>
                Save as new system prompt
              </button>
            </span>
          }
        >
          <div class="stack prompt-save" style={{ gap: "6px" }}>
            <div class="form-grid">
              <label>
                Name
                <input value={name()} onInput={(e) => setName(e.currentTarget.value)} onKeyDown={enter} placeholder="reviewer" pattern="[a-z0-9][a-z0-9_\-]{0,62}" autofocus />
              </label>
              <label>
                Description (optional)
                <input value={description()} onInput={(e) => setDescription(e.currentTarget.value)} onKeyDown={enter} placeholder="One short line for the selector" />
              </label>
            </div>
            <span class="row">
              <button type="button" class="small primary" disabled={busy()} onClick={() => void save()}>
                Save and select
              </button>
              <button
                type="button"
                class="small"
                onClick={() => {
                  setSaving(false);
                  setError("");
                }}
              >
                Cancel
              </button>
            </span>
            <span class="muted small">{"The text goes to system-prompts/<name>.md in the config repo. Commit it on the Harness config page."}</span>
          </div>
        </Show>
      </Show>
      <Show when={error()}>
        <span class="small" role="alert" style={{ color: "var(--danger)" }}>
          {error()}
        </span>
      </Show>
      <span class="muted small">{SYSTEM_PROMPT_SELECT_HELP}</span>
    </fieldset>
  );
}
