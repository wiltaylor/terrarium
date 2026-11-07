import { describe, expect, it } from "vitest";
import { boardVersions, cardVersions, compareVersions, labelHref, matchesVersion, NO_VERSION, versionLabel, versionOf } from "./versionLabels";

describe("version labels", () => {
  it("reads the version name of a label", () => {
    expect(versionOf("version:1.4.0")).toBe("1.4.0");
    expect(versionOf("version:vnext")).toBe("vnext");
    expect(versionOf("version:")).toBeUndefined();
    expect(versionOf("feature")).toBeUndefined();
    expect(versionLabel("1.4.0")).toBe("version:1.4.0");
    expect(cardVersions(["bug", "version:vnext", "version:1.0.0", "version:vnext"])).toEqual(["vnext", "1.0.0"]);
  });

  it("orders vnext first, then the releases newest first, then other names", () => {
    const names = ["0.9.0", "beta", "1.0.0-rc.1", "vnext", "1.0.0", "0.10.0", "1.0.0-rc.2", "alpha"];
    expect([...names].sort(compareVersions)).toEqual(["vnext", "1.0.0", "1.0.0-rc.2", "1.0.0-rc.1", "0.10.0", "0.9.0", "alpha", "beta"]);
    expect(boardVersions([{ labels: ["version:0.1.0"] }, { labels: ["version:vnext", "x"] }, { labels: [] }])).toEqual(["vnext", "0.1.0"]);
  });

  it("filters the cards by version, with none for the cards with no version label", () => {
    expect(matchesVersion(["version:vnext"], undefined)).toBe(true);
    expect(matchesVersion(["version:vnext"], "vnext")).toBe(true);
    expect(matchesVersion(["version:1.0.0"], "vnext")).toBe(false);
    expect(matchesVersion(["bug"], NO_VERSION)).toBe(true);
    expect(matchesVersion(["version:vnext"], NO_VERSION)).toBe(false);
  });

  it("links a version label to the board filtered and grouped by it", () => {
    expect(labelHref("my app", "version:1.4.0")).toBe("/p/my%20app/board?version=1.4.0&group=version");
    expect(labelHref("p", "nightly")).toBeUndefined();
  });
});
