// The webhook part of the schedule dialog (docs/api.md §19.5): the URL (or
// a secret that holds it), the format preset, the body template with a live
// preview for a sample event, the headers (a value or a secret, with the
// CredentialSelect), the signing secret, and the time limit. The state and
// the mapping to the action are in ./form and ./webhook; this file only
// draws them.

import { createMemo, For, Index, Show } from "solid-js";
import type { WebhookFormat, WebhookMethod } from "../../api/types";
import { CredentialSelect } from "../secrets/CredentialSelect";
import { GLOBAL, type ScheduleDialogState } from "./form";
import type { FieldKit } from "./fields";
import { splitList } from "./trigger";
import { buildWebhookAction, DEFAULT_TIMEOUT, MAX_TIMEOUT, prettyBody, previewRequest, sampleEvent, TEMPLATE_STARTER, WEBHOOK_FORMATS, WEBHOOK_PLACEHOLDERS } from "./webhook";

export function WebhookActionFields(props: { dlg: ScheduleDialogState; kit: FieldKit }) {
  const dlg = props.dlg;
  const f = dlg.form;
  const { field, text } = props.kit;
  const scope = () => f().scope || "";
  const project = () => (f().scope && f().scope !== GLOBAL ? f().scope : null);
  /** The preview: the action of the form for a sample event of the first kind. */
  const preview = createMemo(() => {
    const built = buildWebhookAction({ ...f(), hookUrl: f().hookUrl || "https://example.invalid/", hookTemplate: f().hookTemplate });
    if ("error" in built) return { error: built.error };
    const kind = f().specKind === "on" ? (splitList(f().kinds)[0] ?? "monitor.status") : "schedule.tick";
    const e = sampleEvent(kind, project(), new Date().toISOString());
    return { request: previewRequest(built.value, e, f().name || "my-trigger", project()), kind: e.kind };
  });

  return (
    <fieldset class="stack" aria-label="Webhook">
      <legend class="small">Webhook</legend>
      <div class="row small" style={{ gap: "12px", "flex-wrap": "wrap" }} role="radiogroup" aria-label="URL from">
        <label class="row">
          <input type="radio" name="hook-url-mode" checked={f().hookUrlMode === "url"} onChange={() => dlg.set("hookUrlMode", "url")} /> URL
        </label>
        <label class="row">
          <input type="radio" name="hook-url-mode" checked={f().hookUrlMode === "secret"} onChange={() => dlg.set("hookUrlMode", "secret")} /> URL from a secret
        </label>
        <span class="muted">A Slack or Discord URL is a credential: keep it in a secret.</span>
      </div>
      <div class="form-grid">
        <Show
          when={f().hookUrlMode === "url"}
          fallback={field(
            "hookUrlSecret",
            "URL secret",
            <CredentialSelect kind="webhook" label="URL secret" scope={scope()} value={f().hookUrlSecret} onChange={(v) => dlg.set("hookUrlSecret", v)} />,
          )}
        >
          {field("hookUrl", "URL", <input class="mono" aria-label="Webhook URL" type="url" value={f().hookUrl} placeholder="https://ntfy.example/alerts" onInput={(e) => dlg.set("hookUrl", e.currentTarget.value)} />, () => "http or https; no user or password; no redirects are followed")}
        </Show>
        {field(
          "hookFormat",
          "Format",
          <select
            aria-label="Format"
            value={f().hookFormat}
            onChange={(e) => {
              const v = e.currentTarget.value as WebhookFormat;
              dlg.set("hookFormat", v);
              if (v === "template" && !f().hookTemplate.trim()) dlg.set("hookTemplate", TEMPLATE_STARTER);
            }}
          >
            <For each={WEBHOOK_FORMATS}>
              {(x) => (
                <option value={x.id} selected={x.id === f().hookFormat}>
                  {x.label}
                </option>
              )}
            </For>
          </select>,
          () => WEBHOOK_FORMATS.find((x) => x.id === f().hookFormat)?.hint ?? "",
        )}
        {field(
          "hookMethod",
          "Method",
          <select aria-label="Method" value={f().hookMethod} onChange={(e) => dlg.set("hookMethod", e.currentTarget.value as WebhookMethod)}>
            <option value="POST" selected={f().hookMethod === "POST"}>
              POST
            </option>
            <option value="PUT" selected={f().hookMethod === "PUT"}>
              PUT
            </option>
          </select>,
        )}
        {text("timeout", "Timeout (seconds)", String(DEFAULT_TIMEOUT), () => `Default ${DEFAULT_TIMEOUT}, at most ${MAX_TIMEOUT}`)}
        {text("hookContentType", "Content type", f().hookFormat === "ntfy" ? "text/plain" : "application/json")}
      </div>
      <Show when={f().hookFormat === "template"}>
        {field(
          "hookTemplate",
          "Body template",
          <textarea class="mono" rows={7} aria-label="Body template" value={f().hookTemplate} onInput={(e) => dlg.set("hookTemplate", e.currentTarget.value)} />,
          () => "With a JSON content type, a text value is escaped for the inside of a JSON string; {{path | json}} puts a JSON literal.",
        )}
        <details class="small">
          <summary class="muted">Placeholders</summary>
          <ul class="plain">
            <For each={WEBHOOK_PLACEHOLDERS}>
              {(p) => (
                <li>
                  <code>{p.text}</code> <span class="muted">{p.meaning}</span>
                </li>
              )}
            </For>
          </ul>
        </details>
      </Show>

      <div class="stack" style={{ gap: "4px" }} classList={{ "field-bad": !!dlg.fieldError("hookHeaders") }}>
        <span class="small">
          <strong>Headers</strong> <span class="muted">· a secret value is never shown or logged</span>
        </span>
        <Index each={f().hookHeaders}>
          {(h, i) => (
            <div class="row hook-header" style={{ gap: "6px", "flex-wrap": "wrap", "align-items": "flex-start" }} aria-label={`Header ${i + 1}`}>
              <input class="mono" aria-label={`Header ${i + 1} name`} value={h().name} placeholder="Authorization" style={{ flex: "1 1 9rem" }} onInput={(e) => dlg.setHeader(i, { name: e.currentTarget.value })} />
              <select aria-label={`Header ${i + 1} from`} value={h().mode} onChange={(e) => dlg.setHeader(i, { mode: e.currentTarget.value as "value" | "secret" })}>
                <option value="value" selected={h().mode === "value"}>
                  value
                </option>
                <option value="secret" selected={h().mode === "secret"}>
                  secret
                </option>
              </select>
              <div style={{ flex: "2 1 12rem" }}>
                <Show when={h().mode === "secret"} fallback={<input class="mono" aria-label={`Header ${i + 1} value`} value={h().value} placeholder="value" style={{ width: "100%" }} onInput={(e) => dlg.setHeader(i, { value: e.currentTarget.value })} />}>
                  <CredentialSelect kind="webhook" label={`Header ${i + 1} secret`} scope={scope()} value={h().secret} onChange={(v) => dlg.setHeader(i, { secret: v })} />
                </Show>
              </div>
              <button type="button" class="ghost small" aria-label={`Remove header ${i + 1}`} onClick={() => dlg.removeHeader(i)}>
                ✕
              </button>
            </div>
          )}
        </Index>
        <div>
          <button type="button" class="small" onClick={() => dlg.addHeader()}>
            Add header
          </button>
        </div>
        <Show when={dlg.fieldError("hookHeaders")}>
          {(m) => (
            <span class="field-error small" role="alert">
              {m()}
            </span>
          )}
        </Show>
      </div>

      <div class="form-grid">
        {field(
          "hookSignSecret",
          "Signing secret",
          <CredentialSelect kind="webhook" label="Signing secret" scope={scope()} value={f().hookSignSecret} onChange={(v) => dlg.set("hookSignSecret", v)} />,
          () => "Optional: X-Terrarium-Signature: sha256=<HMAC-SHA256 of the raw body>",
        )}
        <Show when={f().hookSignSecret}>{text("hookSignHeader", "Signature header", "X-Terrarium-Signature")}</Show>
      </div>
      <Show when={f().hookSignSecret}>
        <button type="button" class="ghost small" style={{ "align-self": "flex-start" }} onClick={() => dlg.set("hookSignSecret", "")}>
          No signature
        </button>
      </Show>

      <details class="hook-preview" open aria-label="Request preview">
        <summary class="small">
          <strong>Preview</strong> <span class="muted">· a sample {"request" in preview() ? (preview() as { kind: string }).kind : ""} event; the daemon renders the real one</span>
        </summary>
        <Show when={"request" in preview() ? (preview() as { request: ReturnType<typeof previewRequest> }).request : null} fallback={<p class="small muted">{(preview() as { error?: string }).error}</p>}>
          {(r) => (
            <div class="stack" style={{ gap: "4px" }}>
              <pre class="run-output mono small" aria-label="Preview headers">
                {`${r().method} ${r().url}\n${r()
                  .headers.map(([k, v]) => `${k}: ${v}`)
                  .join("\n")}`}
              </pre>
              <pre class="run-output mono small" aria-label="Preview body">
                {prettyBody(r().body)}
              </pre>
              <Show when={r().problem}>
                {(p) => (
                  <span class="chip warn" role="status">
                    {p()}
                  </span>
                )}
              </Show>
            </div>
          )}
        </Show>
      </details>
    </fieldset>
  );
}
