// The files of a card in the mock (docs/api.md §8.4): the seed, and
// `GET/POST /api/cards/{id}/files`, `GET .../files/{name}?version=`,
// `GET .../files/{name}/versions`, `DELETE .../files/{name}`. The answers
// have the daemon's shapes; the checks are the daemon's (store/card_files.rs):
// the name rules, the extension allowlist, the content check of the images
// and the text, unique names with no regard to case, the 10 MiB and 50 MiB
// caps, versions on a replace, and a soft delete of every version.

import type { AgentEvent, Card, CardFile, CardFileUploaded, CardFileVersion } from "../types";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export const MOCK_MAX_FILE = 10 * 1024 * 1024;
export const MOCK_MAX_CARD = 50 * 1024 * 1024;

/** One stored version: the row and the bytes. */
interface Row extends CardFileVersion {
  card: string;
  bytes: Uint8Array;
  deleted_at?: string;
}

const TYPES: Record<string, string> = {
  md: "text/markdown; charset=utf-8",
  markdown: "text/markdown; charset=utf-8",
  txt: "text/plain; charset=utf-8",
  json: "application/json",
  yaml: "application/yaml",
  yml: "application/yaml",
  csv: "text/csv; charset=utf-8",
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  svg: "image/svg+xml",
  pdf: "application/pdf",
};

const starts = (b: Uint8Array, s: number[] | string) => {
  const want = typeof s === "string" ? Array.from(s, (c) => c.charCodeAt(0)) : s;
  return want.every((x, i) => b[i] === x);
};

/** The daemon's `check_name`: the media type, or a failure. */
export function checkName(name: string, fail: Fail): string {
  const bad = (why: string): never => fail(422, "invalid", `bad file name \`${name}\`: ${why}`);
  if (!name) fail(422, "invalid", "the file name is empty");
  if (name.length > 128) bad("at most 128 bytes");
  if (name.startsWith(".")) bad("a hidden name (a leading `.`) is not allowed");
  if (/^ | $|\.$/.test(name)) bad("no leading or trailing space, and no trailing `.`");
  if (/[^A-Za-z0-9._+ -]/.test(name)) bad("use letters, digits, `.`, `_`, `-`, `+`, and spaces");
  if (name.toLowerCase() === "index.md") bad("`index.md` is the name of the index that the daemon writes in the card dir of an agent");
  const ext = name.includes(".") ? name.slice(name.lastIndexOf(".") + 1).toLowerCase() : "";
  return TYPES[ext] ?? fail(415, "unsupported_media_type", `\`${name}\`: a card takes only these file types: md, txt, json, yaml, yml, csv, png, jpg, jpeg, gif, webp, svg, pdf`);
}

/** The daemon's `sniff`. */
export function sniff(name: string, type: string, b: Uint8Array, fail: Fail): void {
  const wrong = (): never => fail(415, "unsupported_media_type", `\`${name}\` is not the type its extension says (the content does not match the extension)`);
  if (type === "image/png") return void (starts(b, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) || wrong());
  if (type === "image/jpeg") return void (starts(b, [0xff, 0xd8, 0xff]) || wrong());
  if (type === "image/gif") return void (starts(b, "GIF87a") || starts(b, "GIF89a") || wrong());
  if (type === "image/webp") return void ((starts(b, "RIFF") && b.length >= 12 && String.fromCharCode(...b.slice(8, 12)) === "WEBP") || wrong());
  if (type === "application/pdf") return void (starts(b, "%PDF-") || wrong());
  let text: string;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(b);
  } catch {
    return fail(415, "unsupported_media_type", `\`${name}\` is not UTF-8 text`);
  }
  if (text.includes("\0")) fail(415, "unsupported_media_type", `\`${name}\` has a NUL byte`);
  if (type === "application/json") {
    try {
      JSON.parse(text);
    } catch (e) {
      fail(422, "invalid", `\`${name}\` is not valid JSON: ${String(e)}`);
    }
  }
  if (type === "image/svg+xml" && !/^\s*(<\?[\s\S]*?\?>\s*|<!--[\s\S]*?-->\s*|<!doctype[^>]*>\s*)*<svg[\s>]/i.test(text.replace(/^\uFEFF/, ""))) wrong();
}

export interface CardFileMockContext {
  iso: () => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: Omit<AgentEvent, "seq" | "id" | "at" | "data"> & { data?: Record<string, unknown> }) => AgentEvent;
  audit: (action: string, target: string, proj: string | undefined, detail?: Record<string, unknown>) => unknown;
  card: (id: string) => Card;
}

const enc = (s: string) => new TextEncoder().encode(s);

/** A fake SHA-256: 64 hex digits from the bytes (the mock needs no real hash). */
function fakeSha(b: Uint8Array): string {
  let h = 2166136261;
  for (const x of b) h = Math.imul(h ^ x, 16777619) >>> 0;
  return (h.toString(16).padStart(8, "0") + b.length.toString(16).padStart(8, "0")).repeat(4);
}

export function createCardFileMock(ctx: CardFileMockContext) {
  const { iso, reply, fail, emit, audit } = ctx;
  const rows: Row[] = [];
  /** The seed writes no audit rows and emits no events. */
  let quiet = false;
  const key = (n: string) => n.toLowerCase();
  const live = (card: string) => rows.filter((r) => r.card === card && !r.deleted_at);
  const latest = (card: string, name: string) =>
    live(card)
      .filter((r) => key(r.name) === key(name))
      .sort((a, b) => b.version - a.version)[0];
  const view = (r: Row): CardFile => {
    const versions = live(r.card).filter((x) => key(x.name) === key(r.name)).length;
    return { card: r.card, name: r.name, version: r.version, size: r.size, sha256: r.sha256, media_type: r.media_type, created_by: r.created_by, created_by_name: r.created_by_name, created_at: r.created_at, versions };
  };
  const version = (r: Row): CardFileVersion => ({ name: r.name, version: r.version, size: r.size, sha256: r.sha256, media_type: r.media_type, created_by: r.created_by, created_by_name: r.created_by_name, created_at: r.created_at });
  const list = (card: string): CardFile[] => {
    const names = [...new Set(live(card).map((r) => key(r.name)))].sort();
    return names.map((n) => view(latest(card, n)!));
  };
  const event = (c: Card, change: string, summary: string, data: Record<string, unknown>) => {
    c.updated_at = iso();
    emit({ kind: `card.${change}` as AgentEvent["kind"], actor: "operator", project: c.project, card: c.id, summary: `${summary} by operator`, data: { change, title: c.title, column: c.column, ...data } });
  };
  const editable = (id: string): Card => {
    const c = ctx.card(id);
    if (c.archived) fail(409, "conflict", `${c.id} is archived`);
    return c;
  };

  /** Add a file or a new version, as `store::card_files::add`. */
  const add = (id: string, name: string, bytes: Uint8Array, replace: boolean, by = "operator"): CardFileUploaded => {
    const c = editable(id);
    const type = checkName(name, fail);
    if (bytes.length > MOCK_MAX_FILE) fail(413, "too_large", `the file is ${bytes.length} bytes; a card file has at most ${MOCK_MAX_FILE} bytes (TERRARIUM_CARD_FILE_MAX_BYTES)`, { size: bytes.length, max_file_bytes: MOCK_MAX_FILE });
    if (!bytes.length) fail(422, "invalid", `\`${name}\` is empty`);
    sniff(name, type, bytes, fail);
    const sha = fakeSha(bytes);
    const cur = latest(c.id, name);
    if (cur && !replace) fail(409, "conflict", `${c.id} has a file \`${cur.name}\` already; replace it (\`--replace\`, \`?replace=true\`) to add a version`, { name: cur.name, version: cur.version });
    if (cur && cur.sha256 === sha && cur.name === name) return { ...view(cur), outcome: "unchanged" };
    const used = live(c.id).reduce((n, r) => n + r.size, 0);
    if (used + bytes.length > MOCK_MAX_CARD)
      fail(413, "too_large", `${c.id} has ${used} bytes of files (all the versions); ${bytes.length} more is over the limit of ${MOCK_MAX_CARD} bytes (TERRARIUM_CARD_FILES_MAX_BYTES). Remove a file first.`, { used, size: bytes.length, max_card_bytes: MOCK_MAX_CARD });
    const v = Math.max(0, ...rows.filter((r) => r.card === c.id && key(r.name) === key(name)).map((r) => r.version)) + 1;
    const row: Row = { card: c.id, name, version: v, size: bytes.length, sha256: sha, media_type: type, created_by: by, created_by_name: by, created_at: iso(), bytes };
    rows.push(row);
    const detail = { name, version: v, size: bytes.length, sha256: sha, media_type: type };
    if (!quiet) audit(cur ? "card.file_replace" : "card.file_add", c.id, c.project, detail);
    if (!quiet) event(c, cur ? "file_replaced" : "file_added", `${c.id} file ${name} v${v} ${cur ? "replaced" : "added"}`, detail);
    return { ...view(row), outcome: cur ? "replaced" : "added" };
  };

  const found = (card: string, name: string, v?: number): Row => {
    const c = ctx.card(card);
    if (v === undefined) return latest(c.id, name) ?? fail(404, "not_found", `${c.id} has no file \`${name}\``);
    return live(c.id).find((r) => key(r.name) === key(name) && r.version === v) ?? fail(404, "not_found", `${c.id} has no version ${v} of the file \`${name}\``);
  };

  return {
    /** The demo files: a brief with two versions and a diagram on the first card that has an assignee. */
    seed(cards: Card[]) {
      const c = cards.find((x) => x.assignee && !x.archived) ?? cards[0];
      if (!c) return;
      quiet = true;
      add(c.id, "brief.md", enc("# Brief\n\nThe first draft."), false, c.created_by);
      add(c.id, "brief.md", enc(`# Brief\n\nShip the parser behind a flag.\n\n- Read \`grammar.json\`.\n- Keep the old path until ${c.id} is done.\n`), true, "operator");
      add(c.id, "grammar.json", enc(JSON.stringify({ rules: ["expr", "term"], start: "expr" }, null, 2)), false);
      // A 1x1 PNG.
      add(c.id, "sketch.png", Uint8Array.from(atob("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="), (ch) => ch.charCodeAt(0)), false);
      quiet = false;
    },
    api: {
      list: (card: string) => reply(() => list(ctx.card(card).id)),
      upload: (card: string, file: Blob, name: string, replace = false) =>
        file.arrayBuffer().then((buf) => reply(() => add(card, name, new Uint8Array(buf), replace))),
      versions: (card: string, name: string) =>
        reply(() => {
          const c = ctx.card(card);
          const vs = live(c.id)
            .filter((r) => key(r.name) === key(name))
            .sort((a, b) => b.version - a.version)
            .map(version);
          return vs.length ? vs : fail(404, "not_found", `${c.id} has no file \`${name}\``);
        }),
      get: (card: string, name: string, v?: number) =>
        reply(() => {
          const r = found(card, name, v);
          return { blob: new Blob([r.bytes.slice()], { type: r.media_type }), fileName: r.name, version: r.version };
        }),
      remove: (card: string, name: string) =>
        reply(() => {
          const c = editable(card);
          const cur = found(c.id, name);
          const at = iso();
          let n = 0;
          for (const r of live(c.id).filter((x) => key(x.name) === key(name))) {
            r.deleted_at = at;
            n++;
          }
          const detail = { name: cur.name, version: cur.version, versions: n };
          audit("card.file_remove", c.id, c.project, detail);
          event(c, "file_removed", `${c.id} file ${cur.name} removed`, detail);
        }),
    },
  };
}
