// Pick the API implementation. VITE_MOCK=1 serves fake data in the browser.

import type { AuthStore } from "../stores/auth";
import { createHttpApi, type Api } from "./client";
import { createHttpTransport } from "./http";
import { createMockApi } from "./mock";

export const isMock: boolean = import.meta.env.VITE_MOCK === "1";

export function createApi(auth: AuthStore): Api {
  if (isMock) return createMockApi();
  return createHttpApi(
    createHttpTransport({
      baseUrl: import.meta.env.VITE_API_BASE ?? "",
      getToken: auth.token,
      onUnauthorized: auth.requireToken,
    }),
  );
}

export type { Api } from "./client";
export { ApiError, AuthError } from "./http";
export * from "./types";
