// Only these grammars ship, with no auto-detection or runtime downloads.
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import css from "highlight.js/lib/languages/css";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import python from "highlight.js/lib/languages/python";
import rust from "highlight.js/lib/languages/rust";
import sql from "highlight.js/lib/languages/sql";
import typescript from "highlight.js/lib/languages/typescript";
import xml from "highlight.js/lib/languages/xml";
import yaml from "highlight.js/lib/languages/yaml";

for (const [name, grammar] of Object.entries({ bash, css, javascript, json, python, rust, sql, typescript, xml, yaml })) {
  hljs.registerLanguage(name, grammar);
}

/** The caller must sanitize this HTML, just like the rest of the renderer. */
export function highlightCode(text: string, language: string): string | undefined {
  // Bound work on untrusted bodies. Larger blocks still display and copy in full.
  if (text.length > 100_000 || !hljs.getLanguage(language)) return undefined;
  return hljs.highlight(text, { language, ignoreIllegals: true }).value;
}
