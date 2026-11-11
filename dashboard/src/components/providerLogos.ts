// The provider marks of the subscription views (TER-642). The SVG files are
// the vendors' own marks, cleaned and bundled: see assets/providers/README.md
// for the source and the terms of each file. Nothing loads from a vendor site.
import anthropic from "../assets/providers/anthropic.svg";
import bedrock from "../assets/providers/bedrock.svg";
import foundry from "../assets/providers/foundry.svg";
import gcp from "../assets/providers/gcp.svg";
import githubDark from "../assets/providers/github-dark.svg";
import githubLight from "../assets/providers/github-light.svg";
import moonshotDark from "../assets/providers/moonshot-dark.svg";
import moonshotLight from "../assets/providers/moonshot-light.svg";
import openaiDark from "../assets/providers/openai-dark.svg";
import openaiLight from "../assets/providers/openai-light.svg";
import openrouterDark from "../assets/providers/openrouter-dark.svg";
import openrouterLight from "../assets/providers/openrouter-light.svg";

/** The mark of a provider: one file for both themes, or one file for each. */
export interface ProviderMark {
  light: string;
  dark: string;
  /** Draw the mark on a white plate in both themes: a dark mark that has no variant for dark backgrounds. */
  plate?: boolean;
}

const both = (src: string): ProviderMark => ({ light: src, dark: src });

/**
 * The bundled mark of each provider kind. A kind that is not here (zai,
 * deepseek: no mark from a brand or press page could be fetched) gets the
 * letter tile. bedrock, gcp and foundry are the cloud kinds of TER-630.
 */
export const PROVIDER_MARKS: Record<string, ProviderMark> = {
  anthropic: both(anthropic),
  openai: { light: openaiLight, dark: openaiDark },
  github: { light: githubLight, dark: githubDark },
  moonshot: { light: moonshotLight, dark: moonshotDark },
  openrouter: { light: openrouterLight, dark: openrouterDark },
  bedrock: both(bedrock),
  gcp: both(gcp),
  foundry: { ...both(foundry), plate: true },
};

/** The vendor and product name of each provider kind, for the tooltip and the screen reader. */
export const PROVIDER_LABELS: Record<string, string> = {
  anthropic: "Anthropic (Claude)",
  openai: "OpenAI (ChatGPT, Codex)",
  github: "GitHub Copilot",
  moonshot: "Moonshot AI (Kimi)",
  zai: "Z.ai (GLM)",
  openrouter: "OpenRouter",
  deepseek: "DeepSeek",
  selfhosted: "Self-hosted server",
  "openai-compatible": "Self-hosted server (OpenAI-compatible)",
  bedrock: "Amazon Bedrock",
  gcp: "Google Cloud Vertex AI",
  foundry: "Microsoft Foundry",
};

/** The background of the letter tile, for a provider with no bundled mark. */
export const PROVIDER_COLORS: Record<string, string> = {
  anthropic: "#c96442",
  openai: "#10a37f",
  github: "#6e40c9",
  google: "#4285f4",
  openrouter: "#6467f2",
  xai: "#222",
  groq: "#f55036",
  deepseek: "#4d6bfe",
};

const key = (provider: string) => provider.trim().toLowerCase();

/** The bundled mark of `provider`, or undefined for the fallback. */
export const providerMark = (provider: string): ProviderMark | undefined => PROVIDER_MARKS[key(provider)];

/** The label of `provider`: its vendor name, or the kind itself when it is not known. */
export const providerLabel = (provider: string): string => PROVIDER_LABELS[key(provider)] ?? (provider.trim() || "Unknown provider");

/** A self-hosted server: a server glyph instead of a vendor mark. */
export const isSelfHostedKind = (provider: string): boolean => key(provider) === "selfhosted" || key(provider) === "openai-compatible";
