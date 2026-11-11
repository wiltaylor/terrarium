import { ESLint } from "eslint";
import { expect, it } from "vitest";

it("forbids local singleton imports in feature code", async () => {
  const eslint = new ESLint();
  for (const statement of [
    'import { api } from "../../stores";',
    'import { agents } from "../../stores/index";',
    'import * as local from "../../stores";',
  ]) {
    const [result] = await eslint.lintText(statement, { filePath: "src/features/home/Home.tsx" });
    expect(result!.messages.some((m) => m.ruleId === "no-restricted-imports")).toBe(true);
  }
  const [result] = await eslint.lintText('import { useInstance } from "../../instance"; export const useApi = () => useInstance().api;', { filePath: "src/features/home/Home.tsx" });
  expect(result!.messages.filter((m) => m.ruleId === "no-restricted-imports")).toEqual([]);
}, 30_000);
