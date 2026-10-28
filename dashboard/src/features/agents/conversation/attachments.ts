// The images of the composer, with no UI (docs/api.md §6.5, attachments).
//
// The operator pastes, drops, or picks images. On send, each image goes to
// `POST /api/agents/{id}/attachments` first; the daemon puts it in the
// agent's workspace and answers the path that the agent sees. The turn
// then names each path on a line of its own, so the model reads the file
// with its own tool (Claude Code `Read`, Codex `view_image`, pi `read`,
// Copilot). When an upload or the send fails, the draft and the images
// stay, and an image that is in the workspace already is not sent again.

/** The image types the daemon takes. */
export const IMAGE_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"] as const;

/** The largest image: 20 MiB, as `MAX_ATTACHMENT_BYTES` in the daemon. */
export const MAX_IMAGE_BYTES = 20 * 1024 * 1024;

/** One image of the draft. */
export interface DraftImage {
  id: string;
  file: File;
  /** A small `data:` thumbnail ([`thumbnailOf`]), once it is made. */
  thumb?: string;
  state: "ready" | "uploading" | "uploaded" | "failed";
  /** The path in the agent's sandbox, once the upload is done. */
  path?: string;
  error?: string;
}

/**
 * A small PNG thumbnail of `file` as a `data:` URL, or undefined when the
 * browser cannot decode it. The dashboard CSP allows `data:` images and
 * not `blob:` URLs, so the composer does not use object URLs. The decode
 * goes through `createImageBitmap` and needs no URL at all.
 */
export async function thumbnailOf(file: Blob, size = 144): Promise<string | undefined> {
  try {
    const bmp = await createImageBitmap(file);
    const scale = Math.min(1, size / Math.max(bmp.width, bmp.height));
    const c = document.createElement("canvas");
    c.width = Math.max(1, Math.round(bmp.width * scale));
    c.height = Math.max(1, Math.round(bmp.height * scale));
    c.getContext("2d")?.drawImage(bmp, 0, 0, c.width, c.height);
    bmp.close();
    return c.toDataURL("image/png");
  } catch {
    return undefined;
  }
}

/** The part of a clipboard or drop item that this module reads. */
export interface ItemLike {
  kind: string;
  type: string;
  getAsFile(): File | null;
}

/** True when the daemon takes this type (`image/jpg` is JPEG too). */
export const isImageType = (type: string): boolean =>
  (IMAGE_TYPES as readonly string[]).includes(type.toLowerCase()) || type.toLowerCase() === "image/jpg";

/** The image files of a paste or a drop (`DataTransfer.items`). Text and other files stay out. */
export function imagesFromItems(items: Iterable<ItemLike> | ArrayLike<ItemLike> | null | undefined): File[] {
  if (!items) return [];
  const out: File[] = [];
  for (const it of Array.from(items as ArrayLike<ItemLike>)) {
    if (it.kind !== "file" || !it.type.toLowerCase().startsWith("image/")) continue;
    const f = it.getAsFile();
    if (f) out.push(f);
  }
  return out;
}

/** The image files of a file input or of `DataTransfer.files`. */
export function imagesFromFiles(files: Iterable<File> | ArrayLike<File> | null | undefined): File[] {
  if (!files) return [];
  return Array.from(files as ArrayLike<File>).filter((f) => f.type.toLowerCase().startsWith("image/"));
}

/** Why the daemon would refuse `file`, or null. */
export function imageProblem(file: { type: string; size: number }): string | null {
  if (!isImageType(file.type)) return `${file.type || "this file"} is not a PNG, JPEG, GIF, or WebP image`;
  if (file.size > MAX_IMAGE_BYTES) return "the image is over 20 MiB";
  if (file.size === 0) return "the image is empty";
  return null;
}

/**
 * The line that names one image in the turn. Every harness model reads it as
 * "open this file": the path is absolute, and the line says which tool to use.
 * It does not start with `[`, so it never reads as a turn tag (PLAN §8.5).
 */
export const imageLine = (path: string): string =>
  `Attached image: ${path} (open this file with your image or file-read tool to see it)`;

/** The turn text: the operator's text, then one line for each image. */
export function turnText(text: string, paths: string[]): string {
  const body = text.trim();
  const lines = paths.map(imageLine);
  if (!lines.length) return body;
  return body ? `${body}\n\n${lines.join("\n")}` : lines.join("\n");
}

/** True when the draft has something to send. */
export const canSend = (text: string, images: readonly DraftImage[]): boolean =>
  text.trim().length > 0 || images.length > 0;

export interface SubmitDeps {
  attach(file: File): Promise<{ path: string }>;
  send(text: string): Promise<unknown>;
  /** A change of one image: its state, path, or error. */
  onImage?(id: string, patch: Partial<DraftImage>): void;
}

export type SubmitResult = { ok: true; text: string } | { ok: false; error: unknown };

/**
 * Upload the images that are not in the workspace yet, one after the other,
 * then send the turn. It never changes the draft: the caller clears the text
 * and the images on `ok` only. The first failed upload stops the send.
 */
export async function submitTurn(text: string, images: readonly DraftImage[], deps: SubmitDeps): Promise<SubmitResult> {
  if (!canSend(text, images)) return { ok: false, error: new Error("type a turn or attach an image") };
  const paths: string[] = [];
  for (const img of images) {
    if (img.path) {
      paths.push(img.path);
      continue;
    }
    const problem = imageProblem(img.file);
    if (problem) {
      deps.onImage?.(img.id, { state: "failed", error: problem });
      return { ok: false, error: new Error(`${img.file.name || "image"}: ${problem}`) };
    }
    deps.onImage?.(img.id, { state: "uploading", error: undefined });
    try {
      const a = await deps.attach(img.file);
      deps.onImage?.(img.id, { state: "uploaded", path: a.path });
      paths.push(a.path);
    } catch (e) {
      deps.onImage?.(img.id, { state: "failed", error: e instanceof Error ? e.message : String(e) });
      return { ok: false, error: e };
    }
  }
  const out = turnText(text, paths);
  try {
    await deps.send(out);
  } catch (e) {
    return { ok: false, error: e };
  }
  return { ok: true, text: out };
}
