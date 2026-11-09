/** Decode a route parameter without letting malformed percent-encoding break rendering. */
export function decodePathParam(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

/** Decode a URL path for comparisons, falling back to its encoded form. */
export function decodePath(value: string): string {
  try {
    return decodeURI(value);
  } catch {
    return value;
  }
}

/** Make a malformed browser path safe for the router, preserving its raw text. */
export function escapeMalformedPath(value: string): string {
  try {
    decodeURI(value);
    return value;
  } catch {
    return value.replaceAll("%", "%25");
  }
}

/** The encoded base URL of a project route parameter. */
export const projectBase = (project: string): string => `/p/${encodeURIComponent(decodePathParam(project))}`;
