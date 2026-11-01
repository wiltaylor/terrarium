// The system prompt selector model: one choice over the two fields of an
// agent or a template (docs/api.md §6.1, §28). `persona` names an entry of
// the system prompt library (or `none`, or empty for "by role"), and
// `system_prompt` is the custom text, which wins over the persona. Pure.

/** The two fields, as the forms keep them: empty strings for "not set". */
export interface PromptFields {
  /** Empty: by role. `none`: the harness's default prompt. Else a library entry. */
  persona: string;
  /** The custom text. Empty: none. */
  system_prompt: string;
}

/** One choice of the selector. */
export type PromptChoice = { kind: "role" } | { kind: "none" } | { kind: "library"; name: string } | { kind: "custom" };

/** The value of the choice in the `<select>`. A library entry is `lib:<name>`. */
export function choiceValue(c: PromptChoice): string {
  return c.kind === "library" ? `lib:${c.name}` : `@${c.kind}`;
}

/** The choice of a `<select>` value. */
export function parseChoice(v: string): PromptChoice {
  if (v.startsWith("lib:")) return { kind: "library", name: v.slice(4) };
  if (v === "@none") return { kind: "none" };
  if (v === "@custom") return { kind: "custom" };
  return { kind: "role" };
}

/**
 * The choice that the fields show. A custom text wins, as at the start.
 * `custom`: the operator picked Custom and has not typed yet.
 */
export function choiceOf(f: PromptFields, custom = false): PromptChoice {
  if (custom || f.system_prompt.trim()) return { kind: "custom" };
  const p = f.persona.trim();
  if (!p) return { kind: "role" };
  if (p === "none") return { kind: "none" };
  return { kind: "library", name: p };
}

/**
 * The fields after a pick. A library entry, by role, and the harness
 * default clear the custom text. Custom keeps the text (or takes `seed`, the
 * text of the entry it starts from, when it has none) and goes back to by
 * role, unless the persona is `none` (then the harness section stays out).
 */
export function applyChoice(f: PromptFields, c: PromptChoice, seed = ""): PromptFields {
  switch (c.kind) {
    case "role":
      return { persona: "", system_prompt: "" };
    case "none":
      return { persona: "none", system_prompt: "" };
    case "library":
      return { persona: c.name, system_prompt: "" };
    case "custom":
      return { persona: f.persona.trim() === "none" ? "none" : "", system_prompt: f.system_prompt.trim() ? f.system_prompt : seed };
  }
}

/** The fields after "Save as new system prompt": select the saved entry, and clear the text. */
export function afterSave(name: string): PromptFields {
  return { persona: name, system_prompt: "" };
}

/** The persona of an agent with no persona: `leader` for a lead role, else `worker`. */
export function byRoleName(roles: readonly string[]): "leader" | "worker" {
  return roles.some((r) => r === "project_admin" || r === "sysadmin") ? "leader" : "worker";
}

/**
 * The warning for a persona that names no library entry, or null. `names`
 * is the library (undefined until it loads: no warning). An older config
 * repo can still have `prompts/<name>.md`, so the text says "may".
 */
export function missingPersonaWarning(persona: string, names: readonly string[] | undefined): string | null {
  const p = persona.trim();
  if (!p || p === "none" || !names) return null;
  if (names.includes(p)) return null;
  return `"${p}" is not in the system prompt library. The agent may not start: add system-prompts/${p}.md, or pick another system prompt.`;
}

/** The daemon's rules for a library name: 1-63 of a-z, 0-9, - and _, starting with a letter or a digit; not `none`. */
export function promptNameProblem(name: string): string | null {
  const n = name.trim();
  if (!n) return "Give a name.";
  if (n === "none") return "none is not a name: it means the harness default.";
  if (!/^[a-z0-9][a-z0-9_-]{0,62}$/.test(n)) return "Name: 1 to 63 of a-z, 0-9, - and _, starting with a letter or a digit.";
  return null;
}

/** One short line for the choice, for the agent page. */
export function choiceLabel(c: PromptChoice, roles: readonly string[]): string {
  switch (c.kind) {
    case "role":
      return `${byRoleName(roles)} (by role)`;
    case "none":
      return "harness default";
    case "library":
      return c.name;
    case "custom":
      return "custom";
  }
}
