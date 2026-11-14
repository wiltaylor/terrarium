// Services in the mock (docs/api.md §17.12, the daemon's api/services.rs):
// the definitions, the requests of each queue, and cancel. The mock caller
// is the operator, an admin of every service, so it sees every request.
// The seed has the global script service `video-gen` with one request in
// each state, and the agent service `doc-summary` of project `terrarium`.

import type { Api } from "../client";
import type { Service, ServiceDetail, ServiceRequest, ServiceRequestState } from "../types";
import type { MockWorld } from "./data";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface ServiceMockContext {
  w: MockWorld & { services?: Service[]; serviceRequests?: ServiceRequest[] };
  now: () => number;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
}

/** The states of a request that has not ended. */
export const OPEN_STATES: readonly ServiceRequestState[] = ["queued", "waiting", "running"];
const STATES: readonly ServiceRequestState[] = ["queued", "waiting", "running", "done", "failed", "cancelled", "timed_out"];

function seedServices(at: string): Service[] {
  return [
    {
      name: "doc-summary",
      description: "Summarise a document",
      kind: "agent",
      target: "ci-box",
      template: "summariser",
      prompt: "Summarise the document in the payload.",
      concurrency: 2,
      callers: ["project:terrarium"],
      project: "terrarium",
      scope: "project",
      source: "api",
      removed_at: null,
      created_at: at,
      updated_at: at,
    },
    {
      name: "video-gen",
      description: "Make a video from a prompt",
      kind: "script",
      target: "ci-box",
      command: ["/opt/video/run"],
      resources: { cpu_millis: 4000, memory: "16GiB", gpus: 1, gpu_exclusive: true },
      max_run_time: "2h",
      concurrency: 1,
      callers: ["project:terrarium", "agent:webshop/shop-lead"],
      scope: "global",
      source: "file",
      removed_at: null,
      created_at: at,
      updated_at: at,
    },
  ];
}

function seedRequests(now: number): ServiceRequest[] {
  const t = (minAgo: number) => new Date(now - minAgo * 60_000).toISOString();
  const day = 24 * 60;
  const row = (n: number, service: string, project: string | null, caller: string, state: ServiceRequestState, minAgo: number, over: Partial<ServiceRequest> = {}): ServiceRequest => ({
    id: `01M7SVCREQ${String(n).padStart(16, "0")}`,
    service,
    project,
    caller,
    caller_agent: caller === "operator" ? null : `01JB0AGENT${String(n).padStart(16, "0")}`,
    state,
    request: { prompt: `request ${n}` },
    payload_size: 0,
    payload_sha256: null,
    response: null,
    exit_code: null,
    error: null,
    wait_until: t(minAgo - day),
    created_at: t(minAgo),
    started_at: null,
    ended_at: null,
    payload_deleted_at: null,
    ...over,
  });
  return [
    row(1, "video-gen", null, "agent:terrarium/lead", "done", 300, { started_at: t(299), ended_at: t(250), exit_code: 0, response: { frames: 240 }, payload_size: 2048, payload_sha256: "ab".repeat(32) }),
    row(2, "video-gen", null, "agent:webshop/shop-lead", "failed", 200, { started_at: t(199), ended_at: t(190), exit_code: 2, error: "out of memory" }),
    row(3, "video-gen", null, "operator", "cancelled", 150, { ended_at: t(140), error: "cancelled by operator" }),
    row(4, "video-gen", null, "agent:terrarium/lead", "timed_out", 2000, { ended_at: t(2000 - day), error: "waited past the wait limit" }),
    row(5, "video-gen", null, "agent:terrarium/lead", "running", 30, { started_at: t(25) }),
    row(6, "video-gen", null, "agent:terrarium/lead", "queued", 10, { payload_size: 1_500_000, payload_sha256: "cd".repeat(32) }),
    row(7, "video-gen", null, "agent:webshop/shop-lead", "queued", 5),
    row(8, "doc-summary", "terrarium", "agent:terrarium/lead", "waiting", 3),
  ];
}

/** The queue places of the `queued` requests of one service: 1 is the oldest. */
function withPositions(rows: ServiceRequest[]): ServiceRequest[] {
  const queued = rows.filter((r) => r.state === "queued").sort((a, b) => a.id.localeCompare(b.id));
  return rows.map((r) => {
    const { position: _drop, ...rest } = r;
    const at = queued.indexOf(r);
    return at < 0 ? rest : { ...rest, position: at + 1 };
  });
}

export function createServiceMock(c: ServiceMockContext) {
  const { w, fail, reply } = c;
  const services = () => (w.services ??= seedServices(new Date(c.now()).toISOString()));
  const requests = () => (w.serviceRequests ??= seedRequests(c.now()));
  const find = (name: string) => services().find((s) => s.name === name) ?? fail(404, "not_found", `no service \`${name}\``);
  const findRequest = (id: string) => requests().find((r) => r.id === id) ?? fail(404, "not_found", `no service request \`${id}\``);
  const of = (name: string) => withPositions(requests().filter((r) => r.service === name));
  const api: Api["services"] = {
    list: () => reply(() => services().map((s) => ({ ...s }))),
    get: (name) =>
      reply((): ServiceDetail => {
        const s = find(name);
        return { ...s, open_requests: requests().filter((r) => r.service === name && OPEN_STATES.includes(r.state)).length, may_call: !s.removed_at };
      }),
    requests: (name, q = {}) =>
      reply(() => {
        find(name);
        if (q.state && !STATES.includes(q.state)) fail(400, "bad_request", `bad state \`${q.state}\``);
        const limit = Math.min(q.limit ?? 100, 1000);
        return of(name)
          .filter((r) => !q.state || r.state === q.state)
          .sort((a, b) => b.id.localeCompare(a.id))
          .slice(0, limit);
      }),
    request: (id) =>
      reply(() => {
        const r = findRequest(id);
        return of(r.service).find((x) => x.id === id)!;
      }),
    cancel: (id) =>
      reply(() => {
        const r = findRequest(id);
        if (!OPEN_STATES.includes(r.state)) fail(409, "conflict", `service request \`${id}\` has ended (${r.state})`);
        r.state = "cancelled";
        r.ended_at = new Date(c.now()).toISOString();
        r.error = "cancelled by operator";
        c.audit("service.cancel", r.service, r.project ?? undefined, { request: r.id });
        return of(r.service).find((x) => x.id === id)!;
      }),
  };
  return { api };
}
