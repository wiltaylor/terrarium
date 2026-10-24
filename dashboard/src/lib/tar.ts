// A minimal ustar writer, so the browser can upload a folder as one bundle
// (PLAN §17: "dir as tar for a bundle"). Regular files only. Pure.

export interface TarEntry {
  /** Path inside the archive, with `/` separators. */
  path: string;
  data: Uint8Array;
  /** Seconds since the epoch. */
  mtime?: number;
}

const enc = new TextEncoder();

function writeString(buf: Uint8Array, offset: number, len: number, s: string) {
  const b = enc.encode(s);
  buf.set(b.subarray(0, len), offset);
}

function writeOctal(buf: Uint8Array, offset: number, len: number, n: number) {
  // len - 1 octal digits, then NUL.
  writeString(buf, offset, len - 1, n.toString(8).padStart(len - 1, "0"));
}

/** Split a path into ustar `name` (100 bytes) and `prefix` (155 bytes). */
export function splitPath(path: string): { name: string; prefix: string } {
  const p = path.replace(/^\/+/, "");
  if (enc.encode(p).length <= 100) return { name: p, prefix: "" };
  const parts = p.split("/");
  for (let i = 1; i < parts.length; i++) {
    const prefix = parts.slice(0, i).join("/");
    const name = parts.slice(i).join("/");
    if (enc.encode(prefix).length <= 155 && enc.encode(name).length <= 100) return { name, prefix };
  }
  throw new Error(`path too long for tar: ${path}`);
}

function header(entry: TarEntry): Uint8Array {
  const h = new Uint8Array(512);
  const { name, prefix } = splitPath(entry.path);
  writeString(h, 0, 100, name);
  writeOctal(h, 100, 8, 0o644);
  writeOctal(h, 108, 8, 0);
  writeOctal(h, 116, 8, 0);
  writeOctal(h, 124, 12, entry.data.length);
  writeOctal(h, 136, 12, Math.floor(entry.mtime ?? 0));
  h.fill(0x20, 148, 156); // checksum field counts as spaces
  h[156] = 0x30; // '0': regular file
  writeString(h, 257, 6, "ustar");
  writeString(h, 263, 2, "00");
  writeString(h, 345, 155, prefix);
  let sum = 0;
  for (const b of h) sum += b;
  writeString(h, 148, 8, `${sum.toString(8).padStart(6, "0")}\0 `);
  return h;
}

/** Build a tar archive. Two zero blocks end it. */
export function buildTar(entries: TarEntry[]): Uint8Array {
  const blocks: Uint8Array[] = [];
  let size = 1024;
  for (const e of entries) {
    const h = header(e);
    const padded = Math.ceil(e.data.length / 512) * 512;
    blocks.push(h);
    const body = new Uint8Array(padded);
    body.set(e.data);
    blocks.push(body);
    size += 512 + padded;
  }
  const out = new Uint8Array(size);
  let off = 0;
  for (const b of blocks) {
    out.set(b, off);
    off += b.length;
  }
  return out;
}

/**
 * The paths of a folder pick (`webkitRelativePath`), with the shared top
 * folder removed, so `site/index.html` becomes `index.html`.
 */
export function bundlePaths(paths: string[]): string[] {
  const tops = new Set(paths.map((p) => p.split("/")[0]));
  if (tops.size === 1 && paths.every((p) => p.includes("/"))) return paths.map((p) => p.split("/").slice(1).join("/"));
  return paths;
}
