import js from "@eslint/js";
import tseslint from "typescript-eslint";
import solid from "eslint-plugin-solid/configs/typescript";
import globals from "globals";

/** The DOM properties and attributes that parse a string as HTML. */
const SINK_PROPS = "/^(innerHTML|outerHTML|srcdoc|srcDoc)$/";
const SINK_MESSAGE = "Untrusted HTML must go through lib/sanitize: use <SafeHtml>, <Markdown>, or setSafeHtml.";

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
      // the `Markdown` and `SafeHtml` components. See docs/security-review.md
      // F3 and F14. `src/lib/sink-lint.test.ts` checks that each form is caught.
      "no-restricted-syntax": [
        "error",
        // JSX: `innerHTML={x}`, Solid's `prop:innerHTML={x}` (any namespace), and `<iframe srcdoc={x}>`.
        { selector: `JSXAttribute[name.name=${SINK_PROPS}]`, message: SINK_MESSAGE },
        { selector: `JSXAttribute[name.name.name=${SINK_PROPS}]`, message: SINK_MESSAGE },
        // `el.innerHTML = x`, `el["innerHTML"] = x`, and `` el[`innerHTML`] = x ``.
        { selector: `AssignmentExpression > MemberExpression.left[property.name=${SINK_PROPS}]`, message: SINK_MESSAGE },
        { selector: `AssignmentExpression > MemberExpression.left[computed=true][property.value=${SINK_PROPS}]`, message: SINK_MESSAGE },
        { selector: `AssignmentExpression > MemberExpression.left[computed=true][property.quasis.0.value.raw=${SINK_PROPS}]`, message: SINK_MESSAGE },
        // `Object.assign(el, { innerHTML })` and `Reflect.set(el, "innerHTML", x)`.
        { selector: `CallExpression[callee.object.name='Object'][callee.property.name='assign'] Property[key.name=${SINK_PROPS}]`, message: SINK_MESSAGE },
        { selector: `CallExpression[callee.object.name='Object'][callee.property.name='assign'] Property[key.value=${SINK_PROPS}]`, message: SINK_MESSAGE },
        { selector: `CallExpression[callee.object.name='Reflect'][callee.property.name=/^(set|defineProperty)$/][arguments.1.value=${SINK_PROPS}]`, message: SINK_MESSAGE },
        // `el.setAttribute("srcdoc", x)`.
        { selector: `CallExpression[callee.property.name='setAttribute'][arguments.0.value=/^srcdoc$/i]`, message: SINK_MESSAGE },
        { selector: "CallExpression[callee.property.name=/^(insertAdjacentHTML|createContextualFragment|setHTMLUnsafe|parseHTMLUnsafe|parseFromString)$/]", message: SINK_MESSAGE },
        { selector: "NewExpression[callee.name='DOMParser']", message: SINK_MESSAGE },
        { selector: "CallExpression[callee.object.name='document'][callee.property.name=/^(write|writeln)$/]", message: "No document.write." },
        { selector: "CallExpression[callee.name=/^(eval|Function)$/], NewExpression[callee.name='Function']", message: "The CSP forbids eval." },
      ],
    },
  },
  {
    files: ["src/features/**/*.{ts,tsx}", "src/components/**/*.{ts,tsx}", "src/app/**/*.{ts,tsx}"],
    ignores: ["src/**/*.test.ts", "src/app/TreeRoot.tsx"],
    rules: {
      "no-restricted-imports": ["error", {
        patterns: [{ regex: "(?:^|/)stores(?:/index)?$", message: "Read daemon state through useInstance(), not the local singleton." }],
      }],
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
