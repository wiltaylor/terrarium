import { describe, expect, it } from "vitest";
import { isSelfHostedKind, PROVIDER_MARKS, providerLabel, providerMark } from "./providerLogos";

const files = import.meta.glob<string>("../assets/providers/*.svg", { query: "?raw", import: "default", eager: true });
const readme = Object.values(import.meta.glob<string>("../assets/providers/README.md", { query: "?raw", import: "default", eager: true }))[0] ?? "";

describe("provider mark files (TER-642)", () => {
  const entries = Object.entries(files);

  it("has the bundled files", () => {
    expect(entries.length).toBeGreaterThanOrEqual(10);
  });

  it.each(entries)("%s is a small, inline-safe SVG", (path, svg) => {
    expect(svg.startsWith("<svg ")).toBe(true);
    expect(svg).toContain('xmlns="http://www.w3.org/2000/svg"');
    expect(svg.length).toBeLessThan(8 * 1024);
    expect(svg).not.toMatch(/<script|<foreignObject|<style|<image|<use|<a[\s>]|<!--|<\?xml|<!DOCTYPE/i);
    expect(svg).not.toMatch(/\s(xlink:)?href\s*=/i);
    expect(svg).not.toMatch(/\son[a-z]+\s*=/i);
    expect(svg).not.toMatch(/\sstyle\s*=/i);
    expect(svg).not.toMatch(/javascript:|data:|https?:\/\/(?!www\.w3\.org\/2000\/svg")/i);
    // A url() may only name a gradient inside the same file.
    for (const m of svg.matchAll(/url\(([^)]*)\)/g)) {
      const ref = m[1] ?? "";
      expect(ref).toMatch(/^#[A-Za-z0-9_-]+$/);
      expect(svg).toContain(`id="${ref.slice(1)}"`);
    }
    // The README names the source of each file.
    expect(readme).toContain(path.split("/").pop());
  });
});

describe("providerMark", () => {
  it("maps each provider kind with a mark to bundled files", () => {
    for (const kind of ["anthropic", "github", "moonshot", "openrouter", "bedrock", "gcp", "foundry"]) {
      const m = providerMark(kind);
      expect(m, kind).toBeDefined();
      expect(m!.light).toBeTruthy();
      expect(m!.dark).toBeTruthy();
    }
    expect(providerMark(" GitHub ")).toBe(PROVIDER_MARKS.github);
  });

  it("gives a light and a dark file for the marks that need one", () => {
    for (const kind of ["github", "moonshot", "openrouter"]) expect(PROVIDER_MARKS[kind]!.light).not.toBe(PROVIDER_MARKS[kind]!.dark);
  });

  it("leaves the kinds with no official file to the fallback", () => {
    for (const kind of ["openai", "zai", "deepseek", "selfhosted", "openai-compatible", "acme", ""]) expect(providerMark(kind)).toBeUndefined();
  });
});

describe("providerLabel", () => {
  it("names the vendor, or the kind itself", () => {
    expect(providerLabel("anthropic")).toBe("Anthropic (Claude)");
    expect(providerLabel("bedrock")).toBe("Amazon Bedrock");
    expect(providerLabel("gcp")).toBe("Google Cloud Vertex AI");
    expect(providerLabel("foundry")).toBe("Microsoft Foundry");
    expect(providerLabel("acme")).toBe("acme");
    expect(providerLabel(" ")).toBe("Unknown provider");
  });

  it("knows the self-hosted kinds", () => {
    expect(isSelfHostedKind("selfhosted")).toBe(true);
    expect(isSelfHostedKind("openai-compatible")).toBe(true);
    expect(isSelfHostedKind("openai")).toBe(false);
  });
});
