// Hostile markup for the sanitizer tests and the mock's /dev/xss page.
// Every entry must come out of `renderWiki` + `sanitizeToFragment` inert, and
// also out of `sanitizeToFragment` alone (as if the renderer had a bug).

export interface XssPayload {
  name: string;
  src: string;
}

export const XSS_PAYLOADS: XssPayload[] = [
  { name: "plain script", src: "<script>window.__xss=1</script>" },
  { name: "script, mixed case", src: "<ScRiPt>window.__xss=1</sCrIpT>" },
  { name: "script, src", src: '<script src="https://evil.example/x.js"></script>' },
  { name: "script, entity-encoded tag", src: "&lt;script&gt;window.__xss=1&lt;/script&gt;" },
  { name: "script, double-encoded", src: "&amp;lt;script&amp;gt;window.__xss=1&amp;lt;/script&amp;gt;" },
  { name: "script, null byte", src: "<scr\u0000ipt>window.__xss=1</script>" },
  { name: "img onerror", src: "<img src=x onerror=window.__xss=1>" },
  { name: "img onerror, quoted, newline", src: '<img src="x"\nonerror="window.__xss=1">' },
  { name: "img onerror, slash separator", src: "<img/src=x/onerror=window.__xss=1>" },
  { name: "svg script", src: "<svg><script>window.__xss=1</script></svg>" },
  { name: "svg onload", src: "<svg onload=window.__xss=1></svg>" },
  { name: "svg animate href", src: '<svg><a><animate attributeName="href" values="javascript:window.__xss=1"/><text y="20">x</text></a></svg>' },
  { name: "math mglyph mXSS", src: '<math><mtext><table><mglyph><style><img src=x onerror=window.__xss=1></style></mglyph></table></mtext></math>' },
  { name: "iframe javascript", src: '<iframe src="javascript:window.__xss=1"></iframe>' },
  { name: "iframe srcdoc", src: '<iframe srcdoc="<script>parent.__xss=1</script>"></iframe>' },
  { name: "meta refresh", src: '<meta http-equiv="refresh" content="0;url=javascript:window.__xss=1">' },
  { name: "base href", src: '<base href="https://evil.example/">' },
  { name: "object data", src: '<object data="javascript:window.__xss=1"></object>' },
  { name: "embed", src: '<embed src="data:text/html,<script>parent.__xss=1</script>">' },
  { name: "form action", src: '<form action="javascript:window.__xss=1"><button>go</button></form>' },
  { name: "style tag", src: "<style>body{background:url(javascript:window.__xss=1)}</style>" },
  { name: "style attr", src: '<p style="background:url(javascript:window.__xss=1)">x</p>' },
  { name: "a href javascript", src: '<a href="javascript:window.__xss=1">x</a>' },
  { name: "a href javascript, tab", src: '<a href="java\tscript:window.__xss=1">x</a>' },
  { name: "a href javascript, newline", src: '<a href="java\nscript:window.__xss=1">x</a>' },
  { name: "a href javascript, leading space", src: '<a href="  javascript:window.__xss=1">x</a>' },
  { name: "a href javascript, entities", src: '<a href="&#106;&#97;&#118;&#97;&#115;&#99;&#114;&#105;&#112;&#116;&#58;window.__xss=1">x</a>' },
  { name: "a href javascript, hex entities", src: '<a href="&#x6A;avascript&#x3A;window.__xss=1">x</a>' },
  { name: "a href javascript, named entity colon", src: '<a href="javascript&colon;window.__xss=1">x</a>' },
  { name: "a href javascript, entity tab", src: '<a href="jav&#x09;ascript:window.__xss=1">x</a>' },
  { name: "a href data:text/html", src: '<a href="data:text/html;base64,PHNjcmlwdD5wYXJlbnQuX194c3M9MTwvc2NyaXB0Pg==">x</a>' },
  { name: "a href vbscript", src: '<a href="vbscript:msgbox(1)">x</a>' },
  { name: "img data:svg", src: '<img src="data:image/svg+xml;base64,PHN2ZyBvbmxvYWQ9YWxlcnQoMSk+">' },
  { name: "markdown link javascript", src: "[x](javascript:window.__xss=1)" },
  { name: "markdown link javascript, entities", src: "[x](&#106;avascript:window.__xss=1)" },
  { name: "markdown link javascript, encoded colon", src: "[x](javascript&#58;window.__xss=1)" },
  { name: "markdown link javascript, angle brackets", src: "[x](<javascript:window.__xss=1>)" },
  { name: "markdown link data:text/html", src: "[x](data:text/html,<script>parent.__xss=1</script>)" },
  { name: "markdown image onerror title", src: '![x](x "\\" onerror=\\"window.__xss=1")' },
  { name: "markdown image data:svg", src: "![x](data:image/svg+xml;base64,PHN2ZyBvbmxvYWQ9YWxlcnQoMSk+)" },
  { name: "markdown autolink javascript", src: "<javascript:window.__xss=1>" },
  { name: "wikilink with markup", src: '[[<img src=x onerror=window.__xss=1>]]' },
  { name: "code fence lang breakout", src: '```"><img src=x onerror=window.__xss=1>\ncode\n```' },
  { name: "nested noscript mXSS", src: '<noscript><p title="</noscript><img src=x onerror=window.__xss=1>">' },
  { name: "nested template mXSS", src: "<template><style></template><img src=x onerror=window.__xss=1></style>" },
  { name: "comment breakout", src: "<!--><img src=x onerror=window.__xss=1>-->" },
  { name: "textarea breakout", src: "<textarea></textarea><img src=x onerror=window.__xss=1></textarea>" },
  { name: "select/option breakout", src: "<select><option><style></option></select><img src=x onerror=window.__xss=1></style>" },
  { name: "details ontoggle", src: "<details open ontoggle=window.__xss=1>x</details>" },
  { name: "input autofocus onfocus", src: "<input autofocus onfocus=window.__xss=1>" },
  { name: "dom clobbering", src: '<a id="__xss" name="__xss" href="#">x</a><form id="test"><input name="attributes"></form>' },
  { name: "target without rel", src: '<a href="https://evil.example/" target="_blank">x</a>' },
];

/** Every payload in one markdown document, for the /dev/xss page. */
export const XSS_DOCUMENT = XSS_PAYLOADS.map((p) => `### ${p.name}\n\n${p.src}\n`).join("\n");
