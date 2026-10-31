// The Files tab of the repo browser: a file tree on the left and a preview
// of the selection on the right, as on GitHub. The query `path` keeps the
// selection (a file or a folder; "" is the root). On a phone the tree goes
// above the preview and closes after a file is selected.
//
// Agents control every name and every file here. They go into text nodes
// only. The one HTML render is a Markdown file with "Rendered" on, through
// the safe Markdown component (lib/sanitize).

import { createEffect, createMemo, createResource, createSignal, For, Match, on, onCleanup, Show, Switch } from "solid-js";
import { buildQuery, seg } from "../../api/http";
import { isMock } from "../../api";
import type { Repo, TreeEntry } from "../../api/types";
import { createRovingTree } from "../../app/treeNav";
import { Markdown } from "../../components/Markdown";
import { CopyButton, Empty, ErrorBanner } from "../../components/ui";
import { crumbs, dirOf, imageType, isMarkdown } from "../../lib/diff";
import { bytes } from "../../lib/format";
import { renderMarkdown } from "../../lib/markdown";
import { api, auth } from "../../stores";
import { createTreeCache, isEntryKey, visibleRows, type FileTree } from "./fileTree";

// The trees stay loaded while the page lives: a tab change or a tree change
// does not load them again.
const treeCache = createTreeCache();

const WIDTH_KEY = "terrarium.repo.treeWidth";
const MIN_W = 180;
const MAX_W = 640;
const DEFAULT_W = 300;

const NARROW = "(max-width: 760px)";
const narrow = () => typeof matchMedia === "function" && matchMedia(NARROW).matches;

function storedWidth(): number {
  try {
    const n = Number(localStorage.getItem(WIDTH_KEY));
    return n >= MIN_W && n <= MAX_W ? n : DEFAULT_W;
  } catch {
    return DEFAULT_W;
  }
}

/**
 * The URL of the raw bytes. The daemon sends text as `text/plain` and other
 * bytes as an attachment, both with `nosniff`, so a new tab does not run them.
 * Only a cookie session can open it: a bearer token is not in a link.
 */
function rawUrl(repo: string, tree: string, path: string): string | null {
  if (isMock || auth.mode() !== "session") return null;
  return `${import.meta.env.VITE_API_BASE ?? ""}/api/repos/${seg(repo)}/trees/${seg(tree)}/file${buildQuery({ path })}`;
}

export function FilesTab(props: { repo: Repo; tree: string; head?: string | null; reload: string; path: string; onPath: (p: string) => void }) {
  const ft = createMemo(() => {
    const id = props.repo.id;
    const tree = props.tree;
    return treeCache(`${id}|${tree}|${props.head ?? ""}|${props.reload}`, (dir) => api.repos.files(id, tree, dir));
  });
  // Open the folders above the selection (a deep link loads each level), and a selected folder.
  createEffect(
    on([ft, () => props.path], ([t, p]) => {
      void t.reveal(p).then(() => {
        if (p && t.entry(p)?.kind === "dir") void t.open(p);
      });
    }),
  );

  const [width, setWidth] = createSignal(storedWidth());
  const keepWidth = (w: number) => {
    const n = Math.round(Math.min(MAX_W, Math.max(MIN_W, w)));
    setWidth(n);
    try {
      localStorage.setItem(WIDTH_KEY, String(n));
    } catch {
      // No storage: the width lasts for this page only.
    }
  };
  // The tree on a phone: open until a file is selected.
  const [treeOpen, setTreeOpen] = createSignal(true);
  let toggleBtn: HTMLButtonElement | undefined;

  const select = (path: string, kind?: TreeEntry["kind"]) => {
    props.onPath(path);
    if (kind && kind !== "dir" && narrow()) {
      setTreeOpen(false);
      toggleBtn?.focus();
    }
  };

  return (
    <div class={`repo-browser${treeOpen() ? "" : " tree-closed"}`} style={{ "--tree-w": `${width()}px` }}>
      <button ref={toggleBtn} class="repo-tree-toggle small" aria-expanded={treeOpen()} aria-controls="repo-file-tree" onClick={() => setTreeOpen(!treeOpen())}>
        <span class={`tree-chevron${treeOpen() ? " open" : ""}`} aria-hidden="true">
          ▸
        </span>
        Files
      </button>
      <TreePane tree={ft()} selected={props.path} onSelect={select} />
      <Splitter width={width()} onWidth={keepWidth} />
      <div class="repo-preview">
        <Preview repo={props.repo} tree={props.tree} ft={ft()} reload={props.reload} path={props.path} onPath={(p) => select(p)} />
      </div>
    </div>
  );
}

// --- The tree ----------------------------------------------------------

const NOTE_TEXT: Record<string, string> = { loading: "Loading…", empty: "Empty folder.", error: "This folder did not load.", truncated: "More than 5000 entries: the list shows the first 5000." };

function TreePane(props: { tree: FileTree; selected: string; onSelect: (path: string, kind: TreeEntry["kind"]) => void }) {
  const [query, setQuery] = createSignal("");
  const items = createMemo(() => visibleRows(props.tree.dirs(), props.tree.expanded(), query()));
  const byKey = createMemo(() => new Map(items().map((i) => [i.key, i])));
  // Plain strings in the For: a load or a selection keeps each row's node, so the focus stays.
  const keys = createMemo(() => items().map((i) => i.key));
  const rowKeys = createMemo(() => keys().filter(isEntryKey));
  const nav = createRovingTree({ keys: rowKeys, preferred: () => props.selected });

  const activate = (e: TreeEntry) => {
    if (e.kind === "dir") void props.tree.open(e.path);
    props.onSelect(e.path, e.kind);
  };

  const onKeyDown = (ev: KeyboardEvent) =>
    nav.onKeyDown(ev, (target) => {
      const key = target.closest<HTMLElement>("[data-key]")?.dataset.key;
      const item = key ? byKey().get(key) : undefined;
      if (!item || item.type !== "entry") return null;
      const e = item.entry;
      return {
        key: e.path,
        parent: dirOf(e.path) || undefined,
        open: item.open,
        button: true,
        onOpen: () => void props.tree.open(e.path),
        onClose: () => props.tree.close(e.path),
        onActivate: () => activate(e),
      };
    });

  return (
    <div class="repo-tree-pane" id="repo-file-tree">
      <input type="search" class="tree-filter" placeholder="Filter the loaded files" aria-label="Filter the loaded files" value={query()} onInput={(e) => setQuery(e.currentTarget.value)} />
      <ul class="tree file-tree" role="tree" aria-label="Files" ref={nav.ref} onKeyDown={onKeyDown}>
        <For each={keys()}>
          {(key) => {
            const entry = () => {
              const i = byKey().get(key);
              return i?.type === "entry" ? i : undefined;
            };
            const note = () => {
              const i = byKey().get(key);
              return i?.type === "note" ? i : undefined;
            };
            return (
              <>
                <Show when={note()}>
                  {(n) => (
                    <li role="none" class="tree-empty small muted" style={{ "padding-left": `${n().depth * 14 + 22}px` }}>
                      {NOTE_TEXT[n().note]}
                    </li>
                  )}
                </Show>
                <Show when={entry()}>{(r) => <TreeRow item={r()} selected={props.selected === key} nav={nav} onToggle={() => props.tree.toggle(key)} onActivate={() => activate(r().entry)} />}</Show>
              </>
            );
          }}
        </For>
      </ul>
      <Show when={query().trim() && !rowKeys().length}>
        <p class="small muted tree-empty">No loaded file has this name. The filter reads the open folders only.</p>
      </Show>
    </div>
  );
}

function TreeRow(props: {
  item: { key: string; entry: TreeEntry; depth: number; open?: boolean };
  selected: boolean;
  nav: ReturnType<typeof createRovingTree>;
  onToggle: () => void;
  onActivate: () => void;
}) {
  const e = () => props.item.entry;
  const isDir = () => e().kind === "dir";
  return (
    <li
      role="treeitem"
      data-key={props.item.key}
      aria-level={props.item.depth + 1}
      aria-expanded={isDir() ? !!props.item.open : undefined}
      aria-selected={props.selected}
      tabIndex={props.nav.tabIndex(props.item.key)}
      onFocus={() => props.nav.setFocusKey(props.item.key)}
      onClick={() => {
        props.nav.setFocusKey(props.item.key);
        props.onActivate();
      }}
    >
      <span class={`tree-row${props.selected ? " current" : ""}`} style={{ "padding-left": `${props.item.depth * 14 + 2}px` }} title={e().path}>
        <Show when={isDir()} fallback={<span class="tree-chevron" aria-hidden="true" />}>
          <span
            class={`tree-chevron${props.item.open ? " open" : ""}`}
            aria-hidden="true"
            title={props.item.open ? "Close" : "Open"}
            onClick={(ev) => {
              // Toggle only: do not select.
              ev.stopPropagation();
              props.nav.setFocusKey(props.item.key);
              props.onToggle();
            }}
          >
            ▸
          </span>
        </Show>
        <EntryIcon kind={e().kind} />
        <span class="tree-name">{e().name}</span>
        <Show when={e().kind === "symlink"}>
          <span class="small muted tree-link">→ {e().target ?? "?"}</span>
        </Show>
        <Show when={e().kind === "submodule"}>
          <span class="chip">submodule</span>
        </Show>
      </span>
    </li>
  );
}

/** A small outline icon: a folder, a file, or a link. */
function EntryIcon(props: { kind: TreeEntry["kind"] }) {
  return (
    <svg class={`entry-icon entry-${props.kind}`} viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
      <Switch fallback={<path d="M4 1.5h5l3.5 3.5v9.5h-8.5z M9 1.5v3.5h3.5" />}>
        <Match when={props.kind === "dir" || props.kind === "submodule"}>
          <path d="M1.5 3.5h4.5l1.5 1.5h7v8.5h-13z" />
        </Match>
        <Match when={props.kind === "symlink"}>
          <path d="M4 1.5h5l3.5 3.5v9.5h-8.5z M6 11l4-4 M7 7h3v3" />
        </Match>
      </Switch>
    </svg>
  );
}

/** The bar between the panes: drag it, or focus it and use the arrows (Home, End). */
function Splitter(props: { width: number; onWidth: (w: number) => void }) {
  let start = 0;
  let startW = 0;
  const onKeyDown = (e: KeyboardEvent) => {
    const step = e.shiftKey ? 64 : 16;
    const to: Record<string, number> = { ArrowLeft: props.width - step, ArrowRight: props.width + step, Home: MIN_W, End: MAX_W };
    if (!(e.key in to)) return;
    e.preventDefault();
    props.onWidth(to[e.key]!);
  };
  return (
    <div
      class="repo-splitter"
      role="separator"
      aria-orientation="vertical"
      aria-label="Width of the file tree"
      aria-controls="repo-file-tree"
      aria-valuemin={MIN_W}
      aria-valuemax={MAX_W}
      aria-valuenow={props.width}
      tabIndex={0}
      onKeyDown={onKeyDown}
      onPointerDown={(e) => {
        start = e.clientX;
        startW = props.width;
        e.currentTarget.setPointerCapture(e.pointerId);
      }}
      onPointerMove={(e) => {
        if (e.currentTarget.hasPointerCapture(e.pointerId)) props.onWidth(startW + e.clientX - start);
      }}
    />
  );
}

// --- The preview -------------------------------------------------------

/** The path as links: root / a / b / c.md. Each part selects its folder; the last part is the selection. */
function Breadcrumbs(props: { path: string; onPath: (p: string) => void }) {
  const parts = () => crumbs(props.path);
  return (
    <nav class="crumbs mono small" aria-label="Path">
      <Show when={props.path} fallback={<span aria-current="page">root</span>}>
        <button class="link" onClick={() => props.onPath("")}>
          root
        </button>
      </Show>
      <For each={parts()}>
        {([name, p], i) => (
          <>
            <span class="muted"> / </span>
            <Show when={i() < parts().length - 1} fallback={<span aria-current="page">{name}</span>}>
              <button class="link" onClick={() => props.onPath(p)}>
                {name}
              </button>
            </Show>
          </>
        )}
      </For>
    </nav>
  );
}

function Preview(props: { repo: Repo; tree: string; ft: FileTree; reload: string; path: string; onPath: (p: string) => void }) {
  // The kind of the selection, from its folder's listing. The root is a folder.
  const parent = () => props.ft.dirs()[dirOf(props.path)];
  const entry = () => (props.path ? props.ft.entry(props.path) : ({ name: "", path: "", kind: "dir" } as TreeEntry));
  return (
    <div class="stack" style={{ gap: "8px", "min-width": 0 }}>
      <Switch
        fallback={
          <div class="card-box stack" style={{ gap: "8px" }}>
            <Breadcrumbs path={props.path} onPath={props.onPath} />
            <Show when={parent()?.status === "error"} fallback={<Empty>{parent()?.status === "ready" ? "No such file or folder in this tree." : "Loading…"}</Empty>}>
              <ErrorBanner error={(parent() as { error: unknown }).error} />
            </Show>
          </div>
        }
      >
        <Match when={entry()?.kind === "dir"}>
          <FolderView repo={props.repo} tree={props.tree} ft={props.ft} reload={props.reload} path={props.path} onPath={props.onPath} />
        </Match>
        <Match when={entry()?.kind === "file"}>
          <Show when={props.path} keyed>
            {(p) => <FileView repo={props.repo} tree={props.tree} reload={props.reload} path={p} onPath={props.onPath} />}
          </Show>
        </Match>
        <Match when={entry()}>
          {(e) => (
            <div class="card-box stack" style={{ gap: "8px" }}>
              <Breadcrumbs path={props.path} onPath={props.onPath} />
              <Switch fallback={<Empty>This entry is not a file or a folder. No content is shown.</Empty>}>
                <Match when={e().kind === "symlink"}>
                  <p style={{ margin: 0 }}>
                    A symlink to <span class="mono">{e().target ?? "?"}</span>. The daemon does not follow it.
                  </p>
                </Match>
                <Match when={e().kind === "submodule"}>
                  <Empty>A submodule. The browser does not show its files.</Empty>
                </Match>
              </Switch>
            </div>
          )}
        </Match>
      </Switch>
    </div>
  );
}

const README = /^readme\.(md|markdown)$/i;

/** A folder: its entries (as on GitHub), then its README rendered below. */
function FolderView(props: { repo: Repo; tree: string; ft: FileTree; reload: string; path: string; onPath: (p: string) => void }) {
  createEffect(() => void props.ft.ensure(props.path));
  const dir = () => props.ft.dirs()[props.path];
  const ready = () => {
    const d = dir();
    return d?.status === "ready" ? d : undefined;
  };
  const readme = () => ready()?.entries.find((e) => e.kind === "file" && README.test(e.name));
  return (
    <>
      <div class="card-box stack" style={{ gap: "8px" }}>
        <Breadcrumbs path={props.path} onPath={props.onPath} />
        <Show when={dir()?.status === "error"}>
          <ErrorBanner error={(dir() as { error: unknown }).error} />
        </Show>
        <Show when={ready()} fallback={<Show when={dir()?.status !== "error"}>{<Empty>Loading…</Empty>}</Show>}>
          {(d) => (
            <Show when={d().entries.length} fallback={<Empty>This folder is empty.</Empty>}>
              <div class="table-wrap">
                <table class="file-table">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th class="num">Size</th>
                    </tr>
                  </thead>
                  <tbody>
                    <Show when={props.path}>
                      <tr>
                        <td>
                          <button class="link mono" onClick={() => props.onPath(dirOf(props.path))} aria-label="The parent folder">
                            ..
                          </button>
                        </td>
                        <td />
                      </tr>
                    </Show>
                    <For each={d().entries}>
                      {(e) => (
                        <tr>
                          <td>
                            <span class="row" style={{ gap: "6px", "flex-wrap": "nowrap" }}>
                              <EntryIcon kind={e.kind} />
                              <button class="link mono" onClick={() => props.onPath(e.path)}>
                                {e.kind === "dir" ? `${e.name}/` : e.name}
                              </button>
                              <Show when={e.kind === "symlink"}>
                                <span class="mono small muted">→ {e.target ?? "?"}</span>
                              </Show>
                              <Show when={e.kind === "submodule"}>
                                <span class="chip">submodule</span>
                              </Show>
                            </span>
                          </td>
                          <td class="num small muted">{e.size != null ? bytes(e.size) : ""}</td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
              <Show when={d().truncated}>
                <p class="small muted">This folder has more than 5000 entries: the list shows the first 5000.</p>
              </Show>
            </Show>
          )}
        </Show>
      </div>
      <Show when={readme()} keyed>
        {(r) => <FileView repo={props.repo} tree={props.tree} reload={props.reload} path={r.path} onPath={props.onPath} readme />}
      </Show>
    </>
  );
}

/**
 * One file: a header (the path, the size, Raw, Copy path), then text with
 * line numbers, Markdown (rendered by default), an image, or a note.
 * `readme` gives the short header of the README under a folder.
 */
function FileView(props: { repo: Repo; tree: string; reload: string; path: string; onPath: (p: string) => void; readme?: boolean }) {
  const [rendered, setRendered] = createSignal(true);
  const [file] = createResource(
    () => [props.repo.id, props.tree, props.path, props.reload] as const,
    async ([id, tree, path]) => {
      const f = await api.repos.file(id, tree, path);
      return { ...f, content: f.text ? await f.blob.text() : "" };
    },
  );
  // An image: a blob URL with a fixed raster type. Never SVG, never the
  // type of the answer (application/octet-stream).
  const [imageUrl, setImageUrl] = createSignal<string | null>(null);
  createEffect(
    on(file, (f) => {
      const old = imageUrl();
      if (old) URL.revokeObjectURL(old);
      const type = imageType(props.path);
      setImageUrl(f && !f.text && type && !f.truncated ? URL.createObjectURL(new Blob([f.blob], { type })) : null);
    }),
  );
  onCleanup(() => {
    const u = imageUrl();
    if (u) URL.revokeObjectURL(u);
  });
  const lines = createMemo(() => {
    const c = file()?.content ?? "";
    const l = c.split("\n");
    if (l.length > 1 && l[l.length - 1] === "") l.pop();
    return l;
  });
  const raw = () => rawUrl(props.repo.id, props.tree, props.path);
  return (
    <div class="card-box stack file-view" style={{ gap: "8px" }}>
      <div class="file-head">
        <Show when={!props.readme} fallback={<span class="mono">{props.path.split("/").pop()}</span>}>
          <Breadcrumbs path={props.path} onPath={props.onPath} />
        </Show>
        <span class="row small muted file-actions">
          <Show when={file()}>{(f) => <span>{bytes(f().size)}</span>}</Show>
          <Show when={isMarkdown(props.path) && file()?.text}>
            <span class="seg" role="group" aria-label="Markdown view">
              <button class={rendered() ? "" : "active"} aria-pressed={!rendered()} onClick={() => setRendered(false)}>
                Source
              </button>
              <button class={rendered() ? "active" : ""} aria-pressed={rendered()} onClick={() => setRendered(true)}>
                Rendered
              </button>
            </span>
          </Show>
          <Show when={!props.readme}>
            <Show when={raw()}>
              {(u) => (
                <a class="btn small" href={u()} target="_blank" rel="noopener noreferrer" title="The bytes in a new tab (text), or a download">
                  Raw
                </a>
              )}
            </Show>
            <CopyButton text={props.path} label="Copy path" />
          </Show>
        </span>
      </div>
      <ErrorBanner error={file.error} />
      <Show when={file.loading}>
        <Empty>Loading…</Empty>
      </Show>
      <Show when={file()}>
        {(f) => (
          <Switch>
            <Match when={imageUrl()}>
              <img class="file-image" src={imageUrl()!} alt={props.path} />
            </Match>
            <Match when={!f().text}>
              <Empty>
                Binary, {bytes(f().size)} ({f().size} bytes). No content is shown.
              </Empty>
            </Match>
            <Match when={rendered() && isMarkdown(props.path)}>
              <Markdown html={renderMarkdown(f().content, props.repo.project)} />
            </Match>
            <Match when={true}>
              <div class="code-scroll">
                <table class="code-table">
                  <tbody>
                    <For each={lines()}>
                      {(l, i) => (
                        <tr>
                          <td class="ln">{i() + 1}</td>
                          <td class="code">{l}</td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </Match>
          </Switch>
        )}
      </Show>
      <Show when={file()?.truncated}>
        <p class="small muted">The file is {bytes(file()!.size)}: the view shows the first 1 MiB only.</p>
      </Show>
    </div>
  );
}
