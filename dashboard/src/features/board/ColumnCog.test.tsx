// @vitest-environment jsdom
import type { JSX } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import { ApiError } from "../../api/http";
import { waitFor } from "../../test/wait";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api });
});
const { ColumnCog, ColumnOptionsPanel } = await import("./ColumnCog");

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.restoreAllMocks(); });
function mount(node: () => JSX.Element) {
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(node, root);
  return root;
}
const text = () => document.body.textContent ?? "";

it("is a viewer when the server says the caller may not write (a sysadmin)", async () => {
  const real = await api.board.options("terrarium", "review");
  vi.spyOn(api.board, "options").mockResolvedValue({ ...real, can_edit: false });
  const catalog = vi.spyOn(api.rules, "catalog");
  const root = mount(() => <ColumnOptionsPanel project="terrarium" column="review" />);
  await waitFor(() => expect(root.querySelector("[data-options-view]")).not.toBeNull());
  expect(text()).toContain("You can read these options");
  expect(text()).toContain("Cards are released in batches of 4");
  expect(root.querySelector('[data-rule="merge-pass"]')?.textContent).toContain("When the merge job of a card here has a verdict, if the job outcome is passed: move the card to done.");
  expect(root.querySelector('[data-rule="merge-red-note"]')?.textContent).toContain("off");
  expect(text()).toContain("Last runs and the queue strip come with");
  // C2a D4: both caps for every rule, the defaults marked.
  expect(root.querySelector('[data-rule="merge-pass"]')?.textContent).toContain("At most 30 runs an hour, 10 for one card (the defaults).");
  expect(root.querySelector("input, select, button[type=submit]")).toBeNull();
  expect(catalog).not.toHaveBeenCalled();
});

it("opens from the cog as an editor for the operator, and saves with the read version", async () => {
  const root = mount(() => <ColumnCog project="terrarium" column="in_progress" title="In progress" />);
  const cog = root.querySelector<HTMLButtonElement>('[data-column-cog="in_progress"]')!;
  expect(cog.getAttribute("aria-label")).toBe("Options of In progress");
  cog.click();
  await waitFor(() => expect(document.querySelector("[data-options-edit]")).not.toBeNull());
  const put = vi.spyOn(api.board, "putOptions");
  const box = [...document.querySelectorAll<HTMLInputElement>("[data-options-edit] fieldset input[type=checkbox]")][0]!;
  box.click();
  document.querySelector<HTMLButtonElement>("[data-options-edit] button[type=submit]")!.click();
  await waitFor(() => expect(text()).toContain("Saved."));
  expect(put).toHaveBeenCalledWith("terrarium", "in_progress", expect.objectContaining({ base_version: 1, checks: expect.objectContaining({ require_comment_on_enter: true }) }));
  expect((await api.board.options("terrarium", "in_progress")).version).toBe(2);
  expect(text()).toContain("Version 2");
});

it("asks for a reload on a 409 stale base", async () => {
  const root = mount(() => <ColumnOptionsPanel project="terrarium" column="ready" />);
  await waitFor(() => expect(root.querySelector("[data-options-edit]")).not.toBeNull());
  vi.spyOn(api.board, "putOptions").mockRejectedValue(new ApiError(409, "the options are at version 2", undefined, "stale_base", { version: 2 }));
  const options = vi.spyOn(api.board, "options");
  root.querySelector<HTMLInputElement>("[data-options-edit] fieldset input[type=checkbox]")!.click();
  root.querySelector<HTMLButtonElement>("button[type=submit]")!.click();
  await waitFor(() => expect(root.querySelector("[data-stale]")).not.toBeNull());
  expect(root.querySelector<HTMLButtonElement>("button[type=submit]")!.disabled).toBe(true);
  [...root.querySelectorAll<HTMLButtonElement>("[data-stale] button")].find((b) => b.textContent === "Reload")!.click();
  await waitFor(() => expect(options).toHaveBeenCalled());
  await waitFor(() => expect(root.querySelector("[data-stale]")).toBeNull());
});

it("shows a 422 problem at its field", async () => {
  const root = mount(() => <ColumnOptionsPanel project="terrarium" column="blocked" />);
  await waitFor(() => expect(root.querySelector("[data-options-edit]")).not.toBeNull());
  const release = root.querySelector<HTMLSelectElement>('select[aria-label="Queue release"]')!;
  release.value = "batch";
  release.dispatchEvent(new Event("change", { bubbles: true }));
  root.querySelector<HTMLButtonElement>("button[type=submit]")!.click();
  await waitFor(() => expect(root.querySelector('[data-problem="queue.batch_size"]')).not.toBeNull());
  expect(root.querySelector("[data-problems]")?.textContent).toContain("queue.max_wait_seconds");
});

it("adds a rule from the catalog lists and shows the daemon's problem in that rule", async () => {
  const root = mount(() => <ColumnOptionsPanel project="terrarium" column="backlog" />);
  await waitFor(() => expect(root.querySelector("[data-options-edit]")).not.toBeNull());
  [...root.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Add rule")!.click();
  const trigger = root.querySelector<HTMLSelectElement>('select[aria-label="Trigger"]')!;
  expect([...trigger.options].map((o) => o.value)).toContain("job_result");
  // card.move without `to`: the daemon refuses at rules[0].actions[0].args.to.
  const verb = root.querySelector<HTMLSelectElement>('select[aria-label="Action"]')!;
  verb.value = "card.move";
  verb.dispatchEvent(new Event("change", { bubbles: true }));
  root.querySelector<HTMLButtonElement>("button[type=submit]")!.click();
  await waitFor(() => expect(root.querySelector('[data-field="to"] [role=alert]')?.textContent).toBe("required"));
  expect(root.querySelector(".column-rule summary")?.textContent).toContain("1 problem");
});

it("shows the effective caps in the editor and stops a max_per_hour over 600", async () => {
  const root = mount(() => <ColumnOptionsPanel project="terrarium" column="review" />);
  await waitFor(() => expect(root.querySelector("[data-options-edit]")).not.toBeNull());
  const rule = root.querySelector('[data-rule="merge-pass"]')!;
  expect(rule.querySelector("[data-rate]")?.textContent).toContain("At most 30 runs an hour, 10 for one card (the defaults).");
  const input = rule.querySelector<HTMLInputElement>('input[aria-label="Most runs an hour"]')!;
  expect(input.max).toBe("600");
  input.value = "120"; input.dispatchEvent(new Event("input", { bubbles: true }));
  await waitFor(() => expect(root.querySelector('[data-rule="merge-pass"] [data-rate]')?.textContent).toContain("At most 120 runs an hour, 120 for one card."));
  expect(root.querySelector('[data-rule="merge-pass"] summary')?.textContent).toContain("At most 120 runs an hour, 120 for one card.");
  const again = root.querySelector<HTMLInputElement>('[data-rule="merge-pass"] input[aria-label="Most runs an hour"]')!;
  again.value = "601"; again.dispatchEvent(new Event("input", { bubbles: true }));
  await waitFor(() => expect(root.querySelector("[data-rate-over]")?.textContent).toContain("At most 600 runs an hour"));
  expect(root.querySelector<HTMLButtonElement>("button[type=submit]")!.disabled).toBe(true);
});

it("asks before closing with unsaved changes", async () => {
  const root = mount(() => <ColumnCog project="terrarium" column="backlog" title="Backlog" />);
  root.querySelector<HTMLButtonElement>("[data-column-cog]")!.click();
  await waitFor(() => expect(document.querySelector("[data-options-edit]")).not.toBeNull());
  const ask = vi.spyOn(window, "confirm").mockReturnValue(false);
  const closeButton = () => document.querySelector<HTMLButtonElement>('[role="dialog"] button[aria-label="Close"]')!;
  // No change: closes without asking.
  closeButton().click();
  expect(ask).not.toHaveBeenCalled();
  expect(document.querySelector("[data-options-edit]")).toBeNull();
  root.querySelector<HTMLButtonElement>("[data-column-cog]")!.click();
  await waitFor(() => expect(document.querySelector("[data-options-edit]")).not.toBeNull());
  document.querySelector<HTMLInputElement>("[data-options-edit] fieldset input[type=checkbox]")!.click();
  closeButton().click();
  expect(ask).toHaveBeenCalledTimes(1);
  expect(document.querySelector("[data-options-edit]")).not.toBeNull();
  ask.mockReturnValue(true);
  closeButton().click();
  expect(ask).toHaveBeenCalledTimes(2);
  expect(document.querySelector("[data-options-edit]")).toBeNull();
});
