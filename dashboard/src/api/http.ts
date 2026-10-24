// HTTP transport: bearer token, JSON bodies, query strings, typed errors.

export type Query = Record<string, string | number | boolean | undefined | null>;

export interface RequestOptions {
  query?: Query;
  body?: unknown;
  /** Multipart upload. Sent as-is, without a JSON content type. */
  form?: FormData;
}

export interface Transport {
  request<T>(method: string, path: string, opts?: RequestOptions): Promise<T>;
  /** Absolute ws:// or wss:// URL for a socket path, with the token as a query parameter. */
  wsUrl(path: string, query?: Query): string;
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly body?: unknown,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

/** The daemon refused the bearer token (HTTP 401). */
export class AuthError extends ApiError {
  constructor(body?: unknown) {
    super(401, "unauthorized", body);
    this.name = "AuthError";
  }
}

export interface HttpTransportOptions {
  /** Origin of the daemon. Empty means the page origin. */
  baseUrl?: string;
  fetch?: typeof fetch;
  getToken: () => string | null;
  onUnauthorized?: () => void;
  /** Page location, for the ws URL. Defaults to `window.location`. */
  location?: { protocol: string; host: string };
}

export function buildQuery(query?: Query): string {
  if (!query) return "";
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null || value === "") continue;
    params.set(key, String(value));
  }
  const s = params.toString();
  return s ? `?${s}` : "";
}

/** Encode one path segment. Wiki paths keep their slashes through `encodePath`. */
export const seg = (s: string): string => encodeURIComponent(s);

export function createHttpTransport(opts: HttpTransportOptions): Transport {
  const base = (opts.baseUrl ?? "").replace(/\/$/, "");
  const doFetch = opts.fetch ?? ((...args: Parameters<typeof fetch>) => fetch(...args));

  return {
    async request<T>(method: string, path: string, ro: RequestOptions = {}): Promise<T> {
      const headers: Record<string, string> = { Accept: "application/json" };
      const token = opts.getToken();
      if (token) headers.Authorization = `Bearer ${token}`;
      let body: BodyInit | undefined;
      if (ro.form) {
        body = ro.form;
      } else if (ro.body !== undefined) {
        headers["Content-Type"] = "application/json";
        body = JSON.stringify(ro.body);
      }
      const res = await doFetch(`${base}${path}${buildQuery(ro.query)}`, { method, headers, body });
      const text = await res.text();
      let data: unknown = undefined;
      if (text) {
        try {
          data = JSON.parse(text);
        } catch {
          data = text;
        }
      }
      if (res.status === 401) {
        opts.onUnauthorized?.();
        throw new AuthError(data);
      }
      if (!res.ok) {
        const message =
          (typeof data === "object" && data && "error" in data && String((data as { error: unknown }).error)) ||
          (typeof data === "string" && data) ||
          `${method} ${path} failed with ${res.status}`;
        throw new ApiError(res.status, message, data);
      }
      return data as T;
    },

    wsUrl(path: string, query: Query = {}): string {
      const token = opts.getToken();
      const q = buildQuery({ ...query, token });
      if (base) return `${base.replace(/^http/, "ws")}${path}${q}`;
      const loc = opts.location ?? window.location;
      const proto = loc.protocol === "https:" ? "wss:" : "ws:";
      return `${proto}//${loc.host}${path}${q}`;
    },
  };
}
