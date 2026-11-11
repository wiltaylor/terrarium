import { useInstance } from "../../instance";
// The Files section of the card drawer (docs/api.md §8.4): the files of the
// card, with a drop zone, paste, and an upload button. A file of a name the
// card has already is uploaded as a new version. Each file has a preview
// (markdown through the safe renderer, text as text, raster images inline),
// a download, its versions, and a remove. The list reloads on the
// `card.file_*` events of the card.

import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { Card, CardFile, CardFileVersion } from "../../api/types";
import { ConfirmButton, ErrorBanner } from "../../components/ui";
import { SafeHtml } from "../../components/SafeHtml";
import { ago, bytes } from "../../lib/format";
import { renderMarkdown } from "../../lib/markdown";

import { ACCEPT, existing, fileLine, fileProblem, isCardFileEvent, pastedFiles, previewOf, rasterType, safeName } from "./cardFileRules";

/** Save a blob as a file of the browser. */
function save(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

export function CardFiles(props: { card: Card }) {
  const { api, events } = useInstance();
  const [files, setFiles] = createSignal<CardFile[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [notes, setNotes] = createSignal<string[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [dragging, setDragging] = createSignal(false);
  const [open, setOpen] = createSignal<string | null>(null);
  let input!: HTMLInputElement;
  let gen = 0;

  const reload = async () => {
    const mine = ++gen;
    try {
      const rows = await api.cardFiles.list(props.card.id);
      if (mine !== gen) return;
      setFiles(rows);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoaded(true);
    }
  };
  createEffect(
    on(
      () => props.card.id,
      () => {
        setFiles([]);
        setLoaded(false);
        setOpen(null);
        void reload();
      },
    ),
  );
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (!isCardFileEvent(e, props.card.id)) return;
      clearTimeout(t);
      t = setTimeout(() => void reload(), 300);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });

  const upload = async (list: { file: File; name: string }[]) => {
    if (!list.length) return;
    const problems: string[] = [];
    setBusy(true);
    try {
      for (const { file, name } of list) {
        const p = fileProblem({ name, size: file.size });
        if (p) {
          problems.push(p);
          continue;
        }
        try {
          const replace = !!existing(files(), name);
          const r = await api.cardFiles.upload(props.card.id, file, name, replace);
          if (r.outcome === "unchanged") problems.push(`${r.name}: the same bytes as v${r.version}; no new version`);
          setFiles((xs) => [...xs.filter((x) => x.name.toLowerCase() !== r.name.toLowerCase()), r].sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase())));
        } catch (e) {
          setError(e);
        }
      }
    } finally {
      setBusy(false);
      setNotes(problems);
    }
  };
  const fromFiles = (list: FileList | null | undefined) => Array.from(list ?? []).map((file) => ({ file, name: safeName(file.name) }));

  return (
    <section
      class="stack card-files"
      classList={{ "card-files-drop": dragging() }}
      style={{ gap: "8px" }}
      aria-label="Files"
      tabIndex={0}
      onDragOver={(e) => {
        if (!e.dataTransfer?.types.includes("Files")) return;
        e.preventDefault();
        setDragging(true);
      }}
      onDragLeave={() => setDragging(false)}
      onDrop={(e) => {
        setDragging(false);
        if (!e.dataTransfer?.files.length) return;
        e.preventDefault();
        void upload(fromFiles(e.dataTransfer.files));
      }}
      onPaste={(e) => {
        const list = pastedFiles(e.clipboardData?.items, new Date());
        if (!list.length) return;
        e.preventDefault();
        void upload(list);
      }}
    >
      <div class="row" style={{ gap: "8px", "flex-wrap": "wrap", "align-items": "center" }}>
        <h3 style={{ margin: 0 }}>Files</h3>
        <button type="button" class="small" disabled={busy() || props.card.archived} onClick={() => input.click()}>
          {busy() ? "Uploading…" : "Upload"}
        </button>
        <input
          ref={input}
          type="file"
          multiple
          accept={ACCEPT}
          hidden
          aria-label="Upload files"
          onChange={(e) => {
            const list = fromFiles(e.currentTarget.files);
            e.currentTarget.value = "";
            void upload(list);
          }}
        />
      </div>
      <p class="small muted" style={{ margin: 0 }}>
        Drop or paste files here: markdown, text, JSON, YAML, CSV, images, SVG, PDF; 10 MiB each. A file with a name the card has gets a new version. The assignee gets them in <code>.terrarium/cards/{props.card.id}/</code>.
      </p>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <For each={notes()}>{(n) => <p class="small warn">{n}</p>}</For>
      <Show when={loaded()} fallback={<p class="muted small">Loading…</p>}>
        <For each={files()} fallback={<p class="muted small">No files.</p>}>
          {(f) => (
            <FileRow
              card={props.card}
              file={f}
              open={open() === f.name}
              onToggle={() => setOpen((o) => (o === f.name ? null : f.name))}
              onError={setError}
              onRemoved={() => setFiles((xs) => xs.filter((x) => x.name !== f.name))}
            />
          )}
        </For>
      </Show>
    </section>
  );
}

function FileRow(props: { card: Card; file: CardFile; open: boolean; onToggle: () => void; onError: (e: unknown) => void; onRemoved: () => void }) {
  const { api } = useInstance();
  const [versions, setVersions] = createSignal<CardFileVersion[] | null>(null);
  const download = async (version?: number) => {
    try {
      const r = await api.cardFiles.get(props.card.id, props.file.name, version);
      save(r.blob, version ? `v${version}-${r.fileName}` : r.fileName);
    } catch (e) {
      props.onError(e);
    }
  };
  const toggleVersions = async () => {
    if (versions()) return setVersions(null);
    try {
      setVersions(await api.cardFiles.versions(props.card.id, props.file.name));
    } catch (e) {
      props.onError(e);
    }
  };
  // A new version: the open version list is stale.
  createEffect(on(() => props.file.version, () => setVersions(null), { defer: true }));
  return (
    <div class="card-file">
      <div class="row" style={{ gap: "6px", "flex-wrap": "wrap", "align-items": "baseline" }}>
        <button type="button" class="link mono" aria-expanded={props.open} title="Show or hide the preview" onClick={() => props.onToggle()}>
          {props.file.name}
        </button>
        <span class="small muted" title={`${props.file.size} bytes, ${props.file.media_type}, ${props.file.created_at}`}>
          {fileLine(props.file, bytes(props.file.size), ago(props.file.created_at))}
        </span>
        <span class="row small" style={{ gap: "4px", "margin-left": "auto" }}>
          <button type="button" class="small" onClick={() => void download()}>
            Download
          </button>
          <Show when={props.file.versions > 1}>
            <button type="button" class="small" aria-expanded={!!versions()} onClick={() => void toggleVersions()}>
              {props.file.versions} versions
            </button>
          </Show>
          <ConfirmButton
            class="small danger"
            label="Remove"
            disabled={props.card.archived}
            onConfirm={async () => {
              try {
                await api.cardFiles.remove(props.card.id, props.file.name);
                props.onRemoved();
              } catch (e) {
                props.onError(e);
              }
            }}
          />
        </span>
      </div>
      <Show when={versions()}>
        {(vs) => (
          <ul class="small card-file-versions">
            <For each={vs()}>
              {(v) => (
                <li class="row" style={{ gap: "6px" }}>
                  <span>{fileLine(v, bytes(v.size), ago(v.created_at))}</span>
                  <button type="button" class="link small" onClick={() => void download(v.version)}>
                    Download v{v.version}
                  </button>
                </li>
              )}
            </For>
          </ul>
        )}
      </Show>
      <Show when={props.open}>
        <FilePreview card={props.card} file={props.file} onError={props.onError} />
      </Show>
    </div>
  );
}

/** The preview of the latest version. The text goes through the safe renderer; an image is a blob URL with a fixed raster type. */
function FilePreview(props: { card: Card; file: CardFile; onError: (e: unknown) => void }) {
  const { api } = useInstance();
  const kind = () => previewOf(props.file.name);
  const [text, setText] = createSignal<string | null>(null);
  const [image, setImage] = createSignal<string | null>(null);
  createEffect(
    on(
      () => [props.card.id, props.file.name, props.file.version] as const,
      async ([card, name]) => {
        setText(null);
        const old = image();
        if (old) URL.revokeObjectURL(old);
        setImage(null);
        if (kind() === "none") return;
        try {
          const { blob } = await api.cardFiles.get(card, name);
          const type = rasterType(name);
          if (kind() === "image" && type) setImage(URL.createObjectURL(new Blob([blob], { type })));
          else setText(await blob.text());
        } catch (e) {
          props.onError(e);
        }
      },
    ),
  );
  onCleanup(() => {
    const u = image();
    if (u) URL.revokeObjectURL(u);
  });
  return (
    <div class="card-file-preview">
      <Show when={kind() === "none"}>
        <p class="small muted">No preview for this type. Download it to open it.</p>
      </Show>
      <Show when={image()}>{(u) => <img class="card-file-image" src={u()} alt={props.file.name} />}</Show>
      <Show when={text() !== null && kind() === "markdown"}>
        <SafeHtml class="prose card-box" html={renderMarkdown(text() ?? "", props.card.project)} />
      </Show>
      <Show when={text() !== null && kind() === "text"}>
        <pre class="card-box card-file-text">{text()}</pre>
      </Show>
    </div>
  );
}
