// The event sources of the Triggers tab (docs/api.md §20): the list order,
// the body of a create, and the one-time secret. The secret is only in the
// answer of a create or a rotate; the list never keeps it, and the panel
// that shows it drops it when it closes. Nothing writes it to storage.
// Pure; no Solid imports.

import type { EventSource, EventSourceKind, EventSourceWithSecret, NewEventSource } from "../../api/types";
import { GLOBAL } from "./form";

export const SOURCE_KINDS: readonly { id: EventSourceKind; label: string }[] = [
  { id: "github", label: "GitHub" },
  { id: "forgejo", label: "Forgejo" },
  { id: "generic", label: "Generic (a script, signed or with a bearer secret)" },
];

export const sourceScopeLabel = (s: Pick<EventSource, "project">): string => s.project ?? "global";

/** Global first, then by project and name. */
export const sortSources = <T extends Pick<EventSource, "project" | "name">>(list: T[]): T[] =>
  list.slice().sort((a, b) => (a.project === null ? -1 : 0) - (b.project === null ? -1 : 0) || (a.project ?? "").localeCompare(b.project ?? "") || a.name.localeCompare(b.name));

/** The source without its secret: the only shape that goes into the list. */
export function withoutSecret(s: EventSource | EventSourceWithSecret): EventSource {
  const { secret: _secret, ...rest } = s as EventSourceWithSecret;
  return rest;
}

/** Put a source in the list (without its secret), or replace it. */
export const upsertSource = (list: EventSource[], s: EventSource | EventSourceWithSecret): EventSource[] => sortSources([...list.filter((x) => x.id !== s.id), withoutSecret(s)]);

export interface SourceForm {
  name: string;
  /** A project name, `GLOBAL`, or "" (not chosen). */
  scope: string;
  kind: EventSourceKind;
}

export type SourceFormKey = keyof SourceForm;

export const emptySourceForm = (project: string | null | undefined): SourceForm => ({ name: "", scope: project ?? "", kind: "github" });

const NAME_RE = /^[a-z0-9_-]{1,63}$/;

/** The body of `POST /api/event-sources`, or the reason the form is not complete. */
export function sourceBody(f: SourceForm): { body: NewEventSource } | { error: string; field: SourceFormKey } {
  const name = f.name.trim();
  if (!name) return { error: "Give the source a name.", field: "name" };
  if (!NAME_RE.test(name)) return { error: "The name is 1-63 of a-z, 0-9, - and _.", field: "name" };
  if (!f.scope) return { error: "Choose a scope: a project, or global.", field: "scope" };
  return { body: { name, kind: f.kind, project: f.scope === GLOBAL ? null : f.scope } };
}

/** The daemon's `details.field` of a `422`, as a form field; `project` is the scope. */
export const sourceFieldOf = (field: unknown): SourceFormKey | undefined => (field === "name" || field === "kind" ? field : field === "project" ? "scope" : undefined);
