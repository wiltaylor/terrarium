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

/** The encoded base URL of a project route parameter. */
export const projectBase = (project: string): string => `/p/${encodeURIComponent(decodePathParam(project))}`;
