# Provider marks

The marks that `components/providerLogos.ts` shows next to a subscription
(TER-642). Each file is the vendor's own mark from its brand, press, or icon
page, official site or official package, downloaded on 2026-11-10 (`openai` and `zai`: 2026-11-11). The dashboard bundles them: nothing loads from
a vendor site at run time.

These files are not under the licence of this repo. They are trademarks of
their owners. The dashboard uses them only to name the vendor of a
subscription, next to the text name, unchanged in shape and colour.

## Cleaning

Each file went through the same cleaning step. The shapes and colours are
unchanged. The step:

- keeps only `svg`, `g`, `path`, `rect`, `circle`, `polygon`, `defs`, gradients and `stop`
- drops `script`, `foreignObject`, `style`, `title`, comments, `class`, every
  `href`, and every event attribute (`on*`)
- drops the clip paths: in these files they clip to the full view box only
- keeps a `url()` only when it names a gradient inside the same file
- drops the `width` and `height` of the root: CSS sets the size

`providerLogos.test.ts` checks these rules on every file in this folder.

## Files

| File | Provider kind | Vendor file | Source | Terms |
|---|---|---|---|---|
| `zai.svg` | `zai` | Z.ai site mark, white glyph on a dark tile with a white border, used unchanged in both themes | Z.ai's own site `https://z.ai`, linked asset `https://z-cdn.chatglm.cn/z-ai/static/logo.svg`. Styles expanded into presentation attributes, unused CSS and empty groups removed. Shapes and colours unchanged. | Z.ai's terms `https://chat.z.ai/legal-agreement/terms-of-service`, Intellectual Property clause 2, require prior written consent for logo display. The operator chose to use this mark for this internal tool on 2026-11-11, question `01M755X450ZTNCDRC80QNKHPJ2` (TER-780). This decision is not a trademark licence. |
| `anthropic.svg` | `anthropic` | Claude Spark, Clay | Anthropic press kit, `https://www.anthropic.com/press-kit` (redirects to `https://www-cdn.anthropic.com/ae59ca4ca194dac9c9dc3bc78c5829468cb0e8af.zip`), `Claude logos/3 Claude Spark/SVG/Claude Spark - Clay.svg` | Press kit for media use. No licence text in the kit. |
| `openai-light.svg`, `openai-dark.svg` | `openai` | OpenAI logo (Blossom), `OpenaiLogoRegular`, drawn black and white | OpenAI's own npm package `@openai/apps-sdk-ui` 0.2.2 (`https://www.npmjs.com/package/@openai/apps-sdk-ui`, source `https://github.com/openai/apps-sdk-ui`, published by the `openai-publisher` account), `dist/es/components/Icon/svg/OpenaiLogoRegular.js`. The path is unchanged. The file uses `currentColor`: the light file sets black and the dark file white. | The package code is MIT, copyright OpenAI. The MIT licence covers the code, not the trademark. The brand page `https://openai.com/brand/` refuses automated downloads (HTTP 403), so its terms could not be read. |
| `github-light.svg`, `github-dark.svg` | `github` | Copilot icon, black and white | GitHub brand page, `https://brand.github.com/foundations/logo`, download `https://brand.github.com/GitHub_Logos.zip`, `SVG/Copilot_Icon_Black.svg` and `SVG/Copilot_Icon_White.svg` | GitHub trademark. The brand page allows the marks to indicate an integration, in black or white, unmodified, and not as the icon of your own product. |
| `openrouter-light.svg`, `openrouter-dark.svg` | `openrouter` | Glyph, Ink and Cloud | OpenRouter brand page, `https://openrouter.ai/brand`, `https://openrouter.ai/brand/logos/transparent/glyph/svg/glyph-ink.svg` and `.../glyph-cloud.svg` | "Please don't stretch, recolor, or remix the marks." No licence named. |
| `moonshot-light.svg`, `moonshot-dark.svg` | `moonshot` | Kimi logomark, Light and Dark | Kimi brand guideline, `https://www.kimi.ai/resources/kimi-brand` (linked from `https://www.moonshot.ai/`), download `kimi-logo-assets.zip`, `KIMI-logos/Logomark/SVG/Logomark_Light.svg` and `Logomark_Dark.svg` | Copyright Moonshot AI. No licence named on the page. |
| `bedrock.svg` | `bedrock` | Amazon Bedrock architecture icon, 32 | AWS architecture icons, `https://aws.amazon.com/architecture/icons/`, package `Icon-package_07312026`, `Architecture-Service-Icons_07312026/Arch_Artificial-Intelligence/32/Arch_Amazon-Bedrock_32.svg` | AWS allows the icons in architecture diagrams and materials such as whitepapers and presentations. A dashboard identifier is not named in the terms. |
| `gcp.svg` | `gcp` | Vertex AI product icon, colour | Google Cloud icon library, `https://cloud.google.com/icons`, `https://services.google.com/fh/files/misc/core-products-icons.zip`, `Unique Icons/Vertex AI/SVG/VertexAI-512-color.svg` | Official product icons "for your diagrams, technical documentation, and more". No licence named. |
| `foundry.svg` | `foundry` | Microsoft Foundry service icon | Azure architecture icons, `https://learn.microsoft.com/en-us/azure/architecture/icons/`, `https://arch-center.azureedge.net/icons/Azure_Public_Service_Icons_V25.zip`, `Icons/ai + machine learning/035746832-icon-service-AI-Foundry.svg` | Microsoft permits the icons in architectural diagrams, training materials, or documentation, unmodified, with the product name close to the icon. Microsoft reserves all other rights. |

## Providers with no mark

These kinds show the letter tile until an official file is added:

- `deepseek`: DeepSeek has no brand or press page.
- `selfhosted` and `openai-compatible`: a server glyph, drawn in `components/ui.tsx`.
- Any other kind: the letter tile.

To add a mark: download it from the vendor's brand page, clean it with the
rules above, add a row to this table, and add the kind to `PROVIDER_MARKS`.
