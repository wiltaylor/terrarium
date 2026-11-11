import { useInstance } from "../../instance";
// The Artifacts page of a project (no tab since TER-507; old links still open it): the files of
// the project and the viewer of the artifact of the URL (/p/:project/board/artifacts/:id).

import { A, useNavigate, useParams } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import { ArtifactViewer } from "../../components/ArtifactViewer";
import { ConfirmButton, Empty, ErrorBanner, Modal, PageHead } from "../../components/ui";
import type { Artifact } from "../../api/types";
import { bundlePaths, buildTar } from "../../lib/tar";
import { ago, bytes } from "../../lib/format";

import { attachTarget } from "./attach";
import { labelHref } from "../board/versionLabels";

export default function ArtifactsPage() {
  const { api } = useInstance();
  const params = useParams<{ project: string; id?: string }>();
  const navigate = useNavigate();
  const [list, { refetch }] = createResource(() => params.project, (p) => api.artifacts.list(p).catch((e: unknown) => (setError(e), [] as Artifact[])));
  const [error, setError] = createSignal<unknown>(null);
  const [uploading, setUploading] = createSignal(false);
  const selected = () => list()?.find((a) => a.id === params.id) ?? list()?.[0];
  const [usage, { refetch: refetchUsage }] = createResource(() => params.project, (p) => api.artifacts.usage(p).catch(() => null));
  const total = () => usage()?.used_bytes ?? (list() ?? []).reduce((n, a) => n + a.size, 0);
  const remove = async (a: Artifact) => {
    try {
      await api.artifacts.remove(a.uid);
      void refetch();
      void refetchUsage();
      navigate(`/p/${params.project}/board/artifacts`);
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="Artifacts" level={2}>
        <span class="muted small">
          {list()?.length ?? 0} files · {bytes(total())} used
          {usage() ? ` of ${bytes(usage()!.quota_bytes)}` : ""}
        </span>
        <button class="primary" onClick={() => setUploading(true)}>
          Upload
        </button>
      </PageHead>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={uploading()}>
        <UploadModal
          project={params.project}
          onClose={() => setUploading(false)}
          onDone={(a) => {
            void refetch();
            void refetchUsage();
            navigate(`/p/${params.project}/board/artifacts/${a.id}`);
          }}
        />
      </Show>
      <div class="split">
        <div class="card-box thread-list" style={{ padding: "4px" }}>
          <For each={list()} fallback={<Empty>No artifacts.</Empty>}>
            {(a) => (
              <A href={`/p/${params.project}/board/artifacts/${a.id}`} style={{ display: "block", padding: "8px 6px", "border-bottom": "1px solid var(--border)", background: selected()?.id === a.id ? "var(--accent-soft)" : undefined }}>
                <div>
                  <span class="mono muted small">{a.id}</span> {a.name}
                </div>
                <div class="muted small">
                  {a.kind === "bundle" ? "bundle" : a.content_type} · {bytes(a.size)} · {a.created_by_name ?? a.created_by}, {ago(a.created_at)}
                </div>
              </A>
            )}
          </For>
        </div>
        <Show when={selected()}>
          {(a) => (
            <div class="stack">
              <ArtifactViewer artifact={a()} />
              <Show when={a().links.length}>
                <div class="small">
                  Attached to:{" "}
                  <For each={a().links}>
                    {(l) => (
                      <span class="chip" style={{ "margin-right": "4px" }}>
                        {/* A label link shows the label itself (`version:1.4.0`); a version label opens the board filtered by it. */}
                        <Show when={l.kind === "label"} fallback={`${l.kind}:${l.label ?? l.ref}`}>
                          <Show when={labelHref(a().project, l.ref)} fallback={l.ref}>
                            {(href) => <A href={href()}>{l.ref}</A>}
                          </Show>
                        </Show>
                      </span>
                    )}
                  </For>
                </div>
              </Show>
              <Show when={(a().embedded_in ?? []).length}>
                <div class="small">
                  Embedded in:{" "}
                  <For each={a().embedded_in}>
                    {(k) => (
                      <A href={`/p/${k.split("/")[0]}/knowledge/wiki/${k.split("/").slice(1).join("/")}`} style={{ "margin-right": "6px" }}>
                        {k}
                      </A>
                    )}
                  </For>
                </div>
              </Show>
              <div class="muted small mono ellipsis">sha256 {a().sha256}</div>
              <div>
                <ConfirmButton label="Delete" onConfirm={() => remove(a())} />
              </div>
            </div>
          )}
        </Show>
      </div>
    </>
  );
}

/**
 * Upload files, or a folder as one bundle (PLAN §15.2, §17). A folder goes
 * up as a tar the browser builds; the daemon unpacks it and serves its
 * `index.html`. The multipart fields are `file`, `name`, `kind`, `attach`.
 */
function UploadModal(props: { project: string; onClose: () => void; onDone: (a: Artifact) => void }) {
  const { api } = useInstance();
  const [mode, setMode] = createSignal<"files" | "folder">("files");
  const [files, setFiles] = createSignal<File[]>([]);
  const [name, setName] = createSignal("");
  const [attach, setAttach] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const [progress, setProgress] = createSignal("");

  const send = async (file: Blob, fileName: string, kind: "file" | "bundle") => {
    const form = new FormData();
    form.set("file", file, fileName);
    form.set("name", fileName);
    form.set("kind", kind);
    // docs/api.md §12.2: `attach_kind` and `attach_ref` (or `attach` as JSON).
    const t = attachTarget(attach());
    if (t) {
      form.set("attach_kind", t.kind);
      form.set("attach_ref", t.ref);
    }
    return api.artifacts.upload(props.project, form);
  };

  const submit = async (e: Event) => {
    e.preventDefault();
    if (!files().length) return;
    setBusy(true);
    setError(null);
    try {
      let last: Artifact | undefined;
      if (mode() === "folder") {
        const rels = bundlePaths(files().map((f) => (f as File & { webkitRelativePath?: string }).webkitRelativePath || f.name));
        setProgress(`Packing ${files().length} files…`);
        const entries = await Promise.all(files().map(async (f, i) => ({ path: rels[i]!, data: new Uint8Array(await f.arrayBuffer()), mtime: f.lastModified / 1000 })));
        const tar = buildTar(entries);
        const top = ((files()[0] as File & { webkitRelativePath?: string }).webkitRelativePath || "bundle").split("/")[0]!;
        setProgress("Uploading the bundle…");
        last = await send(new Blob([tar.buffer as ArrayBuffer], { type: "application/x-tar" }), name().trim() || top, "bundle");
      } else {
        for (const [i, f] of files().entries()) {
          setProgress(`Uploading ${i + 1} of ${files().length}: ${f.name}`);
          last = await send(f, files().length === 1 && name().trim() ? name().trim() : f.name, "file");
        }
      }
      if (last) props.onDone(last);
      props.onClose();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
      setProgress("");
    }
  };

  return (
    <Modal title="Upload artifacts" onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <form class="stack" onSubmit={submit}>
        <div class="row">
          <label class="row small">
            <input type="radio" name="mode" checked={mode() === "files"} onChange={() => (setMode("files"), setFiles([]))} /> Files
          </label>
          <label class="row small">
            <input type="radio" name="mode" checked={mode() === "folder"} onChange={() => (setMode("folder"), setFiles([]))} /> Folder as a static bundle
          </label>
        </div>
        <Show
          when={mode() === "folder"}
          fallback={<input type="file" multiple onChange={(e) => setFiles([...(e.currentTarget.files ?? [])])} aria-label="Files" />}
        >
          {/* webkitdirectory picks a folder; every file keeps its relative path. */}
          <input type="file" {...{ webkitdirectory: true, directory: true }} multiple onChange={(e) => setFiles([...(e.currentTarget.files ?? [])])} aria-label="Folder" />
        </Show>
        <Show when={files().length}>
          <p class="muted small" style={{ margin: 0 }}>
            {files().length} file{files().length === 1 ? "" : "s"}, {bytes(files().reduce((n, f) => n + f.size, 0))}
          </p>
        </Show>
        <label>
          Name {mode() === "folder" ? "(empty: the folder name)" : "(one file only; empty: the file name)"}
          <input value={name()} onInput={(e) => setName(e.currentTarget.value)} />
        </label>
        <label>
          Attach to (optional): page:&lt;path&gt;, card:&lt;id&gt;, label:&lt;label&gt;, or version:&lt;name&gt; (the label version:&lt;name&gt;)
          <input value={attach()} onInput={(e) => setAttach(e.currentTarget.value)} placeholder="card:TER-12" />
        </label>
        <Show when={progress()}>
          <p class="small" style={{ margin: 0 }}>
            {progress()}
          </p>
        </Show>
        <div class="row">
          <button class="primary" type="submit" disabled={busy() || !files().length}>
            Upload
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
