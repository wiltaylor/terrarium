import { describe, expect, it, vi } from "vitest";
import { ApiError, createHttpTransport, isMissingEndpoint, parseErrorBody } from "../api/http";
import { describeDetails, describeError, errorText, NOT_AVAILABLE } from "./errors";

const respond = (status: number, body: unknown) => vi.fn(async () => new Response(JSON.stringify(body), { status }));

describe("daemon error bodies (docs/api.md §2.5)", () => {
  it("parses {error: {code, message, details}} into an ApiError", async () => {
    const body = { error: { code: "dependencies_unmet", message: "TER-2 depends on TER-1 that are not done", details: { blocked_by: ["TER-1"] } } };
    const t = createHttpTransport({ fetch: respond(409, body), getToken: () => "x" });
    const err = (await t.request("POST", "/api/cards/TER-2/claim").catch((e: unknown) => e)) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(409);
    expect(err.code).toBe("dependencies_unmet");
    expect(err.message).toBe("TER-2 depends on TER-1 that are not done");
    expect(err.details).toEqual({ blocked_by: ["TER-1"] });
  });

  it("still reads the old {error: string} shape and plain text", () => {
    expect(parseErrorBody({ error: "bad" })).toEqual({ code: "error", message: "bad" });
    expect(parseErrorBody("oops")).toBeNull();
  });
});

describe("describeError", () => {
  it("shows WIP-limit details with readable labels", () => {
    const v = describeError(new ApiError(409, "column `in_progress` is at its WIP limit (2)", undefined, "wip_limit", { column: "in_progress", wip_limit: 2 }))!;
    expect(v.code).toBe("wip_limit");
    expect(v.details).toEqual([
      { label: "Column", value: "in_progress", pre: false },
      { label: "WIP limit", value: "2", pre: false },
    ]);
  });

  it("shows the wiki stale-base diff as preformatted text", () => {
    const diff = "- old line\n+ new line";
    const v = describeError(new ApiError(409, "stale base 2", undefined, "conflict", { base: 2, current_rev: 3, diff }))!;
    expect(v.details).toContainEqual({ label: "Changes since your base", value: diff, pre: true });
    expect(v.details).toContainEqual({ label: "Your base", value: "2", pre: false });
  });

  it("handles non-object details, plain errors, strings, and nothing", () => {
    expect(describeDetails("line 1\nline 2")).toEqual([{ label: "Details", value: "line 1\nline 2", pre: true }]);
    expect(describeDetails(null)).toEqual([]);
    expect(describeError(new Error("boom"))).toEqual({ message: "boom", details: [] });
    expect(errorText("text")).toBe("text");
    expect(describeError(null)).toBeNull();
    expect(describeError("")).toBeNull();
  });

  it("says 'not available yet' for a route the daemon does not have", () => {
    const missing = new ApiError(404, "no such API route", undefined, "not_found");
    expect(isMissingEndpoint(missing)).toBe(true);
    expect(describeError(missing)!.message).toBe(NOT_AVAILABLE);
    // A missing object is not a missing endpoint.
    expect(isMissingEndpoint(new ApiError(404, "agent `x` not found", undefined, "not_found"))).toBe(false);
    expect(isMissingEndpoint(new ApiError(405, "method", undefined, "method_not_allowed"))).toBe(true);
  });
});
