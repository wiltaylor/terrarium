// The HTTP verb of every `Api` method, read from the real client.
//
// It runs each method of `createHttpApi` once against a transport that only
// records the verb. The mock uses the table to know which calls mutate (and
// so need `X-CSRF-Token` with a cookie session), and the tests use it to
// check the CSRF header on every mutating method. A new method in the
// client is in the table at once.

import { createHttpApi, type Api } from "../client";
import type { Transport } from "../http";
import { AuthSocket, type RawSocket } from "../sockets";

/** `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, or `WS` for a socket. */
export type Verb = string;

/** Methods that do not go through the transport: `artifacts.text` fetches a signed URL with no credentials. */
export const NOT_TRANSPORT = new Set(["artifacts.text"]);

const rawStub = (): RawSocket => ({
  readyState: 0,
  protocol: "",
  binaryType: "blob",
  send() {},
  close() {},
  addEventListener() {},
  removeEventListener() {},
});

/** Call every method of `api` with placeholder arguments. `name` is `namespace.method`. */
export function eachMethod(api: object, fn: (name: string, call: () => unknown) => void, prefix = ""): void {
  for (const [key, value] of Object.entries(api)) {
    const name = `${prefix}${key}`;
    if (typeof value === "function") fn(name, () => (value as (...a: unknown[]) => unknown)("x", "x", "x"));
    else if (typeof value === "object" && value !== null && !Array.isArray(value)) eachMethod(value, fn, `${name}.`);
  }
}

/** The verb of each `Api` method, from the HTTP client. */
export function httpVerbs(): Map<string, Verb> {
  let last: Verb | null = null;
  const pending = () => new Promise<never>(() => {});
  const t: Transport = {
    request: (method) => {
      last = method.toUpperCase();
      return pending();
    },
    page: () => {
      last = "GET";
      return pending();
    },
    wsUrl: (path) => `ws://verbs${path}`,
    openSocket: (url) => {
      last = "WS";
      return new AuthSocket(url, { token: null, make: rawStub });
    },
  };
  const verbs = new Map<string, Verb>();
  eachMethod(createHttpApi(t) satisfies Api, (name, call) => {
    if (NOT_TRANSPORT.has(name)) return;
    last = null;
    const out = call();
    if (out && typeof (out as { close?: unknown }).close === "function") (out as { close(): void }).close();
    if (last) verbs.set(name, last);
  });
  return verbs;
}
