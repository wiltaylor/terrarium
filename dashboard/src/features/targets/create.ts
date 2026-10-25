// The state of the create-target dialog (docs/api.md §17.2): the form, the
// body of `POST /api/targets`, and the answer. The scope is the selected
// project by default; "global" is only an explicit choice. A `403
// forbidden` (a global target needs a sysadmin; a project target needs a
// project_admin of that project, §10) shows apart from other errors. No
// JSX; the tests drive it with the mock API.

import { createSignal } from "solid-js";
import type { Api } from "../../api/client";
import { ApiError } from "../../api/http";
import type { NewTarget, Target, TargetKind } from "../../api/types";

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
  lab: string;
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
  | { kind: "error"; error: unknown }
  | { kind: "done"; target: Target };

/** The kinds, and the fields each one shows (docs/api.md §17.2). */
export const KIND_FIELDS: Record<TargetKind, readonly (keyof CreateTargetForm)[]> = {
  host: ["host"],
  ssh: ["address", "port", "credential"],
  docker: ["host", "image", "container", "ephemeral", "keep"],
  vmlab: ["host", "lab", "machine", "ephemeral", "keep", "snapshot_before_task"],
};

export const showsField = (kind: TargetKind, field: keyof CreateTargetForm) => KIND_FIELDS[kind].includes(field);

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
  lab: "",
  machine: "",
  ephemeral: false,
  keep: false,
  snapshot_before_task: false,
  ...init,
});

/** A value that goes onto a command line: not empty, no spaces, no leading `-` (docs/api.md §17.2). */
const badArg = (v: string) => /\s/.test(v) || v.startsWith("-");

/** The body of `POST /api/targets` from the form, or the reason the form is not complete. */
export function createBody(f: CreateTargetForm): { body: NewTarget } | { error: string } {
  const name = f.name.trim();
  if (!name) return { error: "Give the target a name." };
  if (!f.scope) return { error: "Choose a scope: a project, or global." };
  const body: NewTarget = { name, kind: f.kind };
  if (f.scope !== GLOBAL) body.project = f.scope;
  const text = ["host", "address", "credential", "image", "container", "lab", "machine"] as const;
  for (const k of text) {
    const v = f[k].trim();
    if (!v || !showsField(f.kind, k)) continue;
    if (k !== "lab" && badArg(v)) return { error: `\`${k}\` must not hold spaces or start with "-".` };
    body[k] = v;
  }
  if (showsField(f.kind, "port") && f.port.trim()) {
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
          ? (need("a host", !!body.host) ?? need("a lab", !!body.lab))
          : need("a host", !!body.host);
  if (missing) return missing;
  if (body.lab && !body.lab.startsWith("/")) return { error: "The lab must be an absolute path." };
  if (showsField(f.kind, "ephemeral") && f.ephemeral) {
    if (f.kind === "docker" && !body.image) return { error: "An ephemeral docker target needs an image." };
    body.ephemeral = true;
    if (f.keep) body.keep = true;
  }
  if (showsField(f.kind, "snapshot_before_task") && f.snapshot_before_task) body.snapshot_before_task = true;
  return { body };
}

export function createCreateTargetDialog(api: Pick<Api, "targets">, selectedProject: string | null | undefined, init: Partial<CreateTargetForm> = {}) {
  const [form, setForm] = createSignal<CreateTargetForm>(emptyCreateForm(selectedProject, init));
  const [outcome, setOutcome] = createSignal<CreateOutcome>({ kind: "idle" });
  return {
    form,
    outcome,
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
        if (e instanceof ApiError && e.status === 403) setOutcome({ kind: "forbidden", message: e.message, scope: form().scope });
        else setOutcome({ kind: "error", error: e });
        return null;
      }
    },
  };
}

export type CreateTargetDialogState = ReturnType<typeof createCreateTargetDialog>;
