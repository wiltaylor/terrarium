// The repo browser of the mock (docs/api.md §5.3): the repo `terrarium`
// has `main` and two agent checkouts (runner-dev, adapter-codex), with
// files, a diff (committed and uncommitted), and commits. The other repos
// have `main` only, with one README.

import { ApiError } from "../http";
import type { DiffSet, FileDiff, RepoCommit, RepoCommitDetail, RepoTree, TreeChanges, TreeEntry, TreeListing } from "../types";

/** A 1x1 PNG, for the image viewer. */
const PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

const README = `# Terrarium

One container that runs AI coding harnesses side by side.

- A daemon with an API and a dashboard
- Sandboxes with **bwrap**
- A mailbox and a kanban board

<script>alert("no")</script>

See [the plan](docs/PLAN.md).
`;

const MAIN_RS = `use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: terrariumd <command>");
        return ExitCode::FAILURE;
    }
    terrariumd::run(&args[1..])
}
`;

const RUNNER_RS = `//! The runner: one process per agent.

pub struct Runner {
    pub agent: String,
    pub retries: u32,
}

impl Runner {
    pub fn new(agent: &str) -> Self {
        Self { agent: agent.to_owned(), retries: 0 }
    }
}
`;

const RUNNER_RS_FEAT = `//! The runner: one process per agent.

use std::time::Duration;

pub struct Runner {
    pub agent: String,
    pub retries: u32,
    /// The wait before the next retry.
    pub backoff: Duration,
}

impl Runner {
    pub fn new(agent: &str) -> Self {
        Self { agent: agent.to_owned(), retries: 0, backoff: Duration::from_secs(1) }
    }

    /// The next wait: twice the last, at most 60 s.
    pub fn next_backoff(&mut self) -> Duration {
        self.backoff = (self.backoff * 2).min(Duration::from_secs(60));
        self.backoff
    }
}
`;

type Files = Record<string, string | { b64: string } | { link: string }>;

const MAIN_FILES: Files = {
  "README.md": README,
  "Cargo.toml": `[workspace]\nmembers = ["crates/*"]\nresolver = "3"\n`,
  ".gitignore": "/target\n/node_modules\n",
  "docs/PLAN.md": "# Plan\n\n## 1. Goals\n\nRun many harnesses in one place.\n",
  "docs/logo.png": { b64: PNG },
  "docs/latest.md": { link: "PLAN.md" },
  "crates/terrariumd/src/main.rs": MAIN_RS,
  "crates/terrarium-runner/src/lib.rs": RUNNER_RS,
};

interface MockTree {
  tree: Omit<RepoTree, "id" | "kind" | "label">;
  files: Files;
  changes: Omit<TreeChanges, "tree">;
  commits: RepoCommitDetail[];
}

const ago = (min: number) => new Date(Date.now() - min * 60_000).toISOString();
const sha = (n: number) => (n.toString(16).padStart(4, "0") + "c0ffee").repeat(4).slice(0, 40);

const file = (path: string, status: FileDiff["status"], patch: string, extra: Partial<FileDiff> = {}): FileDiff => ({
  path,
  status,
  additions: patch.split("\n").filter((l) => l.startsWith("+")).length,
  deletions: patch.split("\n").filter((l) => l.startsWith("-")).length,
  binary: false,
  patch,
  truncated: false,
  ...extra,
});

const set = (files: FileDiff[], truncated = false): DiffSet => ({ files, truncated });

const RUNNER_PATCH = `@@ -1,12 +1,23 @@
 //! The runner: one process per agent.

+use std::time::Duration;
+
 pub struct Runner {
     pub agent: String,
     pub retries: u32,
+    /// The wait before the next retry.
+    pub backoff: Duration,
 }

 impl Runner {
     pub fn new(agent: &str) -> Self {
-        Self { agent: agent.to_owned(), retries: 0 }
+        Self { agent: agent.to_owned(), retries: 0, backoff: Duration::from_secs(1) }
+    }
+
+    /// The next wait: twice the last, at most 60 s.
+    pub fn next_backoff(&mut self) -> Duration {
+        self.backoff = (self.backoff * 2).min(Duration::from_secs(60));
+        self.backoff
     }
 }
`;

const TEST_PATCH = `@@ -0,0 +1,8 @@
+use terrarium_runner::Runner;
+
+#[test]
+fn backoff_doubles_to_a_cap() {
+    let mut r = Runner::new("w");
+    assert_eq!(r.next_backoff().as_secs(), 2);
+    for _ in 0..10 { r.next_backoff(); }
+}
`;

const README_PATCH = `@@ -3,6 +3,7 @@
 One container that runs AI coding harnesses side by side.

 - A daemon with an API and a dashboard
+- Retries with a backoff
 - Sandboxes with **bwrap**
 - A mailbox and a kanban board

`;

const CODEX_PATCH = `@@ -1,5 +1,6 @@
 use std::process::ExitCode;

 fn main() -> ExitCode {
+    // Resume the last Codex session when the thread exists.
     let args: Vec<String> = std::env::args().collect();
     if args.len() < 2 {
`;

const mainCommits: RepoCommitDetail[] = [
  {
    sha: sha(9),
    parents: [sha(8)],
    author_name: "Wil Taylor",
    author_email: "wil@example.com",
    date: ago(60 * 5),
    subject: "docs: the plan, section 1",
    committer_name: "Wil Taylor",
    committer_date: ago(60 * 5),
    message: "docs: the plan, section 1",
    diff: set([file("docs/PLAN.md", "modified", "@@ -1,3 +1,5 @@\n # Plan\n \n+## 1. Goals\n+\n Run many harnesses in one place.\n")]),
  },
  ...Array.from({ length: 7 }, (_, i) => {
    const n = 8 - i;
    return {
      sha: sha(n),
      parents: n > 1 ? [sha(n - 1)] : [],
      author_name: i % 2 ? "lead" : "Wil Taylor",
      author_email: i % 2 ? "lead@terrarium.local" : "wil@example.com",
      date: ago(60 * 24 * (i + 1)),
      subject: ["feat(runner): the runner crate", "chore: workspace", "feat: the daemon main", "docs: README", "ci: the first workflow", "chore: gitignore", "init"][i]!,
      committer_name: "Wil Taylor",
      committer_date: ago(60 * 24 * (i + 1)),
      message: ["feat(runner): the runner crate", "chore: workspace", "feat: the daemon main", "docs: README", "ci: the first workflow", "chore: gitignore", "init"][i]!,
      diff: set([file(["crates/terrarium-runner/src/lib.rs", "Cargo.toml", "crates/terrariumd/src/main.rs", "README.md", ".github/workflows/ci.yml", ".gitignore", "README.md"][i]!, i === 6 ? "added" : "modified", "@@ -1,1 +1,2 @@\n line\n+another line\n")]),
    };
  }),
];

/** The trees of each mock repo, by repo name. */
function trees(): Record<string, Record<string, MockTree>> {
  return {
    terrarium: {
      main: {
        tree: { branch: "main", head: sha(9), mirror: { fetched_at: ago(1) } },
        files: MAIN_FILES,
        changes: { base: sha(9), head: sha(9), base_ref: "refs/heads/main", committed: set([]), uncommitted: null, note: "main is the base" },
        commits: mainCommits,
      },
      "agent:01JB0WRK100000000000000002": {
        tree: { agent: "runner-dev", agent_id: "01JB0WRK100000000000000002", branch: "feat/runner-backoff", head: sha(21), base_ref: "refs/remotes/origin/main", base: sha(8), ahead: 2, behind: 1, dirty: { staged: 1, unstaged: 1, untracked: 2 } },
        files: {
          ...MAIN_FILES,
          "crates/terrarium-runner/src/lib.rs": RUNNER_RS_FEAT,
          "crates/terrarium-runner/tests/backoff.rs": TEST_PATCH.split("\n").slice(1).map((l) => l.slice(1)).join("\n"),
          "README.md": README.replace("- Sandboxes", "- Retries with a backoff\n- Sandboxes"),
          "notes.txt": "TODO: jitter\n",
          "scratch/out.log": "run 1 ok\nrun 2 ok\n",
          "secrets": { link: "/etc/passwd" },
        },
        changes: {
          agent: "runner-dev",
          agent_id: "01JB0WRK100000000000000002",
          base: sha(8),
          head: sha(21),
          base_ref: "refs/remotes/origin/main",
          branch: "feat/runner-backoff",
          committed: set([file("crates/terrarium-runner/src/lib.rs", "modified", RUNNER_PATCH), file("crates/terrarium-runner/tests/backoff.rs", "added", TEST_PATCH)]),
          uncommitted: {
            staged: set([file("README.md", "modified", README_PATCH)]),
            unstaged: set([file("docs/logo.png", "modified", "", { binary: true, additions: 0, deletions: 0 })]),
            untracked: ["notes.txt", "scratch/out.log"],
            untracked_truncated: false,
          },
          note: null,
        },
        commits: [
          {
            sha: sha(21),
            parents: [sha(20)],
            author_name: "runner-dev",
            author_email: "runner-dev@terrarium.local",
            date: ago(25),
            subject: "test(runner): the backoff doubles to a cap",
            committer_name: "runner-dev",
            committer_date: ago(25),
            message: "test(runner): the backoff doubles to a cap\n\nTER-12.",
            diff: set([file("crates/terrarium-runner/tests/backoff.rs", "added", TEST_PATCH)]),
          },
          {
            sha: sha(20),
            parents: [sha(8)],
            author_name: "runner-dev",
            author_email: "runner-dev@terrarium.local",
            date: ago(70),
            subject: "feat(runner): retry with a backoff",
            committer_name: "runner-dev",
            committer_date: ago(70),
            message: "feat(runner): retry with a backoff\n\nThe wait doubles after each failure, to at most 60 s.\n\nTER-12.",
            diff: set([file("crates/terrarium-runner/src/lib.rs", "modified", RUNNER_PATCH)]),
          },
        ],
      },
      "agent:01JB0WRK200000000000000003": {
        tree: { agent: "adapter-codex", agent_id: "01JB0WRK200000000000000003", branch: "fix/codex-resume", head: sha(31), base_ref: "refs/remotes/origin/main", base: sha(9), ahead: 1, behind: 0, dirty: { staged: 0, unstaged: 0, untracked: 0 } },
        files: { ...MAIN_FILES, "crates/terrariumd/src/main.rs": MAIN_RS.replace("    let args", "    // Resume the last Codex session when the thread exists.\n    let args") },
        changes: {
          agent: "adapter-codex",
          agent_id: "01JB0WRK200000000000000003",
          base: sha(9),
          head: sha(31),
          base_ref: "refs/remotes/origin/main",
          branch: "fix/codex-resume",
          committed: set([file("crates/terrariumd/src/main.rs", "modified", CODEX_PATCH)]),
          uncommitted: { staged: set([]), unstaged: set([]), untracked: [], untracked_truncated: false },
          note: null,
        },
        commits: [
          {
            sha: sha(31),
            parents: [sha(9)],
            author_name: "adapter-codex",
            author_email: "adapter-codex@terrarium.local",
            date: ago(12),
            subject: "fix(codex): resume the last session",
            committer_name: "adapter-codex",
            committer_date: ago(12),
            message: "fix(codex): resume the last session\n\nTER-15.",
            diff: set([file("crates/terrariumd/src/main.rs", "modified", CODEX_PATCH)]),
          },
        ],
      },
    },
  };
}

const ALL = trees();

/** The error shape of the mock's `fail` (./index), without the import cycle. */
function fail(status: number, code: string, message: string): never {
  throw new ApiError(status, message, { error: { code, message } }, code);
}

function treeOf(repo: string, tree: string): MockTree {
  const t = (ALL[repo] ??= {
    main: {
      tree: { branch: "main", head: sha(1), mirror: { fetched_at: ago(1) } },
      files: { "README.md": `# ${repo}\n` },
      changes: { base: sha(1), head: sha(1), base_ref: "refs/heads/main", committed: set([]), uncommitted: null, note: "main is the base" },
      commits: [{ sha: sha(1), parents: [], author_name: "Wil Taylor", author_email: "wil@example.com", date: ago(60), subject: "init", committer_name: "Wil Taylor", committer_date: ago(60), message: "init", diff: set([file("README.md", "added", `@@ -0,0 +1 @@\n+# ${repo}\n`)]) }],
    },
  })[tree];
  if (!t) fail(404, "not_found", `no tree \`${tree}\` for this repo`);
  return t;
}

/** The same path rules as the daemon (repo_browse::clean_path). */
export function cleanPath(p: string): string {
  if (p.includes("\0")) fail(422, "invalid", `bad path \`${p}\`: it has a NUL`);
  if (p.startsWith("/")) fail(422, "invalid", `bad path \`${p}\`: it is absolute`);
  const parts: string[] = [];
  for (const part of p.split("/")) {
    if (part === "" || part === ".") continue;
    if (part === "..") fail(422, "invalid", `bad path \`${p}\`: \`..\` is not allowed`);
    if (part.toLowerCase() === ".git") fail(422, "invalid", `bad path \`${p}\`: \`.git\` is not shown`);
    parts.push(part);
  }
  return parts.join("/");
}

export function mockTrees(repo: string): RepoTree[] {
  treeOf(repo, "main");
  return Object.entries(ALL[repo]!).map(([id, t]) => ({
    id,
    kind: id === "main" ? "main" : "agent",
    label: id === "main" ? "main" : `${t.tree.agent} · ${t.tree.branch}`,
    ...t.tree,
  }));
}

export function mockList(repo: string, tree: string, rawPath: string): TreeListing {
  const path = cleanPath(rawPath);
  const files = treeOf(repo, tree).files;
  const prefix = path ? `${path}/` : "";
  if (path && !Object.keys(files).some((f) => f.startsWith(prefix))) fail(404, "not_found", `no directory \`${path}\` in this tree`);
  const seen = new Map<string, TreeEntry>();
  for (const [f, v] of Object.entries(files)) {
    if (!f.startsWith(prefix)) continue;
    const rest = f.slice(prefix.length);
    const name = rest.split("/")[0]!;
    if (rest.includes("/")) seen.set(name, { name, path: prefix + name, kind: "dir" });
    else if (typeof v === "string") seen.set(name, { name, path: f, kind: "file", size: new TextEncoder().encode(v).length });
    else if ("link" in v) seen.set(name, { name, path: f, kind: "symlink", target: v.link });
    else seen.set(name, { name, path: f, kind: "file", size: Math.floor((v.b64.length * 3) / 4) });
  }
  const entries = [...seen.values()].sort((a, b) => Number(a.kind !== "dir") - Number(b.kind !== "dir") || a.name.localeCompare(b.name));
  return { path, entries, truncated: false };
}

/** The bytes of a file and whether they are text. */
export function mockFile(repo: string, tree: string, rawPath: string): { bytes: Uint8Array; text: boolean } {
  const path = cleanPath(rawPath);
  const v = treeOf(repo, tree).files[path];
  if (v === undefined) fail(404, "not_found", `no file \`${path}\` in this tree`);
  if (typeof v === "string") return { bytes: new TextEncoder().encode(v), text: true };
  if ("link" in v) fail(422, "invalid", `\`${path}\` is a symlink to \`${v.link}\`; the daemon does not follow it`);
  return { bytes: Uint8Array.from(atob(v.b64), (c) => c.charCodeAt(0)), text: false };
}

export function mockChanges(repo: string, tree: string): TreeChanges {
  return { tree, ...treeOf(repo, tree).changes };
}

export function mockCommits(repo: string, tree: string, cursor: string | null | undefined, limit = 5): { items: RepoCommit[]; next: string | null } {
  const all = treeOf(repo, tree).commits;
  const skip = cursor ? Number(cursor) : 0;
  const items = all.slice(skip, skip + limit).map(({ sha, parents, author_name, author_email, date, subject }) => ({ sha, parents, author_name, author_email, date, subject }));
  return { items, next: skip + limit < all.length ? String(skip + limit) : null };
}

export function mockCommit(repo: string, tree: string, sha: string): RepoCommitDetail {
  if (!/^[0-9a-f]{4,64}$/i.test(sha)) fail(422, "invalid", `bad commit \`${sha}\`: 4 to 64 hex digits`);
  const c = treeOf(repo, tree).commits.find((x) => x.sha.startsWith(sha.toLowerCase()));
  if (!c) fail(404, "not_found", `no commit \`${sha}\` in this tree`);
  return c;
}
