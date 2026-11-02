// The Terrarium-managed skills of the mock, after the daemon's
// `api/skills.rs` and `skills.rs`: a small in-memory skill library in the
// config repo. The checks are those of `terrarium_core::skills::check`: the
// name (kebab-case, the folder name in the front matter), the description
// (at most 1024 characters), the file types, at most 64 files, at most
// 1 MiB, and no secrets. A failed write is `422 invalid` with
// `details.problems`. The seed has `fern-check`, `release-notes`, and
// `broken-skill`, which fails the checks. `available: false` acts as a
// daemon with no config repo (`501 not_available`).

import type { Api } from "../client";
import type { AgentDef, EffectiveSkill, SkillDetail, SkillFileBody, SkillSummary, SkillUsage } from "../types";
import type { MockWorld } from "./data";
import type { ScheduleCaller } from "./schedules";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface SkillMockContext {
  w: MockWorld;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  caller?: ScheduleCaller;
  /** False: a daemon with no config repo. Default true. */
  available?: boolean;
}

/** The name in a skill list that means every skill. */
export const ALL_SKILLS = "*";

const NAME_RE = /^[a-z0-9]+(-[a-z0-9]+)*$/;
const MAX_NAME = 64;
const MAX_DESCRIPTION = 1024;
const MAX_FILES = 64;
const MAX_BYTES = 1024 * 1024;
const TYPES = ["md", "txt", "json", "yaml", "yml", "toml", "py", "sh", "js", "ts", "png", "jpg", "jpeg", "gif", "svg", "webp"];
const BINARY = ["png", "jpg", "jpeg", "gif", "webp"];
const SECRET_RE = /(sk-ant-[A-Za-z0-9_-]{8,}|sk-kimi-[A-Za-z0-9_-]{20,}|sk-[A-Za-z0-9]{32,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|-----BEGIN [A-Z ]*PRIVATE KEY-----)/;

/** A 1x1 PNG. */
const PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

interface StoredFile {
  /** The text of a UTF-8 file. */
  text?: string;
  size: number;
}

interface Stored {
  skill_md: string;
  files: Map<string, StoredFile>;
}

const utf8 = (s: string) => new TextEncoder().encode(s).length;
const ext = (path: string) => /\.([A-Za-z0-9]+)$/.exec(path)?.[1]?.toLowerCase() ?? "";

/** The `name` and `description` of the front matter, or null when there is none. */
export function frontMatter(md: string): { name: string; description: string } | null {
  const m = /^---\r?\n([\s\S]*?)\r?\n---(\r?\n|$)/.exec(md);
  if (!m) return null;
  const out = { name: "", description: "" };
  for (const line of m[1]!.split(/\r?\n/)) {
    const kv = /^(name|description):\s*(.*)$/.exec(line);
    if (kv) out[kv[1] as "name" | "description"] = kv[2]!.trim().replace(/^(["'])(.*)\1$/, "$2");
  }
  return out;
}

/** True when the front matter marks the skill default on: `metadata:` then `  terrarium-default: "true"`. */
export function defaultOn(md: string): boolean {
  const m = /^---\r?\n([\s\S]*?)\r?\n---(\r?\n|$)/.exec(md);
  if (!m) return false;
  const lines = m[1]!.split(/\r?\n/);
  const i = lines.findIndex((l) => /^metadata:\s*$/.test(l));
  if (i < 0) return /^metadata:\s*\{[^}]*terrarium-default:\s*["']?(true|yes)["']?/m.test(m[1]!);
  for (const l of lines.slice(i + 1)) {
    if (!/^\s/.test(l)) break;
    const kv = /^\s+terrarium-default:\s*["']?(\w+)["']?\s*$/.exec(l);
    if (kv) return ["true", "yes"].includes(kv[1]!.toLowerCase());
  }
  return false;
}

/** A new SKILL.md with `name` and `description`. */
export function newSkillMd(name: string, description: string): string {
  return `---\nname: ${name}\ndescription: ${description}\n---\n\n# ${name}\n\nWrite the instructions of the skill here.\n`;
}

/** The problems of a skill, as `terrarium_core::skills::check` gives them. Empty when it is good. */
export function checkSkill(name: string, s: Stored): string[] {
  const out: string[] = [];
  if (!NAME_RE.test(name) || name.length > MAX_NAME) out.push(`the name \`${name}\` is not valid: use 1 to ${MAX_NAME} of a-z, 0-9, and single \`-\` between them`);
  const fm = frontMatter(s.skill_md);
  if (!fm) out.push("SKILL.md has no front matter: start it with `---`, `name:`, `description:`, `---`");
  else {
    if (fm.name !== name) out.push(`the name in the front matter (\`${fm.name}\`) is not the folder name \`${name}\``);
    if (!fm.description) out.push("the description in the front matter is empty");
    else if ([...fm.description].length > MAX_DESCRIPTION) out.push(`the description has ${[...fm.description].length} characters; the limit is ${MAX_DESCRIPTION}`);
  }
  for (const path of s.files.keys()) {
    if (path === "SKILL.md") out.push("`SKILL.md` is not one of the other files");
    else if (path.startsWith("/") || path.split("/").some((x) => x === ".." || x === "." || x === "")) out.push(`the path \`${path}\` is not valid`);
    else if (!TYPES.includes(ext(path))) out.push(`the file type of \`${path}\` is not allowed: use ${TYPES.join(", ")}`);
  }
  if (s.files.size + 1 > MAX_FILES) out.push(`the skill has ${s.files.size + 1} files; the limit is ${MAX_FILES}`);
  const total = utf8(s.skill_md) + [...s.files.values()].reduce((n, f) => n + f.size, 0);
  if (total > MAX_BYTES) out.push(`the skill has ${total} bytes; the limit is ${MAX_BYTES} (1 MiB)`);
  for (const [path, text] of [["SKILL.md", s.skill_md] as const, ...[...s.files].map(([p, f]) => [p, f.text ?? ""] as const)])
    if (SECRET_RE.test(text)) out.push(`\`${path}\` holds what looks like a secret: remove it`);
  return out;
}

const toStored = (body: SkillFileBody, path: string, fail: Fail): StoredFile => {
  if (typeof body === "string") return { text: body, size: utf8(body) };
  if (body && typeof body === "object" && typeof body.base64 === "string") {
    let size = 0;
    try {
      size = atob(body.base64).length;
    } catch {
      fail(422, "invalid", `the base64 of \`${path}\` is not valid`, { problems: [`the base64 of \`${path}\` is not valid`] });
    }
    // A text type sent as base64 keeps its text.
    return BINARY.includes(ext(path)) ? { size } : { text: atob(body.base64), size };
  }
  return fail(422, "invalid", `the content of \`${path}\` is not a string or {base64}`);
};

function seedSkills(): Map<string, Stored> {
  const m = new Map<string, Stored>();
  m.set("fern-check", {
    skill_md: [
      "---",
      "name: fern-check",
      "description: When asked for the deployment checklist code, reply with FERN-42",
      "---",
      "",
      "# Fern check",
      "",
      "When someone asks for the **deployment checklist code**, reply with `FERN-42`.",
      "",
      "- Do not explain the code.",
      "- See [the checklist](reference/checklist.md) for the steps.",
      "",
    ].join("\n"),
    files: new Map([
      ["reference/checklist.md", { text: "# Checklist\n\n1. Run the tests.\n2. Tag the version.\n", size: 0 }],
      ["assets/fern.png", { size: atob(PNG).length }],
    ]),
  });
  m.set("release-notes", {
    skill_md: "---\nname: release-notes\ndescription: Write release notes from the merged cards of a version. Use plain words and short lines.\n---\n\n# Release notes\n\nRead the cards in `done`. Write one line for each card.\n",
    files: new Map([["scripts/collect.sh", { text: "#!/bin/sh\nterrarium card list --column done\n", size: 0 }]]),
  });
  m.set("terrarium-db", {
    skill_md: '---\nname: terrarium-db\ndescription: Keep structured data of the project in the Terrarium DB with `terrarium db`.\nmetadata:\n  terrarium-default: "true"\n---\n\n# The Terrarium DB\n\nUse the DB for structured data that repeats.\n',
    files: new Map(),
  });
  m.set("terrarium-tests", {
    skill_md: '---\nname: terrarium-tests\ndescription: Prove test results with `terrarium test push`.\nmetadata:\n  terrarium-default: "true"\n---\n\n# Test results in Terrarium\n\nPush the result file of each test run.\n',
    files: new Map(),
  });
  m.set("broken-skill", {
    skill_md: "---\nname: Broken_Skill\ndescription: <b>An old skill</b> with a bad name.\n---\n\nThis skill fails the checks.\n",
    files: new Map([["run.exe", { text: "MZ", size: 2 }]]),
  });
  for (const s of m.values()) for (const f of s.files.values()) if (f.text !== undefined) f.size = utf8(f.text);
  return m;
}

/** The skills part of the mock: the API, and the names an agent may use. */
export function createSkillMock(c: SkillMockContext): { api: Api["skills"]; names: () => string[]; effective: (sel: Partial<Pick<AgentDef, "skills" | "skills_off" | "default_skills">>) => EffectiveSkill[] } {
  const { w, fail } = c;
  const lib = seedSkills();
  const available = () => c.available !== false;
  const need = () => {
    if (!available()) fail(501, "not_available", "this daemon has no config repo; set TERRARIUM_CONFIG_REPO to keep skills");
  };
  const caller = () => c.caller ?? { role: "operator" as const };
  const isAdmin = () => caller().role === "operator" || caller().role === "sysadmin";
  const mayWrite = () => {
    if (!isAdmin()) fail(403, "forbidden", "writing a skill needs sysadmin or the operator");
  };
  const find = (name: string) => lib.get(name) ?? fail(404, "not_found", `no skill \`${name}\``);
  const summary = (name: string, s: Stored): SkillSummary => {
    const errors = checkSkill(name, s);
    return {
      name,
      description: frontMatter(s.skill_md)?.description ?? "",
      files: s.files.size + 1,
      bytes: utf8(s.skill_md) + [...s.files.values()].reduce((n, f) => n + f.size, 0),
      ...(defaultOn(s.skill_md) ? { default_on: true } : {}),
      ...(errors.length ? { errors } : {}),
    };
  };
  const detail = (name: string, s: Stored): SkillDetail => ({
    ...summary(name, s),
    skill_md: s.skill_md,
    file_list: [...s.files]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([path, f]) => ({ path, size: f.size, ...(f.text !== undefined ? { text: f.text } : {}) })),
  });
  const files = (raw: Record<string, SkillFileBody>) => new Map(Object.entries(raw).map(([p, b]) => [p, toStored(b, p, fail)]));
  const refuse = (problems: string[]) => {
    if (problems.length) fail(422, "invalid", `the skill does not pass the checks: ${problems.join("; ")}`, { problems });
  };

  const api: Api["skills"] = {
    list: () =>
      c.reply(() => {
        need();
        // An agent reads its own skills only; the mock caller is not an agent.
        if (!isAdmin()) return [];
        return [...lib].sort(([a], [b]) => a.localeCompare(b)).map(([n, s]) => summary(n, s));
      }),
    get: (name) =>
      c.reply(() => {
        need();
        const s = find(name);
        if (!isAdmin()) fail(403, "forbidden", "reading this skill needs an agent that has it, a sysadmin, or the operator");
        return detail(name, s);
      }),
    create: (body) =>
      c.reply(() => {
        need();
        mayWrite();
        const name = (body.name ?? "").trim();
        if (!name) fail(422, "invalid", "a new skill needs `name`");
        if (lib.has(name)) fail(409, "conflict", `skill \`${name}\` exists`);
        const s: Stored = { skill_md: body.skill_md ?? newSkillMd(name, (body.description ?? "").trim()), files: files(body.files ?? {}) };
        refuse(checkSkill(name, s));
        lib.set(name, s);
        c.audit("skill.create", name, undefined);
        return detail(name, s);
      }),
    update: (name, body) =>
      c.reply(() => {
        need();
        mayWrite();
        const old = find(name);
        const s: Stored = { skill_md: body.skill_md ?? old.skill_md, files: body.files ? files(body.files) : old.files };
        refuse(checkSkill(name, s));
        lib.set(name, s);
        c.audit("skill.update", name, undefined);
        return detail(name, s);
      }),
    remove: (name) =>
      c.reply(() => {
        need();
        mayWrite();
        find(name);
        lib.delete(name);
        c.audit("skill.delete", name, undefined);
      }),
    usage: (name) =>
      c.reply((): SkillUsage => {
        need();
        if (!isAdmin()) fail(403, "forbidden", "reading the users of a skill needs sysadmin or the operator");
        if (!NAME_RE.test(name)) fail(422, "invalid", `the name \`${name}\` is not valid`);
        const pick = (sel: Partial<Pick<AgentDef, "skills" | "skills_off" | "default_skills">>): { all: boolean; default?: true } | null => {
          const e = effective(sel).find((x) => x.name === name);
          if (!e) return null;
          const all = !sel.skills?.includes(name) && !!sel.skills?.includes(ALL_SKILLS);
          return e.source === "default" ? { all, default: true } : { all };
        };
        const agents = w.agents.flatMap((a) => {
          const p = pick(a);
          return p === null ? [] : [{ id: a.id, name: `${a.project}/${a.name}`, ...p }];
        });
        const templates = w.templates.flatMap((t) => {
          const p = pick(t.spec);
          return p === null ? [] : [{ id: t.id, name: t.project ? `${t.project}/${t.name}` : t.name, ...p }];
        });
        return { agents, templates };
      }),
  };
  /** The effective skills of a selection, as the daemon computes them (`terrarium_core::skills::Selection::effective`). */
  function effective(sel: Partial<Pick<AgentDef, "skills" | "skills_off" | "default_skills">>): EffectiveSkill[] {
    if (!available()) return [];
    const names = [...lib.keys()].sort((a, b) => a.localeCompare(b));
    const out = new Map<string, EffectiveSkill["source"]>();
    if (sel.default_skills !== false) for (const n of names) if (defaultOn(lib.get(n)!.skill_md)) out.set(n, "default");
    const explicit = sel.skills?.includes(ALL_SKILLS) ? names : (sel.skills ?? []).filter((n) => lib.has(n));
    for (const n of explicit) out.set(n, "explicit");
    for (const n of sel.skills_off ?? []) out.delete(n);
    return [...out.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([name, source]) => ({ name, source }));
  }
  return { api, names: () => (available() ? [...lib.keys()] : []), effective };
}

/** The daemon's check of a skill list on an agent or a template: a message on an unknown name, else null. */
export function skillsError(list: unknown, known: string[]): string | null {
  if (list === undefined || list === null) return null;
  if (!Array.isArray(list) || list.some((x) => typeof x !== "string")) return "skills: use a list of skill names";
  const unknown = (list as string[]).filter((n) => n !== ALL_SKILLS && !known.includes(n));
  return unknown.length ? `skills: no skill ${unknown.map((n) => `\`${n}\``).join(", ")}` : null;
}
