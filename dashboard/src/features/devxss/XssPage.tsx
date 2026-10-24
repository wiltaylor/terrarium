// Mock only (`/dev/xss`): every hostile payload from lib/xss-payloads, through
// the renderer and DOMPurify, and through DOMPurify alone. Open it to eyeball
// that nothing runs. The badge reads `window.__xss` (each payload sets it)
// and counts CSP violations on the page.

import { createSignal, For, onCleanup, onMount } from "solid-js";
import { Markdown } from "../../components/Markdown";
import { SafeHtml } from "../../components/SafeHtml";
import { PageHead } from "../../components/ui";
import { renderWiki } from "../../lib/markdown";
import { XSS_DOCUMENT, XSS_PAYLOADS } from "../../lib/xss-payloads";

export default function XssPage() {
  const [ran, setRan] = createSignal<boolean | null>(null);
  const [violations, setViolations] = createSignal<string[]>([]);
  onMount(() => {
    const onViolation = (e: SecurityPolicyViolationEvent) => setViolations((v) => [...v, `${e.effectiveDirective} ${e.blockedURI || "(inline)"}`]);
    document.addEventListener("securitypolicyviolation", onViolation);
    const t = setTimeout(() => setRan((window as unknown as { __xss?: unknown }).__xss !== undefined), 1000);
    onCleanup(() => {
      clearTimeout(t);
      document.removeEventListener("securitypolicyviolation", onViolation);
    });
  });
  return (
    <>
      <PageHead title="XSS payloads (mock)">
        <span class={`chip ${ran() === null ? "info" : ran() ? "bad" : "ok"}`} data-testid="xss-status">
          {ran() === null ? "checking…" : ran() ? "a payload ran" : "inert: no payload ran"}
        </span>
        <span class={`chip ${violations().length ? "warn" : "ok"}`} title={violations().join("\n")} data-testid="csp-violations">
          CSP violations: {violations().length}
        </span>
      </PageHead>
      <p class="muted small">
        {XSS_PAYLOADS.length} payloads. The top half goes through <code>renderWiki</code> and DOMPurify, as wiki pages do. The bottom half goes through
        DOMPurify alone, as if the renderer had a bug. See also the wiki page <code>security/xss-payloads</code>.
      </p>
      <section class="card-box">
        <h2>Renderer + DOMPurify</h2>
        <Markdown html={renderWiki(XSS_DOCUMENT, { project: "terrarium" })} />
      </section>
      <section class="card-box stack" style={{ "margin-top": "16px" }}>
        <h2>DOMPurify alone</h2>
        <For each={XSS_PAYLOADS}>
          {(p) => (
            <div>
              <strong class="small">{p.name}</strong>
              <pre class="small">{p.src}</pre>
              <SafeHtml class="prose" html={p.src} />
            </div>
          )}
        </For>
      </section>
    </>
  );
}
