// Turn any thrown value into something the UI can show, with the daemon's
// `error.details` (docs/api.md §2.5). Pure; no Solid imports.

import { ApiError, isMissingEndpoint } from "../api/http";

export const NOT_AVAILABLE = "Not available yet: this daemon does not have this endpoint.";

export interface ErrorDetail {
  label: string;
  value: string;
  /** Show as preformatted text: a diff, a multi-line value. */
  pre?: boolean;
}

export interface ErrorView {
  message: string;
  code?: string;
  status?: number;
  details: ErrorDetail[];
}

const text = (v: unknown): string => {
  if (typeof v === "string") return v;
  if (Array.isArray(v) && v.every((x) => typeof x === "string" || typeof x === "number")) return v.join(", ");
  return JSON.stringify(v, null, 2);
};

const LABELS: Record<string, string> = {
  blocked_by: "Blocked by",
  wip_limit: "WIP limit",
  column: "Column",
  diff: "Changes since your base",
  base: "Your base",
  current_rev: "Current revision",
  current: "Current revision",
};

/** The details of an error, as label and value pairs. Known keys get readable labels. */
export function describeDetails(details: unknown): ErrorDetail[] {
  if (details === undefined || details === null) return [];
  if (typeof details !== "object" || Array.isArray(details)) {
    const value = text(details);
    return [{ label: "Details", value, pre: value.includes("\n") }];
  }
  return Object.entries(details as Record<string, unknown>)
    .filter(([, v]) => v !== undefined && v !== null)
    .map(([k, v]) => {
      const value = text(v);
      return { label: LABELS[k] ?? k.replace(/_/g, " "), value, pre: k === "diff" || value.includes("\n") };
    });
}

export function describeError(e: unknown): ErrorView | null {
  if (e === null || e === undefined || e === false || e === "") return null;
  if (isMissingEndpoint(e)) return { message: NOT_AVAILABLE, code: (e as ApiError).code, status: (e as ApiError).status, details: [] };
  if (e instanceof ApiError) return { message: e.message, code: e.code, status: e.status, details: describeDetails(e.details) };
  if (e instanceof Error) return { message: e.message, details: [] };
  if (typeof e === "string") return { message: e, details: [] };
  return { message: text(e), details: [] };
}

/** The message of an error, for places that show one line. */
export const errorText = (e: unknown): string => describeError(e)?.message ?? "";
