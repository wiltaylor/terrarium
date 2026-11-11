import { describe, expect, it } from "vitest";
import { remoteImagePath } from "./html";

const instance = { key: "remote:lab" as const, remoteOrigin: "https://remote.example", path: (s: string) => s.replace(/^\/remote\/lab/, "") };
const page = new URL("https://home.example/remote/lab/p/demo/knowledge/wiki/start");
describe("remote image paths", () => {
  it("rebases only selected-remote or relative images", () => {
    expect(remoteImagePath("https://remote.example/a/01ARZ3NDEKTSV4RRFFQ69G5FAV/photo.png?sig=x", page, instance)).toBe("/api/remotes/lab/proxy/image/a/01ARZ3NDEKTSV4RRFFQ69G5FAV/photo.png?sig=x");
    expect(remoteImagePath("/api/agents", page, instance)).toBeUndefined();
  });
  it.each(["https://evil.example/api/image", "//evil.example/a/image", "data:image/png;base64,AAAA", "https://user:pass@remote.example/a/image", "/api/%2fimage", "/api/%25image", "/settings", "/api/\\evil"])("blocks %s", (raw) => {
    expect(remoteImagePath(raw, page, instance)).toBeUndefined();
  });
});
