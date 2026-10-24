// HTTP transport: bearer token, JSON bodies, query strings, typed errors,
// and cursor pages (docs/api.md §2.4 and §2.5). Sockets carry the token as
// the `bearer.<token>` subprotocol, never in the URL (see ./sockets).

import { AuthSocket, browserSocket, type MakeRawSocket } from "./sockets";

export type Query = Record<string, string | number | boolean | undefined | null>;

export interface RequestOptions {
  query?: Query;
  body?: unknown;
  /** Multipart upload. Sent as-is, without a JSON content type. */
  form?: FormData;
}

/** One page of a list. `next` is the `X-Next-Cursor` header, or null on the last page. */
export interface Page<T> {
  items: T[];
  next: string | null;
}

export interface Transport {
  request<T>(method: string, path: string, opts?: RequestOptions): Promise<T>;
  /** GET a list endpoint: the JSON array and the `X-Next-Cursor` header. */
  page<T>(path: string, query?: Query): Promise<Page<T>>;
  /** Absolute ws:// or wss:// URL for a socket path. It never carries the token. */
  wsUrl(path: string, query?: Query): string;
  /** Open a socket to a `wsUrl` that authenticates with the `bearer.<token>` subprotocol. */
  openSocket(url: string): AuthSocket;
}

/** The daemon's error body: `{"error": {"code", "message", "details"?}}`. */
export interface ErrorBody {
  code: string;
  message: string;
  details?: unknown;
}

export class ApiError extends Error {
  /** The daemon's `error.code`, for example `wip_limit`. `http_<status>` when the body has none. */
  readonly code: string;
  /** The daemon's `error.details`, for example `{blocked_by: [...]}` or a wiki diff. */
  readonly details?: unknown;

  constructor(
    readonly status: number,
    message: string,
    readonly body?: unknown,
    code?: string,
    details?: unknown,
  ) {
    super(message);
    this.name = "ApiError";
    this.code = code ?? `http_${status}`;
    this.details = details;
  }
}

/** The daemon refused the bearer token (HTTP 401). */
export class AuthError extends ApiError {
  constructor(body?: unknown) {
    const e = parseErrorBody(body);
    super(401, e?.message ?? "unauthorized", body, e?.code ?? "unauthorized", e?.details);
    this.name = "AuthError";
  }
}

/** Read `{"error": {...}}`. Also accepts the old `{"error": "text"}`. */
export function parseErrorBody(data: unknown): ErrorBody | null {
  if (typeof data !== "object" || data === null || !("error" in data)) return null;
  const err = (data as { error: unknown }).error;
  if (typeof err === "string") return { code: "error", message: err };
  if (typeof err === "object" && err !== null) {
    const o = err as Record<string, unknown>;
    return {
      code: typeof o.code === "string" ? o.code : "error",
      message: typeof o.message === "string" ? o.message : JSON.stringify(err),
      details: o.details,
    };
  }
  return null;
}

/** Build an ApiError from a response status and its parsed body. */
export function errorFrom(status: number, data: unknown, fallback: string): ApiError {
  const e = parseErrorBody(data);
  if (e) return new ApiError(status, e.message, data, e.code, e.details);
  return new ApiError(status, (typeof data === "string" && data) || fallback, data);
}

/** True when the endpoint does not exist on this daemon yet (`404` or `405`). */
export const isMissingEndpoint = (e: unknown): boolean =>
  e instanceof ApiError &&
  (e.status === 405 || (e.status === 404 && (e.code.startsWith("http_") || /no such API route/i.test(e.message))));

export interface HttpTransportOptions {
  /** Origin of the daemon. Empty means the page origin. */
  baseUrl?: string;
  fetch?: typeof fetch;
  getToken: () => string | null;
  onUnauthorized?: () => void;
  /** Page location, for the ws URL. Defaults to `window.location`. */
  location?: { protocol: string; host: string };
  /** Make the underlying WebSocket. Defaults to the browser's. Tests and the mock pass their own. */
  makeSocket?: MakeRawSocket;
  /** Dev only: retry a refused subprotocol handshake with `?token=` (`VITE_ALLOW_QUERY_TOKEN=1`). */
  allowQueryToken?: boolean;
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

/** Encode one path segment. `project/name` agent keys become `project%2Fname`. */
export const seg = (s: string): string => encodeURIComponent(s);

export const NEXT_CURSOR = "X-Next-Cursor";

export function createHttpTransport(opts: HttpTransportOptions): Transport {
  const base = (opts.baseUrl ?? "").replace(/\/$/, "");
  const doFetch = opts.fetch ?? ((...args: Parameters<typeof fetch>) => fetch(...args));

  const send = async (method: string, path: string, ro: RequestOptions = {}): Promise<{ data: unknown; res: Response }> => {
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
    if (!res.ok) throw errorFrom(res.status, data, `${method} ${path} failed with ${res.status}`);
    return { data, res };
  };

  return {
    async request<T>(method: string, path: string, ro: RequestOptions = {}): Promise<T> {
      return (await send(method, path, ro)).data as T;
    },

    async page<T>(path: string, query?: Query): Promise<Page<T>> {
      const { data, res } = await send("GET", path, { query });
      if (!Array.isArray(data)) throw new ApiError(res.status, `GET ${path}: expected a JSON array`, data, "bad_shape");
      return { items: data as T[], next: res.headers.get(NEXT_CURSOR) || null };
    },

    wsUrl(path: string, query: Query = {}): string {
      const q = buildQuery(query);
      if (base) return `${base.replace(/^http/, "ws")}${path}${q}`;
      const loc = opts.location ?? window.location;
      const proto = loc.protocol === "https:" ? "wss:" : "ws:";
      return `${proto}//${loc.host}${path}${q}`;
    },

    openSocket(url: string): AuthSocket {
      return new AuthSocket(url, {
        token: opts.getToken(),
        make: opts.makeSocket ?? browserSocket,
        allowQueryToken: opts.allowQueryToken ?? false,
        onQueryFallback: (u) => console.warn(`terrarium: ${u} refused the bearer subprotocol; retried with ?token= (VITE_ALLOW_QUERY_TOKEN)`),
      });
    },
  };
}
