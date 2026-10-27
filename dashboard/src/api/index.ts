// Pick the API implementation. VITE_MOCK=1 serves fake data in the browser.

import type { AuthStore } from "../stores/auth";
import { parseOrigin } from "../lib/artifact-origin";
import { createHttpApi, type Api } from "./client";
import { createHttpTransport } from "./http";
import { createMockApi } from "./mock";
import { MOCK_PASSWORD, MOCK_TOKEN } from "./mock/ws";

export const isMock: boolean = import.meta.env.VITE_MOCK === "1";

/** The artifact origin the build knows (`VITE_ARTIFACT_ORIGIN`). The CSP allows it too. */
export const artifactOrigin: string | undefined = parseOrigin(import.meta.env.VITE_ARTIFACT_ORIGIN);

/** Dev only: let a socket retry with `?token=` when the daemon refuses the subprotocol. */
export const allowQueryToken: boolean = import.meta.env.VITE_ALLOW_QUERY_TOKEN === "1";

/** Mock only: the token that the mock "daemon" takes. The token screen shows it. */
export const mockToken: string | undefined = isMock ? MOCK_TOKEN : undefined;
/** Mock only: the operator password of the mock "daemon". The login screen shows it. */
export const mockPassword: string | undefined = isMock ? MOCK_PASSWORD : undefined;

// The mock keeps its fake "cookie" in sessionStorage, so a reload keeps the
// session as a browser does. Mock only: the real cookie is `HttpOnly`.
const sessionStore = () => {
  try {
    return typeof sessionStorage === "undefined" ? null : sessionStorage;
  } catch {
    return null;
  }
};

export function createApi(auth: AuthStore): Api {
  if (isMock)
    return createMockApi({
      // The mock checks what the transport would send, as the TCP listener does.
      client: { token: auth.token, csrf: auth.csrf, onUnauthorized: auth.requireToken },
      cookieJar: sessionStore(),
    });
  return createHttpApi(
    createHttpTransport({
      baseUrl: import.meta.env.VITE_API_BASE ?? "",
      getToken: auth.token,
      getCsrf: auth.csrf,
      onUnauthorized: auth.requireToken,
      allowQueryToken,
    }),
    { artifactOrigin },
  );
}

export type { Api } from "./client";
export { ApiError, AuthError } from "./http";
export * from "./types";
