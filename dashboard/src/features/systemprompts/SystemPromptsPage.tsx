// The System prompts tab of the settings screen (docs/api.md §28): the
// library of the config repo (`system-prompts/<name>.md`), with view, edit,
// delete, and "New system prompt". A delete of an entry in use answers 409
// with the users; the page lists them and offers "Delete anyway". The texts
// are user content: they stay text nodes and textarea values. The logic is
// in ./model.ts.

import { A } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { SystemPromptDetail, SystemPromptSummary, SystemPromptUser } from "../../api/types";
import { ago, bytes } from "../../lib/format";
import { api } from "../../stores";
import { systemPromptProblem } from "../agents/form";
import { refreshPromptLibrary } from "../agents/SystemPromptSelect";
import { promptNameProblem } from "../agents/systemPrompt";
import { COMMIT_NOTE, filterPrompts, sourceText, userLine, usersOfConflict } from "./model";

function CommitNote() {
  return (
    <p class="muted small" style={{ margin: 0 }}>
      {COMMIT_NOTE} <A href="/settings/harness">Open Harness config</A>.
    </p>
  );
}

/** The text and description fields of a new entry or an edit. */
function PromptFields(props: { description: string; text: string; onDescription: (s: string) => void; onText: (s: string) => void }) {
  return (
    <>
      <label>
        Description (optional)
        <input value={props.description} onInput={(e) => props.onDescription(e.currentTarget.value)} placeholder="One short line for the selector" />
      </label>
      <label>
        Text
        <textarea class="mono" rows={12} value={props.text} onInput={(e) => props.onText(e.currentTarget.value)} placeholder="The system prompt. It can use {{agent.name}}, {{project.name}}, and the other prompt variables." />
      </label>
      <Show when={systemPromptProblem(props.text)}>
        {(p) => (
          <span class="small" style={{ color: "var(--danger)" }}>
            {p()}
          </span>
        )}
      </Show>
    </>
  );
}

function NewPromptForm(props: { onCreated: (d: SystemPromptDetail) => void; onCancel: () => void }) {
  const [name, setName] = createSignal("");
  const [description, setDescription] = createSignal("");
  const [text, setText] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const problem = () => (name() ? promptNameProblem(name()) : null);
  const ready = () => !!name().trim() && !problem() && !!text().trim() && !systemPromptProblem(text());

  const submit = async (e: Event) => {
    e.preventDefault();
    if (!ready()) return;
    setBusy(true);
    setError(null);
    try {
      const d = await api.systemPrompts.create({ name: name().trim(), text: text(), ...(description().trim() ? { description: description().trim() } : {}) });
      refreshPromptLibrary();
      props.onCreated(d);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form class="card-box stack" onSubmit={submit} aria-label="New system prompt">
      <h3 style={{ margin: 0 }}>New system prompt</h3>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <label>
        Name
        <input value={name()} onInput={(e) => setName(e.currentTarget.value)} required pattern="[a-z0-9][a-z0-9_\-]{0,62}" placeholder="reviewer" autofocus />
        <Show when={problem()} fallback={<span class="muted small">An agent selects it as its system prompt (persona = the name).</span>}>
          {(p) => (
            <span class="small" style={{ color: "var(--danger)" }}>
              {p()}
            </span>
          )}
        </Show>
      </label>
      <PromptFields description={description()} text={text()} onDescription={setDescription} onText={setText} />
      <div class="row">
        <button class="primary" type="submit" disabled={busy() || !ready()}>
          Save
        </button>
        <button type="button" onClick={() => props.onCancel()}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** One entry: its text, its users, edit, and delete. */
function PromptDetail(props: { name: string; onChanged: () => void; onDeleted: () => void }) {
  const [version, setVersion] = createSignal(0);
  const [entry] = createResource(
    () => [props.name, version()] as const,
    ([n]) => api.systemPrompts.get(n),
  );
  const [editing, setEditing] = createSignal(false);
  const [description, setDescription] = createSignal("");
  const [text, setText] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const [confirm, setConfirm] = createSignal(false);
  const [inUse, setInUse] = createSignal<SystemPromptUser[] | null>(null);

  const startEdit = (d: SystemPromptDetail) => {
    setDescription(d.description);
    setText(d.text);
    setError(null);
    setEditing(true);
  };
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.systemPrompts.update(props.name, { text: text(), description: description().trim() });
      refreshPromptLibrary();
      setEditing(false);
      setVersion((v) => v + 1);
      props.onChanged();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };
  const remove = async (force: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.systemPrompts.remove(props.name, { force });
      refreshPromptLibrary();
      props.onDeleted();
    } catch (e) {
      const users = usersOfConflict(e);
      if (users) setInUse(users);
      else setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section class="card-box stack" aria-label={`System prompt ${props.name}`}>
      <ErrorBanner error={error() ?? entry.error} onDismiss={() => setError(null)} />
      <Show when={entry()}>
        {(d) => (
          <>
            <div class="row" style={{ "justify-content": "space-between", "flex-wrap": "wrap" }}>
              <h3 style={{ margin: 0 }}>
                <span class="mono">{d().name}</span>
                <Show when={d().description}>
                  <span class="muted small"> · {d().description}</span>
                </Show>
              </h3>
              <span class="row">
                <Show when={!editing()}>
                  <button class="small" onClick={() => startEdit(d())}>
                    Edit
                  </button>
                  <Show when={!d().builtin}>
                    <Show
                      when={confirm()}
                      fallback={
                        <button class="small danger" onClick={() => setConfirm(true)}>
                          Delete
                        </button>
                      }
                    >
                      <button class="small danger" disabled={busy()} onClick={() => void remove(false)}>
                        Confirm delete
                      </button>
                      <button
                        class="small"
                        onClick={() => {
                          setConfirm(false);
                          setInUse(null);
                        }}
                      >
                        Cancel
                      </button>
                    </Show>
                  </Show>
                </Show>
              </span>
            </div>
            <p class="muted small" style={{ margin: 0 }}>
              <span class="mono">{d().path}</span> · {sourceText(d())} · {bytes(d().size)}
              <Show when={d().updated}> · changed {ago(d().updated)}</Show>
            </p>
            <Show when={inUse()}>
              {(users) => (
                <div class="error-banner" role="alert" style={{ gap: "4px", "flex-direction": "column", "align-items": "flex-start" }}>
                  <strong>In use by {users().length}. Pick another system prompt for them first, or delete it anyway: their next start then fails.</strong>
                  <ul class="small" style={{ margin: 0 }}>
                    <For each={users()}>{(u) => <li>{userLine(u)}</li>}</For>
                  </ul>
                  <span class="row">
                    <button class="small danger" disabled={busy()} onClick={() => void remove(true)}>
                      Delete anyway
                    </button>
                  </span>
                </div>
              )}
            </Show>
            <Show
              when={editing()}
              fallback={
                <>
                  <pre class="mono small prompt-preview">{d().text}</pre>
                  <div class="small">
                    <strong>Used by: </strong>
                    <Show when={d().users.length} fallback={<span class="muted">no agent or template</span>}>
                      {d().users.map(userLine).join(", ")}
                    </Show>
                  </div>
                </>
              }
            >
              <PromptFields description={description()} text={text()} onDescription={setDescription} onText={setText} />
              <div class="row">
                <button class="primary small" disabled={busy() || !text().trim() || !!systemPromptProblem(text())} onClick={() => void save()}>
                  Save
                </button>
                <button class="small" onClick={() => setEditing(false)}>
                  Cancel
                </button>
              </div>
            </Show>
          </>
        )}
      </Show>
    </section>
  );
}

export default function SystemPromptsPage() {
  const [version, setVersion] = createSignal(0);
  const [list] = createResource(version, () => api.systemPrompts.list());
  const [q, setQ] = createSignal("");
  const [creating, setCreating] = createSignal(false);
  const [open, setOpen] = createSignal<string | null>(null);
  const shown = (): SystemPromptSummary[] => filterPrompts(list() ?? [], q());
  const reload = () => setVersion((v) => v + 1);

  return (
    <div class="stack">
      <PageHead title="System prompts" level={2}>
        <button class="primary" onClick={() => setCreating(true)} disabled={creating()}>
          New system prompt
        </button>
      </PageHead>
      <p class="muted small" style={{ margin: 0 }}>
        The system prompt library. An agent or a template selects an entry in its System prompt field. leader and worker are the prompts by role.
      </p>
      <CommitNote />
      <ErrorBanner error={list.error} />
      <Show when={creating()}>
        <NewPromptForm
          onCreated={(d) => {
            setCreating(false);
            reload();
            setOpen(d.name);
          }}
          onCancel={() => setCreating(false)}
        />
      </Show>
      <input type="search" placeholder="Filter by name or description" value={q()} onInput={(e) => setQ(e.currentTarget.value)} aria-label="Filter system prompts" style={{ "max-width": "24em" }} />
      <Show when={shown().length} fallback={<Empty>{list.loading ? "Loading…" : q() ? "No system prompts match." : "There are no system prompts."}</Empty>}>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Description</th>
                <th>Source</th>
                <th>Size</th>
                <th>Changed</th>
              </tr>
            </thead>
            <tbody>
              <For each={shown()}>
                {(e) => (
                  <tr classList={{ selected: open() === e.name }}>
                    <td>
                      <button class="link mono" aria-expanded={open() === e.name} onClick={() => setOpen(open() === e.name ? null : e.name)}>
                        {e.name}
                      </button>
                    </td>
                    <td>{e.description || <span class="muted">—</span>}</td>
                    <td class="small">{sourceText(e)}</td>
                    <td class="small">{bytes(e.size)}</td>
                    <td class="small muted">{e.updated ? ago(e.updated) : "—"}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <Show when={open()} keyed>
        {(n) => (
          <PromptDetail
            name={n}
            onChanged={reload}
            onDeleted={() => {
              setOpen(null);
              reload();
            }}
          />
        )}
      </Show>
    </div>
  );
}
