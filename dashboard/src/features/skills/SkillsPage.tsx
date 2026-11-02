// Terrarium-managed skills, kept in the config repo: the list (name,
// description, files, "used by", problems), a new skill (name and
// description), and the editor of one skill at /settings/skills/<name>: SKILL.md in a
// textarea, a Markdown preview through the sanitizing Markdown component,
// the other files, save with the problems of a 422, "used by", and delete
// (an in-page confirm). A write stays uncommitted: the Harness config page commits
// it. A daemon with no config repo answers `501`; the page then shows a
// short note. The logic is in ./model.ts. A description is plain text.

import { A, useNavigate, useParams } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, on, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead, Tabs } from "../../components/ui";
import { Markdown } from "../../components/Markdown";
import type { SkillDetail, SkillSummary } from "../../api/types";
import { bytes } from "../../lib/format";
import { renderMarkdown } from "../../lib/markdown";
import { api } from "../../stores";
import { COMMIT_NOTE, DEFAULT_ON_TITLE, descriptionProblem, filterSkills, nameProblem, NO_SKILLS_NOTE, noSkills, previewBody, RESTART_NOTE, skillProblems, usedBy, usedByText } from "./model";

/** The note with a link to the Harness config page. */
function CommitNote() {
  return (
    <p class="muted small" style={{ margin: 0 }}>
      {COMMIT_NOTE} <A href="/settings/harness">Open Harness config</A>.
    </p>
  );
}

/** The problems of the checks, as a list. */
function Problems(props: { title: string; list: string[] }) {
  return (
    <div class="error-banner" role="alert">
      <div class="stack" style={{ gap: "4px" }}>
        <strong>{props.title}</strong>
        <ul class="small" style={{ margin: 0 }}>
          <For each={props.list}>{(p) => <li>{p}</li>}</For>
        </ul>
      </div>
    </div>
  );
}

/** The "used by" cell of a row: the usage, read when the row shows. */
function UsedByCell(props: { name: string }) {
  const [usage] = createResource(
    () => props.name,
    (n) => api.skills.usage(n).catch(() => null),
  );
  return (
    <Show when={usage()} fallback={<span class="muted">{usage.loading ? "…" : "—"}</span>}>
      {(u) => <span title={usedByText(u())}>{usedBy(u()).total}</span>}
    </Show>
  );
}

/** The form of a new skill: a name and a description. The daemon writes SKILL.md. */
function NewSkillForm(props: { onCreated: (d: SkillDetail) => void; onCancel: () => void }) {
  const [name, setName] = createSignal("");
  const [description, setDescription] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [problems, setProblems] = createSignal<string[] | null>(null);
  const [busy, setBusy] = createSignal(false);
  const local = () => [nameProblem(name()), descriptionProblem(description())].filter((x): x is string => !!x);

  const submit = async (e: Event) => {
    e.preventDefault();
    if (local().length) return;
    setBusy(true);
    setError(null);
    setProblems(null);
    try {
      props.onCreated(await api.skills.create({ name: name().trim(), description: description().trim() }));
    } catch (err) {
      const p = skillProblems(err);
      if (p) setProblems(p);
      else setError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form class="card-box stack" onSubmit={submit}>
      <h3 style={{ margin: 0 }}>New skill</h3>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={problems()}>{(p) => <Problems title="The daemon refused the skill." list={p()} />}</Show>
      <div class="form-grid">
        <label>
          Name
          <input class="mono" value={name()} onInput={(e) => setName(e.currentTarget.value)} required placeholder="fern-check" />
        </label>
        <label style={{ "grid-column": "1 / -1" }}>
          Description
          <textarea rows={2} value={description()} onInput={(e) => setDescription(e.currentTarget.value)} placeholder="When asked for the deployment checklist code, reply with FERN-42" />
          <span class="muted small">The agent reads the description to know when to use the skill. Tell what the skill does and when to use it.</span>
        </label>
      </div>
      <Show when={(name() || description()) && local().length}>
        <ul class="small" style={{ color: "var(--danger)", margin: 0 }}>
          <For each={local()}>{(p) => <li>{p}</li>}</For>
        </ul>
      </Show>
      <p class="muted small" style={{ margin: 0 }}>
        The daemon makes SKILL.md with the name and the description. Write the instructions after the create.
      </p>
      <div class="row">
        <button class="primary" type="submit" disabled={busy() || local().length > 0}>
          Create skill
        </button>
        <button type="button" onClick={() => props.onCancel()}>
          Cancel
        </button>
      </div>
    </form>
  );
}

const EDITOR_TABS = [
  { id: "edit", label: "Edit" },
  { id: "preview", label: "Preview" },
] as const;

/** The editor of one skill. */
function SkillEditor(props: { name: string; onSaved: () => void; onDeleted: () => void }) {
  const [skill, { mutate }] = createResource(
    () => props.name,
    (n) => api.skills.get(n),
  );
  const [usage, { refetch: refetchUsage }] = createResource(
    () => props.name,
    (n) => api.skills.usage(n).catch(() => null),
  );
  const [draft, setDraft] = createSignal("");
  const [tab, setTab] = createSignal<(typeof EDITOR_TABS)[number]["id"]>("edit");
  const [error, setError] = createSignal<unknown>(null);
  const [problems, setProblems] = createSignal<string[] | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [openFile, setOpenFile] = createSignal<string | null>(null);
  // A new skill or a reload: the draft takes the text of the daemon.
  createEffect(on(skill, (s) => s && setDraft(s.skill_md)));
  const dirty = () => !!skill() && draft() !== skill()!.skill_md;

  const save = async () => {
    setBusy(true);
    setError(null);
    setProblems(null);
    setNotice(null);
    try {
      const d = await api.skills.update(props.name, { skill_md: draft() });
      mutate(d);
      setNotice("Saved.");
      props.onSaved();
    } catch (e) {
      const p = skillProblems(e);
      if (p) setProblems(p);
      else setError(e);
    } finally {
      setBusy(false);
    }
  };
  const remove = async () => {
    setError(null);
    try {
      await api.skills.remove(props.name);
      props.onDeleted();
    } catch (e) {
      setError(e);
    }
  };

  return (
    <section class="card-box stack">
      <div class="spread">
        <h2 style={{ margin: 0 }} class="mono">
          {props.name}
        </h2>
        <span class="row">
          <A class="small" href="/settings/skills">
            Close
          </A>
        </span>
      </div>
      <Show when={!skill.error} fallback={<ErrorBanner error={skill.error} />}>
        <Show when={skill()} fallback={<span class="muted small">Loading…</span>}>
          {(s) => (
            <>
              <ErrorBanner error={error()} onDismiss={() => setError(null)} />
              <Show when={problems()}>{(p) => <Problems title="The daemon refused the save. Correct these problems:" list={p()} />}</Show>
              <Show when={!problems() && s().errors?.length}>
                <Problems title="This skill fails the checks. Agents do not get it." list={s().errors!} />
              </Show>
              <Show when={notice()}>
                <p class="notice small" role="status" style={{ margin: 0 }}>
                  {notice()} {RESTART_NOTE}
                </p>
              </Show>

              <dl class="kv">
                <dt>Description</dt>
                <dd>{s().description || <span class="muted">none</span>}</dd>
                <dt>Default</dt>
                <dd>
                  <Show when={s().default_on} fallback={<span class="muted">No. An agent gets it only when its skills list names it.</span>}>
                    <span class="chip" title={DEFAULT_ON_TITLE}>
                      default
                    </span>{" "}
                    <span class="muted small">Each agent gets it, unless the agent turns it off.</span>
                  </Show>
                </dd>
                <dt>Size</dt>
                <dd>
                  {s().files} file{s().files === 1 ? "" : "s"}, {bytes(s().bytes)}
                </dd>
                <dt>Used by</dt>
                <dd>
                  <Show when={usage()} fallback={<span class="muted">{usage.loading ? "Loading…" : "not known"}</span>}>
                    {(u) => (
                      <Show when={usedBy(u()).total} fallback={<span class="muted">No agent or template uses this skill.</span>}>
                        <ul class="small" style={{ margin: 0, "padding-left": "1.2em" }}>
                          <For each={u().agents}>
                            {(x) => (
                              <li>
                                agent <A href={`/agents/${encodeURIComponent(x.id)}`}>{x.name}</A>
                                <Show when={x.all}>
                                  <span class="muted"> (all skills)</span>
                                </Show>
                                <Show when={x.default}>
                                  <span class="muted"> (default)</span>
                                </Show>
                              </li>
                            )}
                          </For>
                          <For each={u().templates}>
                            {(x) => (
                              <li>
                                template <A href="/settings/templates">{x.name}</A>
                                <Show when={x.all}>
                                  <span class="muted"> (all skills)</span>
                                </Show>
                                <Show when={x.default}>
                                  <span class="muted"> (default)</span>
                                </Show>
                              </li>
                            )}
                          </For>
                        </ul>
                      </Show>
                    )}
                  </Show>
                </dd>
              </dl>

              <div>
                <Tabs tabs={EDITOR_TABS} active={tab()} onChange={setTab} />
                <Show
                  when={tab() === "edit"}
                  fallback={
                    <div class="card-box">
                      <Markdown html={renderMarkdown(previewBody(draft()))} />
                    </div>
                  }
                >
                  <label>
                    SKILL.md
                    <textarea class="mono" rows={18} value={draft()} onInput={(e) => setDraft(e.currentTarget.value)} spellcheck={false} aria-label="SKILL.md" />
                    <span class="muted small">Keep the front matter: name (the same as the folder name) and description (at most 1024 characters).</span>
                  </label>
                </Show>
              </div>
              <div class="row" style={{ "flex-wrap": "wrap" }}>
                <button class="primary" disabled={busy() || !dirty()} onClick={() => void save()}>
                  Save
                </button>
                <button disabled={busy() || !dirty()} onClick={() => setDraft(s().skill_md)}>
                  Discard changes
                </button>
                <button
                  onClick={() => {
                    void refetchUsage();
                  }}
                >
                  Refresh used by
                </button>
                <ConfirmButton
                  label="Delete"
                  confirmLabel={usage() && usedBy(usage()!).total ? `Yes, delete (${usedByText(usage()!)} use it)` : "Yes, delete"}
                  disabled={busy()}
                  onConfirm={() => remove()}
                />
              </div>
              <CommitNote />

              <h3 style={{ margin: 0 }}>Other files</h3>
              <Show when={s().file_list.length} fallback={<span class="muted small">SKILL.md is the only file.</span>}>
                <ul class="stack" style={{ gap: "4px", margin: 0, "padding-left": 0, "list-style": "none" }}>
                  <For each={s().file_list}>
                    {(f) => (
                      <li class="stack" style={{ gap: "4px" }}>
                        <div class="row">
                          <Show when={f.text !== undefined} fallback={<span class="mono small">{f.path}</span>}>
                            <button class="ghost small" onClick={() => setOpenFile(openFile() === f.path ? null : f.path)} aria-expanded={openFile() === f.path}>
                              {openFile() === f.path ? "▾" : "▸"} <span class="mono">{f.path}</span>
                            </button>
                          </Show>
                          <span class="muted small">{bytes(f.size)}</span>
                          <Show when={f.text === undefined}>
                            <span class="chip">binary</span>
                          </Show>
                        </div>
                        <Show when={openFile() === f.path && f.text !== undefined}>
                          <pre class="mono small" style={{ margin: 0, "white-space": "pre-wrap", "max-height": "20em", overflow: "auto" }}>
                            {f.text}
                          </pre>
                        </Show>
                      </li>
                    )}
                  </For>
                </ul>
              </Show>
              <span class="muted small">
                Change the other files with <code>terrarium skill</code> or in the config repo.
              </span>
            </>
          )}
        </Show>
      </Show>
    </section>
  );
}

export default function SkillsPage() {
  const params = useParams<{ name?: string }>();
  const navigate = useNavigate();
  const selected = () => (params.name ? decodeURIComponent(params.name) : null);
  const [list, { refetch }] = createResource(() => api.skills.list());
  const [creating, setCreating] = createSignal(false);
  const [q, setQ] = createSignal("");
  const [notice, setNotice] = createSignal<string | null>(null);
  const shown = (): SkillSummary[] => filterSkills(list() ?? [], q());

  return (
    <>
      <PageHead title="Skills" level={2}>
        <input type="search" placeholder="Find a skill" value={q()} onInput={(e) => setQ(e.currentTarget.value)} aria-label="Find a skill" />
        <button onClick={() => void refetch()}>Refresh</button>
        <button class="primary" disabled={noSkills(list.error)} onClick={() => setCreating(true)}>
          New skill
        </button>
      </PageHead>
      <Show when={!noSkills(list.error)} fallback={<p class="notice">{NO_SKILLS_NOTE}</p>}>
        <p class="muted small">
          A skill is a folder with SKILL.md and other files. Terrarium gives each agent the skills of its list at the start. Pick the skills of an agent on its page, or in a template.
        </p>
        <CommitNote />
        <ErrorBanner error={list.error} />
        <Show when={notice()}>
          <p class="notice small" role="status" style={{ margin: 0 }}>
            {notice()}
          </p>
        </Show>
        <Show when={creating()}>
          <NewSkillForm
            onCancel={() => setCreating(false)}
            onCreated={(d) => {
              setCreating(false);
              setNotice(`Made the skill ${d.name}.`);
              void refetch();
              navigate(`/settings/skills/${encodeURIComponent(d.name)}`);
            }}
          />
        </Show>
        <Show when={selected()} keyed>
          {(name) => (
            <SkillEditor
              name={name}
              onSaved={() => void refetch()}
              onDeleted={() => {
                setNotice(`Deleted the skill ${name}.`);
                void refetch();
                navigate("/settings/skills");
              }}
            />
          )}
        </Show>
        <Show when={shown().length} fallback={<Empty>{list.loading ? "Loading…" : q() ? "No skills match." : "There are no skills."}</Empty>}>
          <div class="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Description</th>
                  <th>Files</th>
                  <th>Size</th>
                  <th>Used by</th>
                  <th>Checks</th>
                </tr>
              </thead>
              <tbody>
                <For each={shown()}>
                  {(s) => (
                    <tr aria-current={s.name === selected() ? "true" : undefined}>
                      <td>
                        <A class="mono" href={`/settings/skills/${encodeURIComponent(s.name)}`}>
                          {s.name}
                        </A>
                        <Show when={s.default_on}>
                          {" "}
                          <span class="chip" title={DEFAULT_ON_TITLE}>
                            default
                          </span>
                        </Show>
                      </td>
                      <td class="small">{s.description || <span class="muted">none</span>}</td>
                      <td>{s.files}</td>
                      <td class="small">{bytes(s.bytes)}</td>
                      <td>
                        <UsedByCell name={s.name} />
                      </td>
                      <td>
                        <Show when={s.errors?.length} fallback={<span class="chip ok">good</span>}>
                          <span class="chip bad" title={s.errors!.join("\n")}>
                            {s.errors!.length} problem{s.errors!.length === 1 ? "" : "s"}
                          </span>
                        </Show>
                      </td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
        </Show>
      </Show>
    </>
  );
}
