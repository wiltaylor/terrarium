// Where artifact previews load from (docs/security-review.md F5, PLAN §15.2).
//
// Artifacts are untrusted. They must load from the artifact origin
// (`TERRARIUM_ARTIFACT_LISTEN`), never from the dashboard origin. The daemon
// signs a URL with `POST /api/artifacts/{id}/url`. When it answers a bare path
// (`/a/...`), `onArtifactOrigin` puts the path on the configured artifact
// origin (`VITE_ARTIFACT_ORIGIN`). When there is none, the URL stays on the
// dashboard origin and the viewer shows a warning badge.

/** Parse `VITE_ARTIFACT_ORIGIN`: an origin such as `https://art.example.com:8081`, or nothing. */
export function parseOrigin(raw: string | undefined | null): string | undefined {
  const v = (raw ?? "").trim();
  if (!v) return undefined;
  try {
    const u = new URL(v);
    if (u.protocol !== "http:" && u.protocol !== "https:") return undefined;
    return u.origin;
  } catch {
    return undefined;
  }
}

const HAS_SCHEME = /^[a-z][a-z0-9+.-]*:/i;

/** Put a signed artifact path on the artifact origin. Absolute URLs stay as the daemon sent them. */
export function onArtifactOrigin(url: string, artifactOrigin: string | undefined): string {
  if (!artifactOrigin || HAS_SCHEME.test(url) || url.startsWith("//")) return url;
  return new URL(url, artifactOrigin).toString();
}

export type ArtifactOriginKind =
  /** A separate origin: the safe setup. */
  | "separate"
  /** The dashboard origin: a misconfiguration. */
  | "same"
  /** An opaque URL (`data:`, `blob:`), as the mock hands out. */
  | "opaque";

/** Classify a signed URL against the page origin. */
export function artifactOriginKind(url: string, pageOrigin: string): ArtifactOriginKind {
  if (/^(?:data|blob):/i.test(url)) return "opaque";
  try {
    return new URL(url, pageOrigin).origin === pageOrigin ? "same" : "separate";
  } catch {
    return "same";
  }
}

export const SAME_ORIGIN_WARNING = "artifacts share the dashboard origin — set TERRARIUM_ARTIFACT_LISTEN";
