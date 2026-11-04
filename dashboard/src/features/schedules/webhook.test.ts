import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import type { ScheduleCaller } from "../../api/mock/schedules";
import type { Schedule, WebhookAction } from "../../api/types";
import { actionWords } from "../../lib/schedule";
import { createDeliveries, touchesDeliveries } from "./deliveries";
import { createScheduleDialog, emptyScheduleForm, fieldOf, formFromSchedule, scheduleBody } from "./form";
import { buildWebhookAction, canRedeliver, deliveryStatusWords, emptyWebhookFields, headerNameProblem, previewRequest, renderTemplate, sampleEvent, templateProblem, upsertDelivery, urlProblem, webhookFieldsFromAction } from "./webhook";

const mock = (scheduleCaller?: ScheduleCaller) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller });
const AT = "2026-11-03T00:00:00.000Z";
const hook = (a: Partial<WebhookAction> = {}): WebhookAction => ({ type: "webhook", url: "https://hooks.example/x", ...a });

describe("the preview of a webhook body", () => {
  const e = sampleEvent("monitor.status", "terrarium", AT);

  it("renders each format as the daemon does", () => {
    const json = JSON.parse(previewRequest(hook(), e, "page", "terrarium").body);
    expect(json.event).toMatchObject({ kind: "monitor.status", project: "terrarium", data: { to: "crit", value: 93 } });
    expect(json.trigger).toEqual({ name: "page", project: "terrarium" });
    expect(JSON.parse(previewRequest(hook({ format: "slack" }), e, "page", "terrarium").body)).toEqual({ text: "[terrarium] monitor.status: monitor disk-usage: ok → crit 93 %" });
    expect(JSON.parse(previewRequest(hook({ format: "discord" }), e, "page", "terrarium").body).content).toContain("ok → crit");
    const ntfy = previewRequest(hook({ format: "ntfy" }), e, "page", "terrarium");
    expect(ntfy.body).toBe("monitor disk-usage: ok → crit 93 %");
    expect(ntfy.headers).toContainEqual(["Priority", "5"]);
    expect(ntfy.headers).toContainEqual(["Title", "terrarium: monitor.status"]);
    expect(ntfy.headers).toContainEqual(["Content-Type", "text/plain; charset=utf-8"]);
    expect(ntfy.problem).toBeNull();
  });

  it("escapes a template for JSON, leaves a missing path empty, and puts | json as a literal", () => {
    const tricky = { ...e, summary: 'a "quoted"\nline\\' };
    const doc = { event: tricky, trigger: { name: "page", project: "terrarium" } };
    const out = renderTemplate('{"s": "{{event.summary}}", "x": "{{event.data.nope}}", "v": {{event.data.value}}, "d": {{event.data | json}}, "m": {{event.missing | json}}, "t": "{{trigger.name}}"}', doc, true);
    const v = JSON.parse(out);
    expect(v).toMatchObject({ s: 'a "quoted"\nline\\', x: "", v: 93, m: null, t: "page" });
    expect(v.d.monitor).toBe("disk-usage");
    expect(renderTemplate("{{event.kind}} {{event.summary}}", doc, false)).toBe('monitor.status a "quoted"\nline\\');
    expect(templateProblem("{{event.kind}}")).toBeNull();
    expect(templateProblem("{{nope}}")).toContain("unknown placeholder");
    expect(templateProblem("{{event.kind | upper}}")).toContain("unknown filter");
    expect(templateProblem("{{event.kind")).toContain("without");
  });

  it("names a body that is not JSON for a JSON content type, and masks secret headers", () => {
    const bad = previewRequest(hook({ format: "template", template: '{"s": {{event.summary}}}' }), e, "page", null);
    expect(bad.problem).toContain("not valid JSON");
    const r = previewRequest(hook({ headers: [{ name: "Authorization", secret: "hook-token" }, { name: "X-Env", value: "prod" }], signing: { secret: "hook-sign" } }), e, "page", null);
    expect(r.headers).toContainEqual(["Authorization", "[secret:hook-token]"]);
    expect(r.headers).toContainEqual(["X-Env", "prod"]);
    expect(r.headers.some(([k, v]) => k === "X-Terrarium-Signature" && v.includes("hook-sign"))).toBe(true);
  });

  it("makes a sample of a glob kind", () => {
    expect(sampleEvent("ci.*", null, AT).kind).toBe("ci.sample");
    expect(sampleEvent("*", null, AT).kind).toBe("monitor.status");
    expect(sampleEvent("ci.run.failed", null, AT).data.branch).toBe("main");
  });
});

describe("the webhook fields of the form", () => {
  it("check the URL, the headers, and the time limit as the daemon does", () => {
    expect(urlProblem("ftp://x/y")).toContain("scheme");
    expect(urlProblem("https://u:p@x/")).toContain("password");
    expect(urlProblem("https://hooks.example/x")).toBeNull();
    expect(headerNameProblem("Content-Length")).toContain("sets");
    expect(headerNameProblem("X-Terrarium-Delivery")).toContain("daemon");
    expect(headerNameProblem("bad name")).toContain("Bad header");
    expect(headerNameProblem("Authorization")).toBeNull();
    const base = { ...emptyWebhookFields(), timeout: "" };
    expect(buildWebhookAction({ ...base, hookUrl: "nope" })).toMatchObject({ field: "hookUrl" });
    expect(buildWebhookAction({ ...base, hookUrlMode: "secret" })).toMatchObject({ field: "hookUrlSecret" });
    expect(buildWebhookAction({ ...base, hookUrl: "https://x/", timeout: "31" })).toMatchObject({ field: "timeout" });
    expect(buildWebhookAction({ ...base, hookUrl: "https://x/", hookFormat: "template" })).toMatchObject({ field: "hookTemplate" });
    expect(buildWebhookAction({ ...base, hookUrl: "https://x/", hookHeaders: [{ name: "Host", mode: "value", value: "x", secret: "" }] })).toMatchObject({ field: "hookHeaders" });
    expect(buildWebhookAction({ ...base, hookUrl: "https://x/", hookHeaders: [{ name: "X-Key", mode: "secret", value: "", secret: "" }] })).toMatchObject({ field: "hookHeaders" });
  });

  it("map to the action and back, for Edit", () => {
    const a: WebhookAction = { type: "webhook", url_secret: "slack-url", method: "PUT", format: "template", template: '{"k": "{{event.kind}}"}', content_type: "application/json", headers: [{ name: "X-Key", secret: "hook-token" }, { name: "X-Env", value: "prod" }], signing: { secret: "hook-sign" }, timeout_secs: 15 };
    const f = webhookFieldsFromAction(a);
    expect(f).toMatchObject({ hookUrlMode: "secret", hookUrlSecret: "slack-url", hookMethod: "PUT", timeout: "15" });
    expect(buildWebhookAction(f)).toEqual({ value: a });
    const s = { id: "x", name: "page", project: "terrarium", enabled: true, spec: { kind: "on", kinds: ["monitor.status"] }, action: a, catch_up: "skip", created_by: "operator", run_as: "operator", next_run_at: null, last_run: null, runs_count: 0, source: "api", created_at: AT, updated_at: AT } as Schedule;
    const back = scheduleBody(formFromSchedule(s));
    expect("body" in back && back.body.action).toEqual(a);
    expect(fieldOf("action.url")).toBe("hookUrl");
    expect(fieldOf("action.headers")).toBe("hookHeaders");
    expect(fieldOf("action.signing.secret")).toBe("hookSignSecret");
    expect(actionWords(hook({ format: "ntfy" }), "terrarium")).toBe("webhook POST hooks.example · ntfy");
    expect(actionWords(hook({ url: undefined, url_secret: "slack-url" }), null)).toBe("webhook POST (secret slack-url) · json");
  });
});

describe("a webhook trigger with the mock", () => {
  it("creates, sends a test, lists deliveries, and redelivers", async () => {
    const api = mock();
    const dlg = createScheduleDialog(api, { project: "terrarium", init: { specKind: "on" } });
    dlg.set("name", "page");
    dlg.set("kinds", "monitor.status");
    dlg.set("actionType", "webhook");
    dlg.set("hookUrl", "https://ntfy.example/alerts");
    dlg.set("hookFormat", "ntfy");
    dlg.addHeader();
    dlg.setHeader(0, { name: "Authorization", mode: "secret", secret: "github-deploy" });
    const s = await dlg.submit();
    expect(s?.action).toMatchObject({ type: "webhook", format: "ntfy", headers: [{ name: "Authorization", secret: "github-deploy" }] });
    const d = createDeliveries(api, s!.id);
    await d.reload();
    expect(d.deliveries()).toEqual([]);
    const t = await d.sendTest();
    expect(t?.delivery).toMatchObject({ status: "ok", http_status: 200, test: true, event_kind: "monitor.status" });
    // The daemon's shape of a test run (docs/api.md §19.7): no event given, so no match check.
    expect(t).toMatchObject({ test: true, action: "webhook", matched: null, status: "ok" });
    expect(t?.request.headers).toContainEqual(["Authorization", "[secret:github-deploy]"]);
    expect(JSON.stringify(t)).not.toContain("ghp_mock");
    expect(d.deliveries()).toHaveLength(1);
    const copy = await d.redeliver(t!.delivery.id);
    expect(copy).toMatchObject({ status: "pending", redelivery_of: t!.delivery.id });
    expect(d.deliveries()[0]!.id).toBe(copy!.id);
    expect(touchesDeliveries({ kind: "webhook.delivery", data: { trigger: s!.id } }, s!.id)).toBe(true);
    expect(touchesDeliveries({ kind: "webhook.delivery", data: { trigger: "other" } }, s!.id)).toBe(false);
  });

  it("refuses a bad URL with the field, a global secret with no grant for a project_admin, and a test of another action", async () => {
    const api = mock();
    const body = (action: WebhookAction) => ({ name: "x", project: "terrarium", spec: { kind: "on" as const, kinds: ["monitor.status"] }, action });
    await expect(api.schedules.create(body(hook({ url: "ftp://x" })))).rejects.toMatchObject({ status: 422, details: { field: "action.url" } });
    await expect(api.schedules.create(body(hook({ headers: [{ name: "X", secret: "nope" }] })))).rejects.toMatchObject({ status: 422, details: { field: "action.headers" } });
    const admin = mock({ role: "project_admin", project: "terrarium" });
    await expect(admin.schedules.create(body(hook({ headers: [{ name: "X", secret: "github-deploy" }] })))).rejects.toMatchObject({ status: 403 });
    // The project's own secret is fine.
    await expect(admin.schedules.create(body(hook({ headers: [{ name: "X", secret: "openai" }] })))).resolves.toMatchObject({ name: "x" });
    const list = await api.schedules.list({ project: "terrarium" });
    const mail = list.find((x) => x.action.type === "mail")!;
    await expect(api.schedules.test(mail.id)).rejects.toMatchObject({ status: 422 });
  });

  it("seeds a webhook trigger with ok, retry, 404, and dropped deliveries", async () => {
    const api = mock();
    const page = (await api.schedules.list({ kind: "trigger" })).find((s) => s.name === "page-ntfy")!;
    const ds = (await api.schedules.deliveries(page.id)).items;
    expect(ds.map((d) => d.status)).toEqual(["dropped", "pending", "failed", "ok"]);
    expect(deliveryStatusWords(ds[1]!)).toMatch(/^retry 3 at /);
    expect(canRedeliver(ds[0]!)).toBe(false);
    expect(canRedeliver(ds[2]!)).toBe(true);
    await expect(api.schedules.redeliver(page.id, ds[0]!.id)).rejects.toMatchObject({ status: 422 });
    expect(upsertDelivery(ds, { ...ds[3]!, status: "failed" })[3]!.status).toBe("failed");
  });
});

describe("the default form", () => {
  it("has the webhook defaults", () => {
    expect(emptyScheduleForm("p")).toMatchObject({ hookUrlMode: "url", hookMethod: "POST", hookFormat: "json", hookHeaders: [] });
  });
});
