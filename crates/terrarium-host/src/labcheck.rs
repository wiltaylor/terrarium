//! The allowlist check of a lab definition that an agent controls, and the
//! lab file of a lab instance (docs/host-protocol.md, "`vmlab.prepare`").
//!
//! A `lab_repo` or `lab_from_workspace` target runs a `vmlab.wcl` that an
//! agent can write. vmlab runs parts of a lab file on the host: provision
//! scripts and event handlers are wscript, and wscript reads and writes host
//! files (`copy_to`, `copy_from`, screenshots) and host environment
//! variables. A lab file also names host paths (shares, volumes, ISOs,
//! folders for disks and media, the `@dev` workspace), host ports
//! (`forward`, `port`), host devices (`gpu`), raw QEMU flags (`qemu_args`),
//! other hosts (`connect`), and segments that other labs share (`global`).
//! `vmlab validate` accepts an absolute or `..` path in `@dev(workspace)`.
//!
//! So the check is an allowlist over the tokens of the file, not a list of
//! bad words. Each block kind has a list of the attributes and the child
//! blocks it may hold. Everything else is refused, and the message names
//! it. A value must be a literal: a string with no `${`, a number with an
//! optional unit, `true` or `false`, a symbol (`:idle`), or a list of
//! those. The only import is `import <vmlab.wcl>`. The only host path is
//! the `workspace` of an `@dev` decorator, and it must be relative and stay
//! in the lab's source directory; [`render`] writes it as the real
//! absolute path. The host agent adds the attached agent workspace itself.
//!
//! The reference for the block kinds is the `vmlab` skill (`lab-file.md`,
//! `vm.md`, `containers.md`, `networking.md`, `dev-machines.md`).

use std::path::{Path, PathBuf};

/// One token, with its byte span in the text.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num,
    Sym,
    Punct(char),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
    line: usize,
}

fn line_of(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].matches('\n').count() + 1
}

fn lex(text: &str) -> Result<Vec<Token>, String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'#' || (c == b'/' && b.get(i + 1) == Some(&b'/')) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let Some(end) = text[i + 2..].find("*/") else {
                return Err(format!(
                    "line {}: a `/*` comment has no end",
                    line_of(text, i)
                ));
            };
            i += 2 + end + 2;
            continue;
        }
        let start = i;
        let line = line_of(text, i);
        let tok = if c == b'"' {
            i += 1;
            let mut val = String::new();
            loop {
                let Some(&ch) = b.get(i) else {
                    return Err(format!("line {line}: a string has no end"));
                };
                match ch {
                    b'"' => {
                        i += 1;
                        break;
                    }
                    b'\\' => {
                        let Some(&e) = b.get(i + 1) else {
                            return Err(format!("line {line}: a string has no end"));
                        };
                        val.push(match e {
                            b'n' => '\n',
                            b't' => '\t',
                            b'\\' => '\\',
                            b'"' => '"',
                            _ => {
                                return Err(format!(
                                    "line {line}: the escape `\\{}` is not allowed",
                                    e as char
                                ));
                            }
                        });
                        i += 2;
                    }
                    b'\n' => return Err(format!("line {line}: a string runs past the line end")),
                    _ => {
                        let ch = text[i..].chars().next().unwrap_or('?');
                        val.push(ch);
                        i += ch.len_utf8();
                    }
                }
            }
            if val.contains("${") {
                return Err(format!(
                    "line {line}: interpolation (`${{`) in a string is not allowed"
                ));
            }
            Tok::Str(val)
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            Tok::Num
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            Tok::Ident(text[start..i].to_owned())
        } else if c == b':' && b.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic()) {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            Tok::Sym
        } else if b"{}[]()=,@<>.".contains(&c) {
            i += 1;
            Tok::Punct(c as char)
        } else {
            let ch = text[i..].chars().next().unwrap_or('?');
            return Err(format!(
                "line {line}: `{ch}` is not allowed in an agent-defined lab file"
            ));
        };
        out.push(Token {
            tok,
            start,
            end: i,
            line,
        });
    }
    Ok(out)
}

/// A string literal: its value and its byte span.
type StrLit = (String, (usize, usize));

/// What a block kind may hold.
struct Spec {
    /// The number of string labels after the kind.
    labels: usize,
    attrs: &'static [&'static str],
    blocks: &'static [&'static str],
}

fn spec(kind: &str) -> Option<Spec> {
    let s = |labels, attrs, blocks| {
        Some(Spec {
            labels,
            attrs,
            blocks,
        })
    };
    match kind {
        "lab" => s(
            1,
            &["agent_update"],
            &["segment", "vm", "container", "record", "sinkhole"],
        ),
        "segment" => s(
            1,
            &["subnet", "dhcp", "nat", "mtu", "routes_to"],
            &["dns", "route", "record", "block", "redirect", "sinkhole"],
        ),
        "vm" => s(
            1,
            &[
                "template",
                "arch",
                "profile",
                "cpus",
                "memory",
                "disk",
                "depends_on",
                "nested",
                "agent_update",
                "display",
                "cpu_model",
                "accel",
                "firmware",
                "tpm",
                "secure_boot",
            ],
            &["nic", "disk", "login"],
        ),
        "container" => s(
            1,
            &[
                "image",
                "mode",
                "entrypoint",
                "command",
                "workdir",
                "user",
                "profile",
                "cpus",
                "memory",
                "depends_on",
            ],
            &["nic", "env", "login", "healthcheck", "volume"],
        ),
        "nic" => s(
            0,
            &["segment", "nat", "ip", "gateway", "mac", "isolated"],
            &[],
        ),
        // `disk "name" { size }`: a blank disk. `from` names a host folder.
        "disk" => s(1, &["size"], &[]),
        // A named volume only. `host` names a host directory.
        "volume" => s(0, &["name", "target", "read_only"], &[]),
        "env" => s(0, &["name", "value"], &[]),
        "login" => s(1, &["user", "password", "elevated", "default"], &[]),
        "healthcheck" => s(
            0,
            &["command", "interval", "timeout", "retries", "start_period"],
            &[],
        ),
        "record" => s(0, &["name", "ip"], &[]),
        "sinkhole" => s(0, &["pattern", "mode"], &[]),
        "dns" => s(0, &["server", "enabled"], &[]),
        "route" => s(0, &["dest", "via"], &[]),
        "block" => s(0, &["cidr", "proto", "port"], &[]),
        "redirect" => s(0, &["from", "to", "proto"], &[]),
        _ => None,
    }
}

/// Why a known host-reaching feature is refused.
fn reason(name: &str) -> Option<&'static str> {
    Some(match name {
        "provision" | "on" => {
            "it runs a wscript on the host, and wscript reads and writes host files and reads host environment variables"
        }
        "playbook" => "it reads a playbook folder on the host",
        "template" => "a template block builds from host files and URLs",
        "share" => "it mounts a host directory",
        "media" => "it builds an image from a host folder",
        "from" => "it copies a host folder",
        "host" => "it binds a host directory",
        "cdrom" | "floppy" => "it attaches a host file",
        "qemu_args" => "raw QEMU flags reach any host file or device",
        "gpu" => "it hands a host device to the guest",
        "forward" | "port" => "it listens on a host port",
        "connect" => "it bridges the segment to another host",
        "global" => "a global segment is shared with other labs",
        "gui" => "it opens a viewer on the host desktop",
        _ => return None,
    })
}

/// An `@dev` decorator of a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dev {
    /// The byte span of the decorator, from `@` to `)` (or the name).
    pub span: (usize, usize),
    pub default: Option<bool>,
    /// The `workspace` value and the span of its string literal.
    pub workspace: Option<(String, (usize, usize))>,
    pub workspace_guest: Option<String>,
}

/// A `vm` or `container` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub kind: String,
    pub name: String,
    /// The byte offset of the kind keyword.
    pub start: usize,
    /// The byte offset of the start of its line.
    pub line_start: usize,
    pub dev: Option<Dev>,
}

/// A lab file that passed the allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabFile {
    pub name: String,
    pub machines: Vec<Machine>,
}

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Token>,
    i: usize,
    errors: Vec<String>,
    lab: Option<String>,
    machines: Vec<Machine>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.i)
    }

    fn peek_at(&self, n: usize) -> Option<&Token> {
        self.toks.get(self.i + n)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.toks.get(self.i).cloned();
        self.i += 1;
        t
    }

    fn line(&self) -> usize {
        self.peek()
            .or_else(|| self.toks.last())
            .map_or(1, |t| t.line)
    }

    fn expect(&mut self, c: char) -> Result<Token, String> {
        match self.next() {
            Some(t) if t.tok == Tok::Punct(c) => Ok(t),
            Some(t) => Err(format!("line {}: expected `{c}`", t.line)),
            None => Err(format!("expected `{c}` at the end of the file")),
        }
    }

    fn import(&mut self) -> Result<(), String> {
        let want = [
            Tok::Ident("import".into()),
            Tok::Punct('<'),
            Tok::Ident("vmlab".into()),
            Tok::Punct('.'),
            Tok::Ident("wcl".into()),
            Tok::Punct('>'),
        ];
        for w in want {
            match self.next() {
                Some(t) if t.tok == w => {}
                _ => {
                    return Err(
                        "the file must start with `import <vmlab.wcl>`, and no other import is allowed"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }

    /// A literal value. Returns the string of a string literal and its span.
    fn value(&mut self, depth: usize) -> Result<Option<StrLit>, String> {
        let Some(t) = self.next() else {
            return Err("a value is missing at the end of the file".into());
        };
        match t.tok {
            Tok::Str(s) => Ok(Some((s, (t.start, t.end)))),
            Tok::Num | Tok::Sym => Ok(None),
            Tok::Ident(ref w) if w == "true" || w == "false" => Ok(None),
            Tok::Punct('[') if depth == 0 => {
                loop {
                    if self.peek().is_some_and(|t| t.tok == Tok::Punct(']')) {
                        self.next();
                        break;
                    }
                    self.value(depth + 1)?;
                    match self.next() {
                        Some(t) if t.tok == Tok::Punct(',') => {}
                        Some(t) if t.tok == Tok::Punct(']') => break,
                        _ => return Err(format!("line {}: a list is not closed", t.line)),
                    }
                }
                Ok(None)
            }
            _ => Err(format!(
                "line {}: only literal values are allowed (a string, a number, true or false, a symbol, or a list)",
                t.line
            )),
        }
    }

    fn refuse_name(&mut self, line: usize, what: &str, name: &str, inside: &str) {
        let msg = match reason(name) {
            Some(r) => format!("line {line}: {what} `{name}` in `{inside}` is refused: {r}"),
            None => format!(
                "line {line}: {what} `{name}` in `{inside}` is not in the allowlist of agent-defined labs"
            ),
        };
        self.errors.push(msg);
    }

    /// Skip a block body that is refused, so the check goes on.
    fn skip_block(&mut self) -> Result<(), String> {
        while self.peek().is_some_and(|t| t.tok != Tok::Punct('{')) {
            self.next();
        }
        self.expect('{')?;
        let mut depth = 1;
        while depth > 0 {
            match self.next() {
                Some(t) if t.tok == Tok::Punct('{') => depth += 1,
                Some(t) if t.tok == Tok::Punct('}') => depth -= 1,
                Some(_) => {}
                None => return Err("a block is not closed".into()),
            }
        }
        Ok(())
    }

    fn decorator(&mut self, parent: &str) -> Result<Option<Dev>, String> {
        let at = self.expect('@')?;
        let name = match self.next() {
            Some(Token {
                tok: Tok::Ident(n), ..
            }) => n,
            _ => return Err(format!("line {}: a decorator needs a name", at.line)),
        };
        let mut end = self.toks[self.i - 1].end;
        if name != "dev" {
            self.errors.push(format!(
                "line {}: the decorator `@{name}` is not in the allowlist",
                at.line
            ));
        }
        if parent != "lab" {
            self.errors.push(format!(
                "line {}: `@{name}` is only allowed on a machine in `lab`",
                at.line
            ));
        }
        let mut dev = Dev {
            span: (at.start, end),
            default: None,
            workspace: None,
            workspace_guest: None,
        };
        if self.peek().is_some_and(|t| t.tok == Tok::Punct('(')) {
            self.next();
            loop {
                if self.peek().is_some_and(|t| t.tok == Tok::Punct(')')) {
                    end = self.next().map_or(end, |t| t.end);
                    break;
                }
                let arg = match self.next() {
                    Some(Token {
                        tok: Tok::Ident(n),
                        line,
                        ..
                    }) => (n, line),
                    _ => {
                        return Err(format!(
                            "line {}: a decorator argument needs a name",
                            at.line
                        ));
                    }
                };
                self.expect('=')?;
                let vline = self.line();
                let is_bool = matches!(
                    self.peek().map(|t| &t.tok),
                    Some(Tok::Ident(w)) if w == "true" || w == "false"
                );
                let bool_val =
                    matches!(self.peek().map(|t| &t.tok), Some(Tok::Ident(w)) if w == "true");
                let v = self.value(0)?;
                match (arg.0.as_str(), v) {
                    ("default", None) if is_bool => dev.default = Some(bool_val),
                    ("workspace", Some((s, span))) => {
                        if let Err(e) = check_rel(&s) {
                            self.errors
                                .push(format!("line {vline}: `@dev(workspace)` {e}"));
                        }
                        dev.workspace = Some((s, span));
                    }
                    ("workspace_guest", Some((s, _))) => {
                        if s.contains('"') || s.contains('\\') || s.contains('$') {
                            self.errors.push(format!(
                                "line {vline}: `workspace_guest` has a character that is not allowed"
                            ));
                        }
                        dev.workspace_guest = Some(s);
                    }
                    (other, _) => self.errors.push(format!(
                        "line {}: the `@dev` argument `{other}` is not allowed",
                        arg.1
                    )),
                }
                match self.next() {
                    Some(t) if t.tok == Tok::Punct(',') => {}
                    Some(t) if t.tok == Tok::Punct(')') => {
                        end = t.end;
                        break;
                    }
                    _ => return Err(format!("line {}: a decorator is not closed", at.line)),
                }
            }
        }
        dev.span = (at.start, end);
        Ok(Some(dev))
    }

    /// One block whose kind token is next. `parent` is the kind of the
    /// enclosing block (`""` at the top).
    fn block(&mut self, parent: &str, dev: Option<Dev>) -> Result<(), String> {
        let Some(kind_tok) = self.next() else {
            return Err("a block is missing at the end of the file".into());
        };
        let Tok::Ident(kind) = kind_tok.tok.clone() else {
            return Err(format!("line {}: expected a block", kind_tok.line));
        };
        let allowed_here = match parent {
            "" => kind == "lab",
            p => spec(p).is_some_and(|s| s.blocks.contains(&kind.as_str())),
        };
        let Some(sp) = spec(&kind).filter(|_| allowed_here) else {
            let inside = if parent.is_empty() {
                "the file"
            } else {
                parent
            };
            self.refuse_name(kind_tok.line, "the block", &kind, inside);
            return self.skip_block();
        };
        if dev.is_some() && !matches!(kind.as_str(), "vm" | "container") {
            self.errors.push(format!(
                "line {}: `@dev` is only allowed on a `vm` or a `container`",
                kind_tok.line
            ));
        }
        let mut labels = Vec::new();
        while let Some(Token {
            tok: Tok::Str(s), ..
        }) = self.peek().cloned()
        {
            self.next();
            labels.push(s);
        }
        if labels.len() != sp.labels {
            self.errors.push(format!(
                "line {}: `{kind}` takes {} label(s), not {}",
                kind_tok.line,
                sp.labels,
                labels.len()
            ));
        }
        if kind == "lab" {
            if self.lab.is_some() {
                self.errors
                    .push(format!("line {}: a second `lab` block", kind_tok.line));
            }
            self.lab = labels.first().cloned();
        }
        if matches!(kind.as_str(), "vm" | "container") {
            let line_start = self.text[..kind_tok.start].rfind('\n').map_or(0, |n| n + 1);
            self.machines.push(Machine {
                kind: kind.clone(),
                name: labels.first().cloned().unwrap_or_default(),
                start: dev.as_ref().map_or(kind_tok.start, |d| d.span.0),
                line_start,
                dev,
            });
        }
        self.expect('{')?;
        loop {
            let Some(t) = self.peek().cloned() else {
                return Err(format!("line {}: `{kind}` is not closed", kind_tok.line));
            };
            match &t.tok {
                Tok::Punct('}') => {
                    self.next();
                    return Ok(());
                }
                Tok::Punct('@') => {
                    let d = self.decorator(&kind)?;
                    self.block(&kind, d)?;
                }
                Tok::Ident(name) => {
                    let name = name.clone();
                    let is_attr = self.peek_at(1).is_some_and(|n| n.tok == Tok::Punct('='));
                    if is_attr {
                        self.next();
                        self.next();
                        if !sp.attrs.contains(&name.as_str()) {
                            self.refuse_name(t.line, "the attribute", &name, &kind);
                        }
                        self.value(0)?;
                    } else {
                        self.block(&kind, None)?;
                    }
                }
                _ => {
                    return Err(format!(
                        "line {}: expected an attribute or a block in `{kind}`",
                        t.line
                    ));
                }
            }
        }
    }
}

/// A relative path that stays in its directory: no `/` at the start, no
/// `~`, no `..`, and no `"`, `\`, `$`, or control characters.
fn check_rel(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("is empty".into());
    }
    if p.starts_with('/') || p.starts_with('~') {
        return Err(format!(
            "`{p}` must be a relative path in the lab directory"
        ));
    }
    if p.split('/').any(|s| s == "..") {
        return Err(format!("`{p}` must not leave the lab directory (`..`)"));
    }
    if p.chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\\' | '$'))
    {
        return Err(format!("`{p}` has a character that is not allowed"));
    }
    Ok(())
}

/// Check a lab file against the allowlist. All the problems come back at
/// once.
pub fn check(text: &str) -> Result<LabFile, Vec<String>> {
    let toks = lex(text).map_err(|e| vec![e])?;
    let mut p = Parser {
        text,
        toks,
        i: 0,
        errors: Vec::new(),
        lab: None,
        machines: Vec::new(),
    };
    let run = (|| -> Result<(), String> {
        p.import()?;
        while let Some(t) = p.peek().cloned() {
            if t.tok == Tok::Punct('@') {
                let d = p.decorator("")?;
                p.block("", d)?;
            } else {
                p.block("", None)?;
            }
        }
        Ok(())
    })();
    if let Err(e) = run {
        p.errors.push(e);
    }
    if p.lab.is_none() && p.errors.is_empty() {
        p.errors.push("the file has no `lab` block".into());
    }
    if p.errors.is_empty() {
        Ok(LabFile {
            name: p.lab.unwrap_or_default(),
            machines: p.machines,
        })
    } else {
        Err(p.errors)
    }
}

/// The workspace to attach to one machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attach {
    pub machine: String,
    /// The real host path of the workspace.
    pub host: PathBuf,
    /// The guest path, `/work/<project>/<agent>`.
    pub guest: String,
}

/// What [`render`] did for the attached machine.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DevInfo {
    pub machine: String,
    /// The host directory that syncs into the guest.
    pub workspace: String,
    /// The guest path, when the lab file or the attach names one.
    pub guest: Option<String>,
    /// The lab file declares the workspace itself (`@dev(workspace)`).
    pub own: bool,
}

fn quote(p: &Path) -> Result<String, String> {
    let s = p
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", p.display()))?;
    if s.chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\\' | '$'))
    {
        return Err(format!("{s} has a character that a lab file cannot hold"));
    }
    Ok(format!("\"{s}\""))
}

fn quote_guest(g: &str) -> Result<String, String> {
    if g.chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\\' | '$'))
    {
        return Err(format!("{g} has a character that a lab file cannot hold"));
    }
    Ok(format!("\"{g}\""))
}

/// The lab file of an instance: `text` with each `@dev(workspace)` written
/// as its real absolute path under `source_dir` (refused when it leads out
/// of it), and `attach` added as an `@dev` decorator on its machine. When
/// that machine declares its own `@dev(workspace)`, the lab's own mount
/// stays and `attach` adds nothing.
pub fn render(
    text: &str,
    lab: &LabFile,
    source_dir: &Path,
    attach: Option<&Attach>,
) -> Result<(String, Option<DevInfo>), String> {
    let root = std::fs::canonicalize(source_dir)
        .map_err(|e| format!("the lab directory {}: {e}", source_dir.display()))?;
    // Edits as (start, end, replacement), applied from the end.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut info = None;
    for m in &lab.machines {
        if let Some((rel, span)) = m.dev.as_ref().and_then(|d| d.workspace.as_ref()) {
            check_rel(rel)?;
            let real = std::fs::canonicalize(root.join(rel)).map_err(|e| {
                format!(
                    "the `@dev(workspace)` `{rel}` of `{}` does not exist in the lab directory: {e}",
                    m.name
                )
            })?;
            if !real.starts_with(&root) || !real.is_dir() {
                return Err(format!(
                    "the `@dev(workspace)` `{rel}` of `{}` leads out of the lab directory",
                    m.name
                ));
            }
            edits.push((span.0, span.1, quote(&real)?));
            if attach.is_some_and(|a| a.machine == m.name) {
                info = Some(DevInfo {
                    machine: m.name.clone(),
                    workspace: real.display().to_string(),
                    guest: m.dev.as_ref().and_then(|d| d.workspace_guest.clone()),
                    own: true,
                });
            }
        }
    }
    if let Some(a) = attach
        && info.is_none()
    {
        let m = lab
            .machines
            .iter()
            .find(|m| m.name == a.machine)
            .ok_or_else(|| format!("the lab has no machine `{}`", a.machine))?;
        let mut args = Vec::new();
        if let Some(true) = m.dev.as_ref().and_then(|d| d.default) {
            args.push("default = true".to_owned());
        }
        args.push(format!("workspace = {}", quote(&a.host)?));
        args.push(format!("workspace_guest = {}", quote_guest(&a.guest)?));
        let deco = format!("@dev({})", args.join(", "));
        match &m.dev {
            Some(d) => edits.push((d.span.0, d.span.1, deco)),
            None => {
                let indent: String = text[m.line_start..m.start]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect();
                edits.push((m.start, m.start, format!("{deco}\n{indent}")));
            }
        }
        info = Some(DevInfo {
            machine: m.name.clone(),
            workspace: a.host.display().to_string(),
            guest: Some(a.guest.clone()),
            own: false,
        });
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = text.to_owned();
    for (s, e, r) in edits {
        out.replace_range(s..e, &r);
    }
    Ok((out, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOX: &str = r#"# a lab
import <vmlab.wcl>

lab "alpine-box" {
  container "box" {
    image   = "alpine:3.22"
    profile = "container"
    // idle
    mode    = :idle
    cpus = 1
    memory = 256MiB
    env { name = "A" value = "b" }
  }
}
"#;

    #[test]
    fn the_test_lab_passes() {
        let lab = check(BOX).unwrap();
        assert_eq!(lab.name, "alpine-box");
        assert_eq!(lab.machines.len(), 1);
        assert_eq!(lab.machines[0].name, "box");
        assert_eq!(lab.machines[0].kind, "container");
    }

    fn refused(text: &str) -> String {
        check(text).unwrap_err().join("\n")
    }

    fn with(inner: &str) -> String {
        format!("import <vmlab.wcl>\nlab \"l\" {{\n{inner}\n}}\n")
    }

    #[test]
    fn host_reaching_features_are_refused() {
        let cases = [
            (
                r#"vm "v" { template = "x86_64/u" share { host = "/etc" guest = "/mnt" } }"#,
                "mounts a host directory",
            ),
            (
                r#"container "c" { image = "a" volume { host = "/home" target = "/x" } }"#,
                "binds a host directory",
            ),
            (
                r#"container "c" { image = "a" provision "s.ws" { } }"#,
                "wscript",
            ),
            (r#"on "vm.crashed" { run = "x.ws" }"#, "wscript"),
            (
                r#"vm "v" { template = "t" qemu_args = ["-drive", "file=/etc/shadow"] }"#,
                "raw QEMU",
            ),
            (
                r#"vm "v" { template = "t" cdrom = "/isos/x.iso" }"#,
                "host file",
            ),
            (
                r#"vm "v" { template = "t" disk "d" { from = "./payload" } }"#,
                "host folder",
            ),
            (
                r#"vm "v" { template = "t" gpu { mode = "passthrough" address = "0000:01:00.0" } }"#,
                "host device",
            ),
            (
                r#"segment "s" { forward { host_port = 22 to = "v:22" } }"#,
                "host port",
            ),
            (
                r#"container "c" { image = "a" port { host = 80 container = 80 } }"#,
                "host port",
            ),
            (r#"segment "s" { global = true }"#, "other labs"),
            (
                r#"segment "s" { connect { host = "h:1" } }"#,
                "another host",
            ),
            (r#"gui = true"#, "desktop"),
            (
                r#"vm "v" { template = "t" media { kind = "iso" from = "./u" } }"#,
                "host folder",
            ),
            (
                r#"vm "v" { template = "t" playbook "p" { play = "x" } }"#,
                "playbook",
            ),
            (
                r#"vm "v" { template = "t" frobnicate = 1 }"#,
                "not in the allowlist",
            ),
            (r#"vm "v" { template = other.thing }"#, "literal values"),
            (r#"vm "v" { template = "${env}" }"#, "interpolation"),
        ];
        for (inner, want) in cases {
            let e = refused(&with(inner));
            assert!(e.contains(want), "{inner}: {e}");
        }
        let e = refused(&format!("{}\ntemplate \"t\" {{ }}\n", with("")));
        assert!(e.contains("template"), "{e}");
        let e = refused("import \"./other.wcl\"\nlab \"l\" { }\n");
        assert!(e.contains("import <vmlab.wcl>"), "{e}");
        let e = refused("lab \"l\" { }\n");
        assert!(e.contains("import <vmlab.wcl>"), "{e}");
    }

    #[test]
    fn dev_workspace_paths_stay_in_the_lab_dir() {
        for (p, want) in [
            ("/home/alice", "relative"),
            ("../../etc", ".."),
            ("~/x", "relative"),
        ] {
            let t = with(&format!(
                "@dev(workspace = \"{p}\")\ncontainer \"c\" {{ image = \"a\" }}"
            ));
            let e = refused(&t);
            assert!(e.contains(want), "{p}: {e}");
        }
        let e = refused(&with("@evil\ncontainer \"c\" { image = \"a\" }"));
        assert!(e.contains("@evil"), "{e}");
        assert!(
            check(&with(
                "@dev(workspace = \"src\", default = true)\ncontainer \"c\" { image = \"a\" }"
            ))
            .is_ok()
        );
    }

    #[test]
    fn render_adds_the_attach_decorator() {
        let dir = tempfile::tempdir().unwrap();
        let lab = check(BOX).unwrap();
        let a = Attach {
            machine: "box".into(),
            host: PathBuf::from("/srv/work/terra/w1"),
            guest: "/work/terra/w1".into(),
        };
        let (out, info) = render(BOX, &lab, dir.path(), Some(&a)).unwrap();
        assert!(
            out.contains(
                "  @dev(workspace = \"/srv/work/terra/w1\", workspace_guest = \"/work/terra/w1\")\n  container \"box\" {"
            ),
            "{out}"
        );
        let info = info.unwrap();
        assert!(!info.own);
        assert_eq!(info.guest.as_deref(), Some("/work/terra/w1"));
        // The output passes the allowlist too, apart from its absolute path.
        assert!(check(&out).is_err());
        let none = render(BOX, &lab, dir.path(), None).unwrap();
        assert_eq!(none.0, BOX);
        let bad = Attach {
            machine: "nope".into(),
            ..a
        };
        assert!(render(BOX, &lab, dir.path(), Some(&bad)).is_err());
    }

    #[test]
    fn render_keeps_the_labs_own_mount() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::os::unix::fs::symlink("/etc", dir.path().join("out")).unwrap();
        let text = with(
            "@dev(workspace = \"src\", workspace_guest = \"/src\")\ncontainer \"c\" { image = \"a\" }",
        );
        let lab = check(&text).unwrap();
        let a = Attach {
            machine: "c".into(),
            host: PathBuf::from("/w/p/a"),
            guest: "/work/p/a".into(),
        };
        let (out, info) = render(&text, &lab, dir.path(), Some(&a)).unwrap();
        let real = std::fs::canonicalize(dir.path().join("src")).unwrap();
        assert!(
            out.contains(&format!("workspace = \"{}\"", real.display())),
            "{out}"
        );
        assert!(!out.contains("/w/p/a"), "{out}");
        let info = info.unwrap();
        assert!(info.own);
        assert_eq!(info.guest.as_deref(), Some("/src"));
        // A link that leads out of the lab dir is refused.
        let text = with("@dev(workspace = \"out\")\ncontainer \"c\" { image = \"a\" }");
        let lab = check(&text).unwrap();
        let e = render(&text, &lab, dir.path(), None).unwrap_err();
        assert!(e.contains("leads out"), "{e}");
        // A bare `@dev` is replaced, and keeps `default`.
        let text = with("@dev(default = true)\ncontainer \"c\" { image = \"a\" }");
        let lab = check(&text).unwrap();
        let (out, _) = render(&text, &lab, dir.path(), Some(&a)).unwrap();
        assert!(
            out.contains("@dev(default = true, workspace = \"/w/p/a\", workspace_guest = \"/work/p/a\")\ncontainer"),
            "{out}"
        );
    }
}
