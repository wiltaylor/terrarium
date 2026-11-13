import { describe, expect, it } from "vitest";
import { featureWidget, PROJECT_WIDGET_VIEWS, validFeatureOptions, widgetBadgeSources, widgetDefinition, WIDGET_REGISTRY } from "./widgetRegistry";
import type { PageWidget, ProjectWidgetKind } from "../../api/types";

describe("widget registry", () => {
  it("has one stable part per kind and view, with the first view as default", () => {
    expect(new Set(WIDGET_REGISTRY.map(item => item.part)).size).toBe(WIDGET_REGISTRY.length);
    for (const [kind, views] of Object.entries(PROJECT_WIDGET_VIEWS)) {
      expect(widgetDefinition({ kind: kind as ProjectWidgetKind })?.view).toBe(views[0]);
      for (const view of views) expect(widgetDefinition({ kind: kind as ProjectWidgetKind, options: { view } })?.part).toBe(`${kind}:${view}`);
    }
  });

  it("keeps repeated kinds independent with saved options and IDs", () => {
    const a = featureWidget("board", "lanes", "a", 0, "first");
    const b = featureWidget("board", "done", "b", 3, "second");
    const restored = JSON.parse(JSON.stringify([a, b]));
    expect(restored).toEqual([a, b]);
    b.options!.search = "changed";
    expect(a.options?.search).toBe("first");
    expect(widgetBadgeSources([a, b])).toEqual(["board", "board"]);
  });

  it("refuses unknown views, unbounded options, and project overrides", () => {
    expect(validFeatureOptions("board", { view: "done", search: "a" })).toBe(true);
    expect(validFeatureOptions("board", {})).toBe(true);
    expect(validFeatureOptions("board", { view: "month" })).toBe(false);
    expect(validFeatureOptions("board", { project: "other" })).toBe(false);
    expect(validFeatureOptions("board", { search: "é".repeat(129) })).toBe(false);
    expect(() => featureWidget("board", "lanes", "../bad", 0)).toThrow();
    expect(() => featureWidget("board", "lanes", "ok", 0, "é".repeat(129))).toThrow();
  });

  it("has no fallback monitor renderer or badge for an unknown kind or view", () => {
    expect(widgetDefinition({ kind: "unknown" as PageWidget["kind"] })).toBeUndefined();
    expect(widgetDefinition({ kind: "board", options: { view: "month" } })).toBeUndefined();
  });
});
