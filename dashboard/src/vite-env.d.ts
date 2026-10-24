/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_MOCK?: string;
  readonly VITE_API_BASE?: string;
  /** The artifact origin, for example `https://artifacts.example.com`. Also goes into the CSP. */
  readonly VITE_ARTIFACT_ORIGIN?: string;
  /** Dev only. `1`: a socket may retry with `?token=` when the subprotocol is refused. */
  readonly VITE_ALLOW_QUERY_TOKEN?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
