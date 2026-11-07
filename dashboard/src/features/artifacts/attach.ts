// The "Attach to" field of the artifact upload (docs/api.md §12.2): pure. No Solid imports.

import type { ArtifactLinkKind } from "../../api/types";

/**
 * The link of the "Attach to" text: `page:<path>`, `card:<id>`, or
 * `label:<label>`. A version is a card label since TER-58, so
 * `version:<name>` links the label `version:<name>`. Undefined for an empty
 * or unknown text.
 */
export function attachTarget(text: string): { kind: ArtifactLinkKind; ref: string } | undefined {
  const t = text.trim();
  const m = /^(page|card|label|version):(.+)$/.exec(t);
  if (!m) return undefined;
  const ref = m[2]!.trim();
  if (!ref) return undefined;
  if (m[1] === "version") return { kind: "label", ref: t };
  return { kind: m[1] as ArtifactLinkKind, ref };
}
