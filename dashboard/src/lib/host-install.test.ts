import { describe, expect, it } from "vitest";
import { createMockApi } from "../api/mock";
import { installCommand, installLine } from "./host-install";

describe("the host install line (docs/api.md §17.9)", () => {
  it("is the daemon's install_command line", () => {
    expect(installCommand("http://t.lan:8080/", "the_x", "lab1")).toBe(
      "curl -fsSL http://t.lan:8080/install/host.sh | sudo sh -s -- --token the_x --name lab1",
    );
  });

  it("takes the daemon's line first, else the page origin", () => {
    expect(installLine({ install_command: "curl daemon", token: "the_x", name: "lab1" }, "http://page")).toBe("curl daemon");
    expect(installLine({ token: "the_x", name: "lab1" }, "http://page:8080")).toBe(
      "curl -fsSL http://page:8080/install/host.sh | sudo sh -s -- --token the_x --name lab1",
    );
  });

  it("comes with an enrol token from the mock, as from the daemon", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const made = await api.hosts.create("lab9", { enrol: true });
    expect(made.token).toMatch(/^the_/);
    expect(made.token_kind).toBe("enrol");
    expect(made.token_expires_at).toBeTruthy();
    expect(made.install_command).toContain(`--token ${made.token} --name lab9`);
    expect(made.install_command).toContain("/install/host.sh | sudo sh -s --");
    // The list never carries the token or the line.
    const listed = JSON.stringify(await api.hosts.list());
    expect(listed).not.toContain(made.token);
    expect(listed).not.toContain("install_command");
    // A long-lived token has no line.
    const plain = await api.hosts.create("lab10");
    expect(plain.token).toMatch(/^thk_/);
    expect(plain.install_command).toBeUndefined();
  });
});
