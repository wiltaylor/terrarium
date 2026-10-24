import js from "@eslint/js";
import tseslint from "typescript-eslint";
import solid from "eslint-plugin-solid/configs/typescript";
import globals from "globals";

export default tseslint.config(
  { ignores: ["dist/", "node_modules/"] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ["src/**/*.{ts,tsx}"],
    ...solid,
    languageOptions: {
      parser: tseslint.parser,
      parserOptions: { project: "./tsconfig.json" },
      globals: { ...globals.browser },
    },
    rules: {
      "@typescript-eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_", varsIgnorePattern: "^_", destructuredArrayIgnorePattern: "^_" }],
      // Solid assigns `let el!: T` through the `ref` prop, which this rule cannot see.
      "no-unassigned-vars": "off",
      // Untrusted HTML goes into the DOM only through lib/sanitize (DOMPurify):
      // the `Markdown` and `SafeHtml` components. See docs/security-review.md F3.
      "no-restricted-syntax": [
        "error",
        { selector: "JSXAttribute[name.name=/^(innerHTML|outerHTML)$/]", message: "Use <SafeHtml> or <Markdown>: HTML must go through lib/sanitize." },
        { selector: "AssignmentExpression > MemberExpression.left[property.name=/^(innerHTML|outerHTML|srcdoc)$/]", message: "Use setSafeHtml from lib/sanitize." },
        { selector: "CallExpression[callee.property.name=/^(insertAdjacentHTML|createContextualFragment|setHTMLUnsafe)$/]", message: "Use setSafeHtml from lib/sanitize." },
        { selector: "CallExpression[callee.object.name='document'][callee.property.name=/^(write|writeln)$/]", message: "No document.write." },
        { selector: "CallExpression[callee.name=/^(eval|Function)$/], NewExpression[callee.name='Function']", message: "The CSP forbids eval." },
      ],
    },
  },
  {
    // Tests parse the sanitizer's output again to check it.
    files: ["src/**/*.test.ts"],
    rules: { "no-restricted-syntax": "off" },
  },
  {
    files: ["*.config.{js,ts}"],
    languageOptions: { globals: { ...globals.node } },
  },
);
