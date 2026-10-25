// The state of the create-target dialog (docs/api.md §17.2): the form, the
// body of `POST /api/targets`, and the answer. The scope is the selected
// project by default; "global" is only an explicit choice. A `403
// forbidden` (a global target needs a sysadmin; a project target needs a
// project_admin of that project, §10) and the `409` of the project cap
// show apart from other errors. A vmlab target has one of three lab
// sources: a lab dir on the host, a lab repo of the project (a worktree for
// the target), or the workspace of each holder (one instance per holder).
// A project target gets the name `<project>-<name>`. No JSX; the tests
// drive it with the mock API.

import { createSignal } from "solid-js";
import type { Api } from "../../api/client";
import { ApiError } from "../../api/http";
import type { NewTarget, Target, TargetKind } from "../../api/types";
import { capError, fullTargetName, type CapError, type LabSource } from "../../lib/target-runtime";

/** The scope choice: a project name, or `GLOBAL`. Empty: not chosen yet. */
export const GLOBAL = "global" as const;

export interface CreateTargetForm {
  name: string;
  kind: TargetKind;
  /** A project name, `GLOBAL`, or "" (not chosen). */
  scope: string;
  host: string;
  address: string;
  port: string;
  credential: string;
  image: string;
  container: string;
  /** vmlab: where the lab comes from. */
  labSource: LabSource;
  lab: string;
  /** vmlab, source `repo`: a repo name of the project. */
  lab_repo: string;
  lab_ref: string;
  /** vmlab, source `repo` or `workspace`: the dir that holds `vmlab.wcl`. */
  lab_path: string;
  machine: string;
  ephemeral: boolean;
  keep: boolean;
  snapshot_before_task: boolean;
}

export type CreateOutcome =
  | { kind: "idle" }
  | { kind: "sending" }
  | { kind: "invalid"; message: string }
  /** `403 forbidden`: the viewer may not define a target in that scope. */
  | { kind: "forbidden"; message: string; scope: string }
  /** `409 conflict`: the project has `TERRARIUM_MAX_TARGETS_PER_PROJECT` targets. */
  | { kind: "cap"; cap: CapError }
  | { kind: "error"; error: unknown }
  | { kind: "done"; target: Target };

/** The kinds, and the fields each one shows (docs/api.md §17.2). */
export const KIND_FIELDS: Record<TargetKind, readonly (keyof CreateTargetForm)[]> = {
  host: ["host"],
  ssh: ["address", "port", "credential"],
  docker: ["host", "image", "container", "ephemeral", "keep"],
  vmlab: ["host", "labSource", "lab", "lab_repo", "lab_ref", "lab_path", "machine", "ephemeral", "keep", "snapshot_before_task"],
};

/** The fields of each lab source (docs/api.md §17.2). */
export const SOURCE_FIELDS: Record<LabSource, readonly (keyof CreateTargetForm)[]> = {
  dir: ["lab", "snapshot_before_task"],
  repo: ["lab_repo", "lab_ref", "lab_path", "snapshot_before_task"],
  workspace: ["lab_path"],
};
const SOURCE_ONLY: readonly (keyof CreateTargetForm)[] = ["lab", "lab_repo", "lab_ref", "lab_path", "snapshot_before_task"];

/** The lab sources, with the sentence the dialog shows for each. */
export const LAB_SOURCES: readonly { id: LabSource; label: string; help: string }[] = [
  { id: "dir", label: "Lab directory on the host", help: "A lab dir that the operator wrote on the host. No agent workspace is attached." },
  { id: "repo", label: "Lab repo", help: "A repo of the project. On up, the host makes a git worktree of it at the ref, and runs vmlab.wcl in the path. One instance for the target." },
  { id: "workspace", label: "Lab from workspace", help: "The vmlab.wcl in the workspace of each holder. One instance per holder; a lease on it cannot be exclusive, and there is no snapshot before each task." },
];

/** Whether the form shows a field: the kind's fields, and for vmlab the fields of the lab source. */
export const showsField = (kind: TargetKind, field: keyof CreateTargetForm, source: LabSource = "dir") =>
  KIND_FIELDS[kind].includes(field) && (kind !== "vmlab" || !SOURCE_ONLY.includes(field) || SOURCE_FIELDS[source].includes(field));

/** The name the daemon gives the target: `<project>-<name>` for a project scope. */
export const formFullName = (f: Pick<CreateTargetForm, "name" | "scope">): string => fullTargetName(f.name, f.scope && f.scope !== GLOBAL ? f.scope : null);

/** The empty form. The scope is the selected project; with no project, the viewer must choose. Never global by default. */
export const emptyCreateForm = (selectedProject: string | null | undefined, init: Partial<CreateTargetForm> = {}): CreateTargetForm => ({
  name: "",
  kind: "docker",
  scope: selectedProject ?? "",
  host: "",
  address: "",
  port: "",
  credential: "",
  image: "",
  container: "",
  labSource: "dir",
  lab: "",
  lab_repo: "",
  lab_ref: "",
  lab_path: "",
  machine: "",
  ephemeral: false,
  keep: false,
  snapshot_before_task: false,
  ...init,
});

/** A value that goes onto a command line: not empty, no spaces, no leading `-` (docs/api.md §17.2). */
const badArg = (v: string) => /\s/.test(v) || v.startsWith("-");

/** A relative directory with no `..` (docs/api.md §17.2, `lab_path`). */
const badLabPath = (v: string) => v.startsWith("/") || v.split("/").some((s) => s === ".." || s.startsWith("-")) || /[\s\p{Cc}]/u.test(v);

/** The body of `POST /api/targets` from the form, or the reason the form is not complete. */
export function createBody(f: CreateTargetForm): { body: NewTarget } | { error: string } {
  const name = f.name.trim();
  if (!name) return { error: "Give the target a name." };
  if (!f.scope) return { error: "Choose a scope: a project, or global." };
  const body: NewTarget = { name, kind: f.kind };
  if (f.scope !== GLOBAL) body.project = f.scope;
  const shows = (k: keyof CreateTargetForm) => showsField(f.kind, k, f.labSource);
  const text = ["host", "address", "credential", "image", "container", "lab", "machine", "lab_repo", "lab_ref", "lab_path"] as const;
  for (const k of text) {
    const v = f[k].trim();
    if (!v || !shows(k)) continue;
    if (k === "lab_path") {
      if (badLabPath(v)) return { error: "The lab path must be a relative directory with no `..`." };
    } else if (k !== "lab" && badArg(v)) return { error: `\`${k}\` must not hold spaces or start with "-".` };
    body[k] = v;
  }
  if (shows("port") && f.port.trim()) {
    const port = Number(f.port.trim());
    if (!Number.isInteger(port) || port < 1 || port > 65535) return { error: "The port must be a number from 1 to 65535." };
    body.port = port;
  }
  const need = (field: string, ok: boolean) => (ok ? null : { error: `A ${f.kind} target needs ${field}.` });
  const missing =
    f.kind === "ssh"
      ? need("an address", !!body.address)
      : f.kind === "docker"
        ? (need("a host", !!body.host) ?? need("an image or a container", !!(body.image || body.container)))
        : f.kind === "vmlab"
          ? (need("a host", !!body.host) ?? vmlabMissing(f, body))
          : need("a host", !!body.host);
  if (missing) return missing;
  if (body.lab && !body.lab.startsWith("/")) return { error: "The lab must be an absolute path." };
  if (shows("ephemeral") && f.ephemeral) {
    if (f.kind === "docker" && !body.image) return { error: "An ephemeral docker target needs an image." };
    body.ephemeral = true;
    if (f.keep) body.keep = true;
  }
  if (shows("snapshot_before_task") && f.snapshot_before_task) body.snapshot_before_task = true;
  return { body };
}

/** The vmlab rules of each lab source (docs/api.md §17.2). Sets `lab_from_workspace` on the body. */
function vmlabMissing(f: CreateTargetForm, body: NewTarget): { error: string } | null {
  if (f.labSource === "dir") return body.lab ? null : { error: "A vmlab target needs a lab directory." };
  if (f.scope === GLOBAL) return { error: "A lab repo or a lab from the workspace needs a project target." };
  if (f.labSource === "repo" && !body.lab_repo) return { error: "Choose the lab repo." };
  if (!body.machine) return { error: "A lab repo or a lab from the workspace needs the machine that gets the workspace." };
  if (f.labSource === "workspace") body.lab_from_workspace = true;
  return null;
}

export function createCreateTargetDialog(api: Pick<Api, "targets">, selectedProject: string | null | undefined, init: Partial<CreateTargetForm> = {}) {
  const [form, setForm] = createSignal<CreateTargetForm>(emptyCreateForm(selectedProject, init));
  const [outcome, setOutcome] = createSignal<CreateOutcome>({ kind: "idle" });
  return {
    form,
    outcome,
    /** The name the target gets: `<project>-<name>` for a project scope. */
    fullName: () => formFullName(form()),
    /** Change one field. A change clears a refusal. */
    set<K extends keyof CreateTargetForm>(key: K, value: CreateTargetForm[K]) {
      setForm((f) => ({ ...f, [key]: value }));
      if (outcome().kind !== "sending" && outcome().kind !== "done") setOutcome({ kind: "idle" });
    },
    /** Send the target. Answers it, or null on a refusal (see `outcome`). */
    async submit(): Promise<Target | null> {
      if (outcome().kind === "sending") return null;
      const b = createBody(form());
      if ("error" in b) {
        setOutcome({ kind: "invalid", message: b.error });
        return null;
      }
      setOutcome({ kind: "sending" });
      try {
        const target = await api.targets.create(b.body);
        setOutcome({ kind: "done", target });
        return target;
      } catch (e) {
        const cap = capError(e);
        if (e instanceof ApiError && e.status === 403) setOutcome({ kind: "forbidden", message: e.message, scope: form().scope });
        else if (cap) setOutcome({ kind: "cap", cap });
        else setOutcome({ kind: "error", error: e });
        return null;
      }
    },
  };
}

export type CreateTargetDialogState = ReturnType<typeof createCreateTargetDialog>;
