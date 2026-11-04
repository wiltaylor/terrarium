// The rules of the Files section of the card drawer (docs/api.md §8.4): the
// types a card takes, the client-side checks before an upload, the names of
// dropped and pasted files, and the preview of each type. The daemon checks
// all of it again; these checks only save a round trip.

import type { AgentEvent, CardFile } from "../../api/types";

/** The largest file by default (`TERRARIUM_CARD_FILE_MAX_BYTES`). */
export const MAX_FILE_BYTES = 10 * 1024 * 1024;
/** The longest name, in bytes (the daemon's `MAX_NAME`). */
export const MAX_NAME = 128;

/** The extensions a card takes, and the `accept` of the file input. */
export const EXTENSIONS = ["md", "markdown", "txt", "json", "yaml", "yml", "csv", "png", "jpg", "jpeg", "gif", "webp", "svg", "pdf"] as const;
export const ACCEPT = EXTENSIONS.map((e) => `.${e}`).join(",");

export type Preview = "markdown" | "text" | "image" | "none";

const ext = (name: string): string => {
  const i = name.lastIndexOf(".");
  return i < 0 ? "" : name.slice(i + 1).toLowerCase();
};

/** How the drawer shows a file. An SVG is never shown inline: it downloads (a blob URL of an SVG would run on the dashboard origin when opened in a tab). A PDF downloads. */
export function previewOf(name: string): Preview {
  const e = ext(name);
  if (e === "md" || e === "markdown") return "markdown";
  if (["txt", "json", "yaml", "yml", "csv"].includes(e)) return "text";
  if (["png", "jpg", "jpeg", "gif", "webp"].includes(e)) return "image";
  return "none";
}

/** The raster type of an image name, for a blob URL with a fixed type. */
export function rasterType(name: string): string | null {
  const e = ext(name);
  if (e === "png") return "image/png";
  if (e === "jpg" || e === "jpeg") return "image/jpeg";
  if (e === "gif") return "image/gif";
  if (e === "webp") return "image/webp";
  return null;
}

/** Make a name the daemon takes: safe characters only, no leading dot, no `index.md`. The extension stays. */
export function safeName(raw: string): string {
  let n = raw.split(/[\\/]/).pop() ?? "";
  n = n.replace(/[^A-Za-z0-9._+ -]/g, "-").replace(/^[. ]+/, "").replace(/[. ]+$/, "");
  if (!n) n = "file";
  if (n.toLowerCase() === "index.md") n = "index-1.md";
  if (n.length > MAX_NAME) {
    const e = ext(n);
    const keep = e ? MAX_NAME - e.length - 1 : MAX_NAME;
    n = e ? `${n.slice(0, keep)}.${e}` : n.slice(0, keep);
  }
  return n;
}

/** The name of a pasted image: `pasted-<YYYYMMDD-HHMMSS>.<ext>` in UTC. */
export function pastedName(type: string, now: Date): string {
  const e = type === "image/jpeg" ? "jpg" : (type.split("/")[1] ?? "png").replace(/[^a-z]/g, "");
  const p = (n: number) => String(n).padStart(2, "0");
  const stamp = `${now.getUTCFullYear()}${p(now.getUTCMonth() + 1)}${p(now.getUTCDate())}-${p(now.getUTCHours())}${p(now.getUTCMinutes())}${p(now.getUTCSeconds())}`;
  return `pasted-${stamp}.${e || "png"}`;
}

/** Why the daemon would refuse this file, or null. */
export function fileProblem(file: { name: string; size: number }, max = MAX_FILE_BYTES): string | null {
  if (!(EXTENSIONS as readonly string[]).includes(ext(file.name))) return `${file.name}: a card takes only ${EXTENSIONS.join(", ")} files`;
  if (file.size === 0) return `${file.name} is empty`;
  if (file.size > max) return `${file.name} is larger than ${Math.round(max / (1024 * 1024))} MiB`;
  return null;
}

/** Does a file of this name (any case) exist on the card? Then an upload adds a version. */
export const existing = (files: readonly CardFile[], name: string): CardFile | undefined => files.find((f) => f.name.toLowerCase() === name.toLowerCase());

/** The files of a paste: each file item, a pasted image named by the time. */
export function pastedFiles(items: ArrayLike<{ kind: string; type: string; getAsFile(): File | null }> | null | undefined, now: Date): { file: File; name: string }[] {
  const out: { file: File; name: string }[] = [];
  for (const it of Array.from(items ?? [])) {
    if (it.kind !== "file") continue;
    const f = it.getAsFile();
    if (!f) continue;
    const generic = !f.name || f.name === "image.png" || f.name === "blob";
    out.push({ file: f, name: generic && f.type.startsWith("image/") ? pastedName(f.type, now) : safeName(f.name) });
  }
  return out;
}

/** Is this event a file change of `card`? */
export const isCardFileEvent = (e: AgentEvent, card: string): boolean => e.kind.startsWith("card.file_") && e.card?.toUpperCase() === card.toUpperCase();

/** One line of a file: the version, the size, who, when. */
export function fileLine(f: { version: number; created_by_name: string }, size: string, when: string): string {
  return `v${f.version} · ${size} · ${f.created_by_name} · ${when}`;
}
