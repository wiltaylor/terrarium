/// <reference types="vitest/config" />
import { defineConfig, loadEnv, type Plugin } from "vite";
import solid from "vite-plugin-solid";

// The daemon serves the API and the artifact origin. In dev, Vite proxies to it.
const daemon = process.env.TERRARIUM_DAEMON_URL ?? "http://localhost:8080";

/** An http(s) origin, or undefined. */
function origin(raw: string | undefined): string | undefined {
  if (!raw?.trim()) return undefined;
  try {
    const u = new URL(raw.trim());
    return u.protocol === "http:" || u.protocol === "https:" ? u.origin : undefined;
  } catch {
    return undefined;
  }
}

export interface CspInputs {
  /** `VITE_ARTIFACT_ORIGIN`: previews load from here. */
  artifactOrigin?: string;
  /** `VITE_API_BASE`: the daemon, when it is not the page origin. */
  apiBase?: string;
  /** `VITE_MOCK=1`: the mock hands out `data:` URLs for artifacts. */
  mock?: boolean;
}

/**
 * The dashboard's Content-Security-Policy (docs/security-review.md F3, F5).
 * A meta tag cannot set `frame-ancestors`, `report-uri`, or `sandbox`; the
 * daemon should also send this policy as a header (src/README.md).
 */
export function dashboardCsp(i: CspInputs): string {
  const art = i.artifactOrigin ? [i.artifactOrigin] : [];
  const api = i.apiBase ? [i.apiBase, i.apiBase.replace(/^http/, "ws")] : [];
  const directives: Record<string, string[]> = {
    "default-src": ["'none'"],
    // No inline script and no eval.
    "script-src": ["'self'"],
    // Solid templates carry static style attributes, and xterm.js adds a <style>.
    // The sanitizer drops every style attribute from untrusted HTML.
    "style-src": ["'self'", "'unsafe-inline'"],
    "img-src": ["'self'", "data:", ...art],
    "font-src": ["'self'"],
    // 'self' covers ws:// and wss:// to the page's own host and port (CSP Level 3).
    "connect-src": ["'self'", ...art, ...api],
    // Without an artifact origin, today's daemon signs `/a/...` paths on the
    // dashboard origin; the viewer shows a warning badge for that.
    "frame-src": art.length ? [...art] : ["'self'"],
    "worker-src": ["'none'"],
    "manifest-src": ["'self'"],
    "media-src": ["'none'"],
    "object-src": ["'none'"],
    "base-uri": ["'none'"],
    "form-action": ["'none'"],
  };
  if (i.mock) directives["frame-src"]!.push("data:");
  return Object.entries(directives)
    .map(([k, v]) => `${k} ${v.join(" ")}`)
    .join("; ");
}

function cspPlugin(env: Record<string, string>): Plugin {
  const csp = dashboardCsp({
    artifactOrigin: origin(env.VITE_ARTIFACT_ORIGIN),
    apiBase: origin(env.VITE_API_BASE),
    mock: env.VITE_MOCK === "1",
  });
  return {
    name: "terrarium-csp",
    transformIndexHtml: {
      order: "pre",
      handler: (html) => html.replace("%TERRARIUM_CSP%", csp.replace(/"/g, "&quot;")),
    },
  };
}

export default defineConfig(({ mode }) => {
  const env = { ...loadEnv(mode, process.cwd(), "VITE_"), ...process.env } as Record<string, string>;
  return {
    plugins: [solid(), cspPlugin(env)],
    server: {
      port: 5173,
      proxy: {
        "^/api(/|$)": { target: daemon, changeOrigin: true, ws: true },
        // Regex key: a bare "/a" prefix would also catch /agents and /audit.
        "^/a/": { target: daemon, changeOrigin: true },
      },
    },
    build: {
      outDir: "dist",
      emptyOutDir: true,
      sourcemap: false,
      chunkSizeWarningLimit: 800,
    },
    test: {
      environment: "node",
      include: ["src/**/*.test.ts"],
    },
  };
});
