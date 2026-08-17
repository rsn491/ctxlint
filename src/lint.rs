//! Applies ctxlint's rules to agent instruction files.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::discover::{Kind, Target};
use crate::parse::{self, ErrKind, Frontmatter, Value};
use crate::tokens::{self, Counter};

/// Marks how a finding affects the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
        }
    }
}

// Rule ids, usable with Config::disabled.
pub const RULE_FRONTMATTER_MISSING: &str = "frontmatter.missing";
pub const RULE_FRONTMATTER_NOT_FIRST: &str = "frontmatter.not-first";
pub const RULE_FRONTMATTER_UNTERMINATED: &str = "frontmatter.unterminated";
pub const RULE_FRONTMATTER_INVALID: &str = "frontmatter.invalid";
pub const RULE_FRONTMATTER_UNKNOWN_KEY: &str = "frontmatter.unknown-key";
pub const RULE_NAME_REQUIRED: &str = "name.required";
pub const RULE_NAME_FORMAT: &str = "name.format";
pub const RULE_NAME_LENGTH: &str = "name.length";
pub const RULE_NAME_DIR_MISMATCH: &str = "name.dir-mismatch";
pub const RULE_DESCRIPTION_REQUIRED: &str = "description.required";
pub const RULE_DESCRIPTION_LENGTH: &str = "description.length";
pub const RULE_ALLOWED_TOOLS_TYPE: &str = "allowed-tools.type";
pub const RULE_METADATA_TYPE: &str = "metadata.type";
pub const RULE_TOKENS_CONTENT: &str = "tokens.content";
pub const RULE_TOKENS_NAME: &str = "tokens.name";
pub const RULE_TOKENS_DESCRIPTION: &str = "tokens.description";
pub const RULE_FILE_REFERENCE_MISSING: &str = "file-reference.missing";

/// Every rule id ctxlint can emit, in report order.
pub const RULES: &[&str] = &[
    RULE_FRONTMATTER_MISSING,
    RULE_FRONTMATTER_NOT_FIRST,
    RULE_FRONTMATTER_UNTERMINATED,
    RULE_FRONTMATTER_INVALID,
    RULE_FRONTMATTER_UNKNOWN_KEY,
    RULE_NAME_REQUIRED,
    RULE_NAME_FORMAT,
    RULE_NAME_LENGTH,
    RULE_NAME_DIR_MISMATCH,
    RULE_DESCRIPTION_REQUIRED,
    RULE_DESCRIPTION_LENGTH,
    RULE_ALLOWED_TOOLS_TYPE,
    RULE_METADATA_TYPE,
    RULE_TOKENS_CONTENT,
    RULE_TOKENS_NAME,
    RULE_TOKENS_DESCRIPTION,
    RULE_FILE_REFERENCE_MISSING,
];

static RULE_ORDER: LazyLock<HashMap<&'static str, usize>> =
    LazyLock::new(|| RULES.iter().enumerate().map(|(i, r)| (*r, i)).collect());

/// Limits from the Anthropic skill spec, in characters.
pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Default token budgets. `Config::default` and the CLI's flag defaults both
/// read these, so the two cannot drift apart.
pub const DEFAULT_MAX_AGENTS_TOKENS: i64 = 2500;
pub const DEFAULT_MAX_SKILL_TOKENS: i64 = 5000;
pub const DEFAULT_MAX_SKILL_NAME_TOKENS: i64 = 16;
pub const DEFAULT_MAX_SKILL_DESCRIPTION_TOKENS: i64 = 100;

/// The front-matter keys the skill spec defines, plus the Claude Code
/// extensions ctxlint additionally supports. Anything else is reported as an
/// unknown key.
static KNOWN_KEYS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "name",
        "description",
        "license",
        "compatibility",
        "metadata",
        "allowed-tools",
        "disable-model-invocation",
        "argument-hint",
    ]
    .into_iter()
    .collect()
});

/// The spec's naming rule: lowercase alphanumerics in hyphen-separated
/// segments.
static NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*$").unwrap());

/// Matches inline markdown links and images: `[text](target)` or
/// `![alt](target)`. Does not handle reference-style links (`[text][ref]`).
static LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!?\[[^\]]*\]\(([^)]+)\)").unwrap());

/// Matches inline code spans: `` `text` ``. Fenced code blocks are excluded
/// separately by the caller's in_fence tracking.
static CODE_SPAN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());

/// Detects an absolute URI (`https://`, `mailto:`, `tel:`, and the like) so
/// those targets are left for a browser rather than checked as files.
static URI_SCHEME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9+.-]*:").unwrap());

/// Holds the thresholds and switches that shape a run.
#[derive(Debug, Clone)]
pub struct Config {
    /// Body token budgets, one per file kind. Zero disables the check for
    /// that kind.
    pub max_agents_tokens: i64,
    pub max_skill_tokens: i64,
    /// Skill-only budgets. Zero disables the check.
    pub max_skill_name_tokens: i64,
    pub max_skill_description_tokens: i64,
    /// Rule ids to skip.
    pub disabled: Vec<String>,
    /// Treat warnings as errors.
    pub strict: bool,
}

/// Zero means "check disabled", so a derived `Default` would hand back a
/// linter that silently enforces nothing. Spell the real budgets out instead,
/// so the value reached by accident is the safe one.
impl Default for Config {
    fn default() -> Self {
        Config {
            max_agents_tokens: DEFAULT_MAX_AGENTS_TOKENS,
            max_skill_tokens: DEFAULT_MAX_SKILL_TOKENS,
            max_skill_name_tokens: DEFAULT_MAX_SKILL_NAME_TOKENS,
            max_skill_description_tokens: DEFAULT_MAX_SKILL_DESCRIPTION_TOKENS,
            disabled: Vec::new(),
            strict: false,
        }
    }
}

impl Config {
    fn content_limit(&self, kind: Kind) -> i64 {
        if kind == Kind::Agents {
            self.max_agents_tokens
        } else {
            self.max_skill_tokens
        }
    }
}

/// A single rule violation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub file: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub line: usize,
    pub rule: String,
    pub severity: Severity,
    pub message: String,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The measured token cost of a file's parts.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Counts {
    pub content: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub name: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub description: usize,
}

/// Everything ctxlint learned about one file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileResult {
    pub path: String,
    pub kind: Kind,
    pub tokens: Counts,
    pub findings: Vec<Finding>,
}

impl FileResult {
    pub fn errors(&self) -> usize {
        self.count(Severity::Error)
    }

    pub fn warnings(&self) -> usize {
        self.count(Severity::Warning)
    }

    fn count(&self, s: Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == s).count()
    }
}

/// Collects findings, applying `--disable` and `--strict`.
struct Reporter<'a> {
    findings: &'a mut Vec<Finding>,
    disabled: &'a HashSet<String>,
    strict: bool,
    path: &'a str,
}

impl Reporter<'_> {
    fn add(&mut self, rule: &str, mut sev: Severity, line: usize, msg: String) {
        if self.disabled.contains(rule) {
            return;
        }
        if sev == Severity::Warning && self.strict {
            sev = Severity::Error;
        }
        self.findings.push(Finding {
            file: self.path.to_string(),
            line,
            rule: rule.to_string(),
            severity: sev,
            message: msg,
        });
    }
}

/// Applies [`Config`]'s rules using a token [`Counter`].
pub struct Linter {
    cfg: Config,
    counter: Box<dyn Counter>,
    disabled: HashSet<String>,
    /// Resolved once, at construction. Rules read this instead of calling
    /// `std::env::current_dir` themselves, so a rule's verdict cannot depend
    /// on where the process happens to be when it runs.
    cwd: PathBuf,
}

impl Linter {
    /// Returns a Linter anchored to the process's working directory. `None`
    /// for `counter` falls back to the heuristic estimator.
    pub fn new(cfg: Config, counter: Option<Box<dyn Counter>>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        Self::with_cwd(cfg, counter, cwd)
    }

    /// Returns a Linter that treats `cwd` as the working directory, so the one
    /// rule that cares can be tested without mutating process-global state
    /// while other tests run alongside it.
    pub fn with_cwd(cfg: Config, counter: Option<Box<dyn Counter>>, cwd: PathBuf) -> Self {
        let counter = counter.unwrap_or_else(|| Box::new(tokens::Estimator::new()));
        let disabled = cfg.disabled.iter().cloned().collect();
        Linter {
            cfg,
            counter,
            disabled,
            cwd,
        }
    }

    /// Reads and lints one target.
    pub fn file(&self, t: &Target) -> Result<FileResult, String> {
        let src = std::fs::read(&t.path).map_err(|e| format!("cannot read {}: {e}", t.path))?;
        Ok(self.check(t, &src))
    }

    /// Lints already-read file contents. Findings are ordered by rule so
    /// output does not depend on the order the checks happen to run in.
    pub fn check(&self, t: &Target, src: &[u8]) -> FileResult {
        let (fm, body, err) = parse::split(src);
        let mut tokens = Counts {
            content: self.counter.count(&body),
            name: 0,
            description: 0,
        };
        let mut findings = Vec::new();

        {
            let mut r = Reporter {
                findings: &mut findings,
                disabled: &self.disabled,
                strict: self.cfg.strict,
                path: &t.path,
            };

            if t.kind == Kind::Skill {
                self.check_skill(t, &fm, err.as_ref(), &mut r, &mut tokens);
            } else if let Some(e) = &err
                && e.kind == ErrKind::Unterminated
            {
                r.add(
                    RULE_FRONTMATTER_UNTERMINATED,
                    Severity::Error,
                    e.line,
                    e.msg.clone(),
                );
            }

            self.check_file_references(t, &fm, &body, &mut r);

            let limit = self.cfg.content_limit(t.kind);
            if limit > 0 && tokens.content as i64 > limit {
                r.add(
                    RULE_TOKENS_CONTENT,
                    Severity::Error,
                    0,
                    format!(
                        "content is {} tokens, over the {} token limit",
                        humanize(tokens.content as i64),
                        humanize(limit)
                    ),
                );
            }
        }

        sort_findings(&mut findings);
        FileResult {
            path: t.path.clone(),
            kind: t.kind,
            tokens,
            findings,
        }
    }

    fn check_skill(
        &self,
        t: &Target,
        fm: &Frontmatter,
        err: Option<&parse::Error>,
        r: &mut Reporter,
        tokens: &mut Counts,
    ) {
        if let Some(e) = err {
            match e.kind {
                ErrKind::NotFirst => r.add(
                    RULE_FRONTMATTER_NOT_FIRST,
                    Severity::Error,
                    e.line,
                    e.msg.clone(),
                ),
                ErrKind::Unterminated => r.add(
                    RULE_FRONTMATTER_UNTERMINATED,
                    Severity::Error,
                    e.line,
                    e.msg.clone(),
                ),
                _ => r.add(
                    RULE_FRONTMATTER_INVALID,
                    Severity::Error,
                    e.line,
                    e.msg.clone(),
                ),
            }
            return;
        }
        if !fm.present {
            r.add(
                RULE_FRONTMATTER_MISSING,
                Severity::Error,
                1,
                "missing YAML front matter: a skill must open with a --- fenced block declaring name and description".to_string(),
            );
            return;
        }

        for key in fm.keys() {
            if !KNOWN_KEYS.contains(key.as_str()) {
                r.add(
                    RULE_FRONTMATTER_UNKNOWN_KEY,
                    Severity::Warning,
                    fm.line(key),
                    format!("unknown front-matter key {key:?}"),
                );
            }
        }

        self.check_name(t, fm, r, tokens);
        self.check_description(fm, r, tokens);
        self.check_allowed_tools(fm, r);
        self.check_metadata(fm, r);
    }

    fn check_name(&self, t: &Target, fm: &Frontmatter, r: &mut Reporter, tokens: &mut Counts) {
        let raw = fm.string("name");
        let name = raw.as_deref().unwrap_or("").trim().to_string();
        if raw.is_none() || name.is_empty() {
            r.add(
                RULE_NAME_REQUIRED,
                Severity::Error,
                fm.line("name"),
                "front matter must declare a non-empty string name".to_string(),
            );
            return;
        }
        tokens.name = self.counter.count(&name);
        let line = fm.line("name");

        if !NAME_RE.is_match(&name) {
            r.add(
                RULE_NAME_FORMAT,
                Severity::Error,
                line,
                format!(
                    "name {name:?} must be lowercase letters, digits and single hyphens (for example my-skill)"
                ),
            );
        }
        let n = name.chars().count();
        if n > MAX_NAME_CHARS {
            r.add(
                RULE_NAME_LENGTH,
                Severity::Error,
                line,
                format!("name is {n} characters, over the {MAX_NAME_CHARS} character limit"),
            );
        }
        if let Some(dir) = self.skill_dir(&t.path)
            && dir != name
        {
            r.add(
                RULE_NAME_DIR_MISMATCH,
                Severity::Warning,
                line,
                format!("name {name:?} does not match its directory {dir:?}"),
            );
        }
        if self.cfg.max_skill_name_tokens > 0 && tokens.name as i64 > self.cfg.max_skill_name_tokens
        {
            r.add(
                RULE_TOKENS_NAME,
                Severity::Error,
                line,
                format!(
                    "name is {} tokens, over the {} token limit",
                    humanize(tokens.name as i64),
                    humanize(self.cfg.max_skill_name_tokens)
                ),
            );
        }
    }

    fn check_description(&self, fm: &Frontmatter, r: &mut Reporter, tokens: &mut Counts) {
        let raw = fm.string("description");
        let desc = raw.as_deref().unwrap_or("").trim().to_string();
        if raw.is_none() || desc.is_empty() {
            r.add(
                RULE_DESCRIPTION_REQUIRED,
                Severity::Error,
                fm.line("description"),
                "front matter must declare a non-empty string description saying when to use the skill".to_string(),
            );
            return;
        }
        tokens.description = self.counter.count(&desc);
        let line = fm.line("description");

        let n = desc.chars().count();
        if n > MAX_DESCRIPTION_CHARS {
            r.add(
                RULE_DESCRIPTION_LENGTH,
                Severity::Error,
                line,
                format!(
                    "description is {} characters, over the {} character limit",
                    humanize(n as i64),
                    humanize(MAX_DESCRIPTION_CHARS as i64)
                ),
            );
        }
        if self.cfg.max_skill_description_tokens > 0
            && tokens.description as i64 > self.cfg.max_skill_description_tokens
        {
            r.add(
                RULE_TOKENS_DESCRIPTION,
                Severity::Error,
                line,
                format!(
                    "description is {} tokens, over the {} token limit",
                    humanize(tokens.description as i64),
                    humanize(self.cfg.max_skill_description_tokens)
                ),
            );
        }
    }

    fn check_allowed_tools(&self, fm: &Frontmatter, r: &mut Reporter) {
        if !fm.has("allowed-tools") {
            return;
        }
        if fm.string_slice("allowed-tools").is_none() {
            r.add(
                RULE_ALLOWED_TOOLS_TYPE,
                Severity::Error,
                fm.line("allowed-tools"),
                "allowed-tools must be a list of tool names or a comma-separated string"
                    .to_string(),
            );
        }
    }

    fn check_metadata(&self, fm: &Frontmatter, r: &mut Reporter) {
        match fm.node("metadata") {
            None | Some(Value::Mapping) => {}
            Some(_) => r.add(
                RULE_METADATA_TYPE,
                Severity::Error,
                fm.line("metadata"),
                "metadata must be a mapping of keys to values".to_string(),
            ),
        }
    }

    /// Flags markdown links and path-shaped inline code spans whose target
    /// looks like a local file but does not resolve to one, relative to the
    /// linted file's directory. Applies to both AGENTS.md and SKILL.md, since
    /// a dangling reference breaks the file's contract with the runtime
    /// regardless of kind.
    ///
    /// References resolving outside the linted tree are left alone, like URLs
    /// and absolute paths: ctxlint often runs in CI over a checkout it does
    /// not trust, and stat-ing whatever path a file names would turn its
    /// markdown into a probe for what exists on the host.
    fn check_file_references(&self, t: &Target, fm: &Frontmatter, body: &str, r: &mut Reporter) {
        let dir = Path::new(&t.path).parent().unwrap_or_else(|| Path::new(""));
        let root = self.abs_path(&t.root);
        let offset = if fm.present { fm.end_line } else { 0 };

        let mut fences = FenceTracker::default();
        for (i, line) in body.split('\n').enumerate() {
            if !fences.scan_line(line) {
                continue;
            }
            let mut targets: Vec<String> = Vec::new();
            for caps in LINK_RE.captures_iter(line) {
                if let Some(target) = link_target_path(&caps[1]) {
                    targets.push(target);
                }
            }
            for caps in CODE_SPAN_RE.captures_iter(line) {
                if let Some(target) = code_span_target_path(&caps[1])
                    && !targets.contains(&target)
                {
                    targets.push(target);
                }
            }
            for target in targets {
                // Resolved lexically, so deciding whether a reference escapes
                // the tree costs no filesystem access of its own.
                let resolved = self.abs_path(&dir.join(&target));
                if !resolved.starts_with(&root) {
                    continue;
                }
                if std::fs::metadata(&resolved).is_err() {
                    r.add(
                        RULE_FILE_REFERENCE_MISSING,
                        Severity::Error,
                        offset + i + 1,
                        format!("referenced file {target:?} does not exist"),
                    );
                }
            }
        }
    }

    /// Returns the name of the directory holding the skill file, or `None`
    /// when there is no meaningful directory to match the name against: a
    /// loose skill file in the working directory is named after the project,
    /// not the skill.
    fn skill_dir(&self, path: &str) -> Option<String> {
        let abs = self.abs_path(Path::new(path));
        let dir = abs.parent()?.to_path_buf();
        if dir == clean_path(&self.cwd) {
            return None;
        }
        dir.file_name()?.to_str().map(str::to_string)
    }

    /// Joins `path` onto the linter's working directory when relative and
    /// lexically normalizes it, without touching the filesystem or resolving
    /// symlinks (mirroring Go's `filepath.Abs`).
    fn abs_path(&self, path: &Path) -> PathBuf {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        clean_path(&joined)
    }
}

/// Tracks which lines sit inside a fenced code block, so their contents are
/// not scanned for file references.
///
/// A single boolean is not enough: a fence closes only on a run of the same
/// marker character at least as long as the one that opened it, so a ``` line
/// inside a ```` block is content rather than a close. Getting that wrong
/// re-exposes the block body to the reference check, and
/// `file-reference.missing` is an error, so it fails a build on a correct file.
#[derive(Default)]
struct FenceTracker {
    /// The open fence's marker character and run length, `None` outside a block.
    open: Option<(char, usize)>,
}

impl FenceTracker {
    /// Feeds the tracker one line and reports whether that line's content
    /// should be scanned. Fence lines themselves never are.
    fn scan_line(&mut self, line: &str) -> bool {
        let trimmed = line.trim();
        let Some((marker, len, info)) = fence_parts(trimmed) else {
            return self.open.is_none();
        };
        match self.open {
            // Inside a block, only a matching fence closes it: same marker, at
            // least as long, and no info string. Anything else -- a shorter
            // run, the other marker, ```rust -- is block content.
            Some((open_marker, open_len)) => {
                if marker == open_marker && len >= open_len && info.trim().is_empty() {
                    self.open = None;
                }
            }
            None => self.open = Some((marker, len)),
        }
        false
    }
}

/// Splits a fence line into its marker character, the length of its marker run,
/// and the info string that follows. `None` when the line is not a fence.
fn fence_parts(trimmed: &str) -> Option<(char, usize, &str)> {
    let marker = match trimmed.chars().next()? {
        c @ ('`' | '~') => c,
        _ => return None,
    };
    // Markers are ASCII, so the char count doubles as a byte offset.
    let len = trimmed.chars().take_while(|c| *c == marker).count();
    if len < 3 {
        return None;
    }
    Some((marker, len, &trimmed[len..]))
}

/// Extracts the file path a markdown link points at, or `None` when the link
/// is not a checkable local file reference: an absolute URL, an in-page
/// anchor, an absolute path, or a templated placeholder.
fn link_target_path(raw: &str) -> Option<String> {
    let mut target = raw.trim().to_string();
    if let Some(rest) = target.strip_prefix('<')
        && let Some(end) = rest.find('>')
    {
        target = rest[..end].to_string();
    }
    if let Some(sp) = target.find([' ', '\t']) {
        target.truncate(sp);
    }
    if target.is_empty() || target.starts_with('#') {
        return None;
    }
    if URI_SCHEME_RE.is_match(&target) || target.starts_with("//") {
        return None;
    }
    if let Some(frag) = target.find(['#', '?']) {
        target.truncate(frag);
    }
    if target.is_empty()
        || Path::new(&target).is_absolute()
        || target.contains(['{', '}', '$', '*'])
    {
        return None;
    }
    Some(target)
}

/// Extracts a checkable file path from an inline code span, or `None` when
/// the span is not clearly a relative file reference. Narrowly scoped to
/// avoid false positives on flags, identifiers, and shell snippets: the span
/// must be a single whitespace-free token starting with `./` or `../` and
/// ending in a file extension.
fn code_span_target_path(raw: &str) -> Option<String> {
    let target = raw.trim();
    if target.is_empty() || target.chars().any(char::is_whitespace) {
        return None;
    }
    if !(target.starts_with("./") || target.starts_with("../")) {
        return None;
    }
    if target.contains(['{', '}', '$', '*', '#', '?']) {
        return None;
    }
    let last_seg = target.rsplit('/').next().unwrap_or(target);
    let (_, ext) = last_seg.rsplit_once('.')?;
    if ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(target.to_string())
}

fn clean_path(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ => out.push(comp),
            },
            other => out.push(other),
        }
    }
    let mut result = PathBuf::new();
    for c in out {
        result.push(c.as_os_str());
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by_key(|f| {
        RULE_ORDER
            .get(f.rule.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
}

/// Renders n with thousands separators, so "6,142 tokens" reads at a glance
/// in reports.
fn humanize(n: i64) -> String {
    let s = n.to_string();
    if n < 0 {
        return s;
    }
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(skill_name: &str, src: &str) -> (tempfile::TempDir, Target) {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join(skill_name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, src).unwrap();
        let target = Target {
            path: path.to_string_lossy().to_string(),
            kind: Kind::Skill,
            root: base.path().to_path_buf(),
        };
        (base, target)
    }

    fn write_file(dir: &Path, name: &str, src: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, src).unwrap();
        path.to_string_lossy().to_string()
    }

    fn agents_target(dir: &Path, src: &str) -> Target {
        agents_target_under(dir, dir, src)
    }

    /// An AGENTS.md written into `dir`, but discovered under walk root `root`.
    /// Separate from `agents_target` so a fixture can sit below its root, which
    /// is what makes a `../` reference back into the tree distinguishable from
    /// one that escapes it.
    fn agents_target_under(root: &Path, dir: &Path, src: &str) -> Target {
        Target {
            path: write_file(dir, "AGENTS.md", src),
            kind: Kind::Agents,
            root: root.to_path_buf(),
        }
    }

    fn rule_ids(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.rule.as_str()).collect()
    }

    /// Every budget off, so rule tests see only the rule under test. Spelled
    /// out rather than derived from Config::default(), which now carries the
    /// real budgets -- keep it explicit.
    fn generous_config() -> Config {
        Config {
            max_agents_tokens: 0,
            max_skill_tokens: 0,
            max_skill_name_tokens: 0,
            max_skill_description_tokens: 0,
            disabled: vec![],
            strict: false,
        }
    }

    const VALID_SKILL: &str = "---\nname: valid-skill\ndescription: Use this skill when you need a fixture that passes every rule.\n---\n\n# Valid skill\n\nBody content.\n";

    #[test]
    fn skill_frontmatter_rules() {
        let long_name = "a".repeat(MAX_NAME_CHARS + 1);
        let long_desc = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        let cases: Vec<(&str, &str, String, Vec<&str>)> = vec![
            ("valid skill has no findings", "valid-skill", VALID_SKILL.to_string(), vec![]),
            (
                "missing front matter",
                "no-frontmatter",
                "# A skill with no front matter\n\nBody.\n".to_string(),
                vec![RULE_FRONTMATTER_MISSING],
            ),
            (
                "front matter not first",
                "late-frontmatter",
                "\n---\nname: late-frontmatter\ndescription: Late.\n---\n".to_string(),
                vec![RULE_FRONTMATTER_NOT_FIRST],
            ),
            (
                "unterminated front matter",
                "unterminated",
                "---\nname: unterminated\ndescription: No closing fence.\n".to_string(),
                vec![RULE_FRONTMATTER_UNTERMINATED],
            ),
            (
                "invalid yaml",
                "bad-yaml",
                "---\nname: bad-yaml\n  description: wrong indent\n---\nBody.\n".to_string(),
                vec![RULE_FRONTMATTER_INVALID],
            ),
            (
                "front matter is a sequence",
                "sequence",
                "---\n- name: sequence\n---\nBody.\n".to_string(),
                vec![RULE_FRONTMATTER_INVALID],
            ),
            (
                "name missing",
                "nameless",
                "---\ndescription: A skill with no name.\n---\nBody.\n".to_string(),
                vec![RULE_NAME_REQUIRED],
            ),
            (
                "name empty",
                "empty-name",
                "---\nname: \"\"\ndescription: A skill with an empty name.\n---\nBody.\n".to_string(),
                vec![RULE_NAME_REQUIRED],
            ),
            (
                "name not a scalar",
                "listy-name",
                "---\nname:\n  - listy-name\ndescription: A skill whose name is a list.\n---\nBody.\n"
                    .to_string(),
                vec![RULE_NAME_REQUIRED],
            ),
            (
                "name has bad characters",
                "Bad_Name",
                "---\nname: Bad_Name\ndescription: A skill with an invalid name.\n---\nBody.\n".to_string(),
                vec![RULE_NAME_FORMAT],
            ),
            (
                "name has double hyphen",
                "double--hyphen",
                "---\nname: double--hyphen\ndescription: A skill with a doubled hyphen.\n---\nBody.\n"
                    .to_string(),
                vec![RULE_NAME_FORMAT],
            ),
            (
                "name too long",
                "long",
                format!(
                    "---\nname: {long_name}\ndescription: A skill with a very long name.\n---\nBody.\n"
                ),
                vec![RULE_NAME_LENGTH, RULE_NAME_DIR_MISMATCH],
            ),
            (
                "name does not match directory",
                "some-directory",
                "---\nname: other-name\ndescription: A skill in a mismatched directory.\n---\nBody.\n"
                    .to_string(),
                vec![RULE_NAME_DIR_MISMATCH],
            ),
            (
                "description missing",
                "no-description",
                "---\nname: no-description\n---\nBody.\n".to_string(),
                vec![RULE_DESCRIPTION_REQUIRED],
            ),
            (
                "description empty",
                "blank-description",
                "---\nname: blank-description\ndescription: \"   \"\n---\nBody.\n".to_string(),
                vec![RULE_DESCRIPTION_REQUIRED],
            ),
            (
                "description too long",
                "verbose",
                format!("---\nname: verbose\ndescription: {long_desc}\n---\nBody.\n"),
                vec![RULE_DESCRIPTION_LENGTH],
            ),
            (
                "unknown key",
                "extra-keys",
                "---\nname: extra-keys\ndescription: A skill with a stray key.\nauthor: someone\n---\nBody.\n"
                    .to_string(),
                vec![RULE_FRONTMATTER_UNKNOWN_KEY],
            ),
            (
                "known optional keys are accepted",
                "optional-keys",
                "---\nname: optional-keys\ndescription: A skill using every optional key.\nlicense: MIT\nallowed-tools:\n  - Read\n  - Bash\nmetadata:\n  owner: infra\n---\nBody.\n".to_string(),
                vec![],
            ),
            (
                "allowed-tools wrong type",
                "bad-tools",
                "---\nname: bad-tools\ndescription: A skill with a malformed tool list.\nallowed-tools:\n  read: true\n---\nBody.\n".to_string(),
                vec![RULE_ALLOWED_TOOLS_TYPE],
            ),
            (
                "metadata wrong type",
                "bad-metadata",
                "---\nname: bad-metadata\ndescription: A skill with scalar metadata.\nmetadata: not-a-mapping\n---\nBody.\n".to_string(),
                vec![RULE_METADATA_TYPE],
            ),
            (
                "several problems are all reported",
                "multi",
                "---\nname: Multi_Skill\nextra: true\n---\nBody.\n".to_string(),
                vec![
                    RULE_FRONTMATTER_UNKNOWN_KEY,
                    RULE_NAME_FORMAT,
                    RULE_NAME_DIR_MISMATCH,
                    RULE_DESCRIPTION_REQUIRED,
                ],
            ),
        ];

        for (name, skill_dir_name, src, want_rules) in cases {
            let (_base, target) = write_skill(skill_dir_name, &src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert_eq!(rule_ids(&res.findings), want_rules, "{name}");
        }
    }

    #[test]
    fn token_budgets() {
        let (_base, target) = write_skill("budgeted", VALID_SKILL);

        let mut cfg = generous_config();
        cfg.max_skill_tokens = 1;
        let res = Linter::new(cfg, None).file(&target).unwrap();
        assert_eq!(
            rule_ids(&res.findings),
            vec![RULE_NAME_DIR_MISMATCH, RULE_TOKENS_CONTENT]
        );

        let mut cfg = generous_config();
        cfg.max_skill_tokens = 0;
        let res = Linter::new(cfg, None).file(&target).unwrap();
        assert_eq!(rule_ids(&res.findings), vec![RULE_NAME_DIR_MISMATCH]);

        let mut cfg = generous_config();
        cfg.max_skill_name_tokens = 1;
        let res = Linter::new(cfg, None).file(&target).unwrap();
        assert_eq!(
            rule_ids(&res.findings),
            vec![RULE_NAME_DIR_MISMATCH, RULE_TOKENS_NAME]
        );

        let mut cfg = generous_config();
        cfg.max_skill_description_tokens = 2;
        let res = Linter::new(cfg, None).file(&target).unwrap();
        assert_eq!(
            rule_ids(&res.findings),
            vec![RULE_NAME_DIR_MISMATCH, RULE_TOKENS_DESCRIPTION]
        );

        let res = Linter::new(generous_config(), None).file(&target).unwrap();
        assert!(res.tokens.content > 0 && res.tokens.name > 0 && res.tokens.description > 0);
    }

    #[test]
    fn per_kind_content_budget() {
        let base = tempfile::tempdir().unwrap();
        let body = "some filler prose to spend tokens on. ".repeat(20);
        let agents = agents_target(base.path(), &body);
        let (_skill_base, skill) = write_skill(
            "kinded",
            &format!("---\nname: kinded\ndescription: A skill with a body.\n---\n{body}"),
        );

        let mut cfg = generous_config();
        cfg.max_skill_tokens = 10000;
        cfg.max_agents_tokens = 1;
        let linter = Linter::new(cfg.clone(), None);

        let agents_res = linter.file(&agents).unwrap();
        assert_eq!(agents_res.errors(), 1);

        let skill_res = linter.file(&skill).unwrap();
        assert!(
            !skill_res
                .findings
                .iter()
                .any(|f| f.rule == RULE_TOKENS_CONTENT)
        );

        cfg.max_agents_tokens = 0;
        cfg.max_skill_tokens = 1;
        let skill_res = Linter::new(cfg, None).file(&skill).unwrap();
        assert!(
            skill_res
                .findings
                .iter()
                .any(|f| f.rule == RULE_TOKENS_CONTENT)
        );
    }

    #[test]
    fn agents_frontmatter_is_not_validated() {
        let base = tempfile::tempdir().unwrap();
        let src =
            "---\ntitle: Project instructions\nowner: infra\n---\n\n# Instructions\n\nBody.\n";
        let target = agents_target(base.path(), src);

        let res = Linter::new(generous_config(), None).file(&target).unwrap();
        assert!(res.findings.is_empty());
        assert!(res.tokens.content > 0);
    }

    #[test]
    fn agents_unterminated_frontmatter_is_reported() {
        let base = tempfile::tempdir().unwrap();
        let src = "---\ntitle: Project instructions\n\n# Instructions never closed\n";
        let target = agents_target(base.path(), src);

        let res = Linter::new(generous_config(), None).file(&target).unwrap();
        assert_eq!(rule_ids(&res.findings), vec![RULE_FRONTMATTER_UNTERMINATED]);
    }

    #[test]
    fn agents_frontmatter_excluded_from_content() {
        let base = tempfile::tempdir().unwrap();
        let fm_src =
            "---\ntitle: A fairly wordy front matter block that costs real tokens\n---\nBody.\n";
        let with_fm = agents_target(base.path(), fm_src);
        let base2 = tempfile::tempdir().unwrap();
        let bare = agents_target(base2.path(), "Body.\n");

        let linter = Linter::new(generous_config(), None);
        let with_res = linter.file(&with_fm).unwrap();
        let bare_res = linter.file(&bare).unwrap();
        assert_eq!(with_res.tokens.content, bare_res.tokens.content);
    }

    #[test]
    fn name_dir_mismatch_uses_injected_cwd() {
        // name.dir-mismatch deliberately ignores a skill sitting directly in
        // the working directory, since a loose SKILL.md is named for its
        // project rather than its folder. Injecting the cwd is what makes that
        // branch testable: reading it from the process would mean chdir-ing
        // while the rest of the suite runs concurrently.
        let (base, target) = write_skill(
            "some-directory",
            "---\nname: other-name\ndescription: A skill in a mismatched directory.\n---\nBody.\n",
        );
        let holding_dir = Path::new(&target.path).parent().unwrap().to_path_buf();

        let res = Linter::with_cwd(generous_config(), None, holding_dir)
            .file(&target)
            .unwrap();
        assert!(res.findings.is_empty(), "{:?}", rule_ids(&res.findings));

        let res = Linter::with_cwd(generous_config(), None, base.path().to_path_buf())
            .file(&target)
            .unwrap();
        assert_eq!(rule_ids(&res.findings), vec![RULE_NAME_DIR_MISMATCH]);
    }

    #[test]
    fn default_config_enforces_budgets() {
        let cfg = Config::default();
        assert_eq!(cfg.max_agents_tokens, DEFAULT_MAX_AGENTS_TOKENS);
        assert_eq!(cfg.max_skill_tokens, DEFAULT_MAX_SKILL_TOKENS);
        assert_eq!(cfg.max_skill_name_tokens, DEFAULT_MAX_SKILL_NAME_TOKENS);
        assert_eq!(
            cfg.max_skill_description_tokens,
            DEFAULT_MAX_SKILL_DESCRIPTION_TOKENS
        );

        // The point of the hand-written Default: reaching for it must not
        // hand back a linter that checks nothing.
        let base = tempfile::tempdir().unwrap();
        let body = "some filler prose to spend tokens on. ".repeat(500);
        let target = agents_target(base.path(), &body);
        let res = Linter::new(Config::default(), None).file(&target).unwrap();
        assert_eq!(rule_ids(&res.findings), vec![RULE_TOKENS_CONTENT]);
    }

    #[test]
    fn strict_promotes_warnings() {
        let (_base, target) = write_skill(
            "mismatched-dir",
            "---\nname: other-name\ndescription: A skill in a mismatched directory.\n---\nBody.\n",
        );

        let cfg = generous_config();
        let res = Linter::new(cfg.clone(), None).file(&target).unwrap();
        assert_eq!(res.errors(), 0);
        assert_eq!(res.warnings(), 1);

        let mut cfg = cfg;
        cfg.strict = true;
        let res = Linter::new(cfg, None).file(&target).unwrap();
        assert_eq!(res.errors(), 1);
        assert_eq!(res.warnings(), 0);
    }

    #[test]
    fn disable_skips_rules() {
        let (_base, target) = write_skill(
            "mismatched-dir",
            "---\nname: Bad_Name\ndescription: A skill with an invalid name.\n---\nBody.\n",
        );

        let mut cfg = generous_config();
        cfg.disabled = vec![
            RULE_NAME_FORMAT.to_string(),
            RULE_NAME_DIR_MISMATCH.to_string(),
        ];
        let res = Linter::new(cfg, None).file(&target).unwrap();
        assert!(res.findings.is_empty());
    }

    #[test]
    fn findings_carry_file_and_line() {
        let (_base, target) = write_skill(
            "lines",
            "---\nname: Bad_Name\ndescription: A skill with an invalid name.\n---\nBody.\n",
        );

        let res = Linter::new(generous_config(), None).file(&target).unwrap();
        for f in &res.findings {
            assert_eq!(f.file, target.path);
            if f.rule == RULE_NAME_FORMAT {
                assert_eq!(f.line, 2);
            }
        }
    }

    #[test]
    fn missing_file_is_an_error() {
        let base = tempfile::tempdir().unwrap();
        let target = Target {
            path: base.path().join("SKILL.md").to_string_lossy().to_string(),
            kind: Kind::Skill,
            root: base.path().to_path_buf(),
        };
        assert!(Linter::new(generous_config(), None).file(&target).is_err());
    }

    #[test]
    fn humanize_formats_with_separators() {
        let cases: &[(i64, &str)] = &[
            (0, "0"),
            (7, "7"),
            (999, "999"),
            (1000, "1,000"),
            (6142, "6,142"),
            (1234567, "1,234,567"),
        ];
        for (n, want) in cases {
            assert_eq!(humanize(*n), *want, "humanize({n})");
        }
    }

    #[test]
    fn file_reference_rule() {
        {
            let dir = tempfile::tempdir().unwrap();
            write_file(dir.path(), "notes.md", "notes");
            let src = "# Instructions\n\nSee [notes](./notes.md) for details.\n";
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert!(res.findings.is_empty());
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let src = "# Instructions\n\nSee [notes](./notes.md) for details.\n";
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert_eq!(rule_ids(&res.findings), vec![RULE_FILE_REFERENCE_MISSING]);
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let src = "See [docs](https://example.com/x), [mail](mailto:a@example.com) and [section](#heading).\n";
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert!(res.findings.is_empty());
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let src = "Example syntax:\n\n```md\n[example](./missing.md)\n```\n";
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert!(res.findings.is_empty());
        }
        {
            let (_base, target) = write_skill(
                "with-link",
                "---\nname: with-link\ndescription: A skill that references a missing helper script.\n---\nSee [helper](./helper.sh).\n",
            );
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert_eq!(rule_ids(&res.findings), vec![RULE_FILE_REFERENCE_MISSING]);
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let src = "# Instructions\n\nIntro line.\n\nSee [notes](./notes.md).\n";
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert_eq!(res.findings.len(), 1);
            assert_eq!(res.findings[0].line, 5);
        }
    }

    #[test]
    fn file_references_outside_the_linted_tree_are_skipped() {
        // Escapes are skipped whether or not the target exists. ctxlint runs
        // in CI over checkouts it does not trust, and a finding that
        // distinguished "exists" from "does not exist" above the root would
        // make any file's markdown a probe for what is on the host.
        let outer = tempfile::tempdir().unwrap();
        write_file(outer.path(), "outside.md", "a real file above the root");
        let root = outer.path().join("root");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();

        let escaping: &[(&str, &str)] = &[
            (
                "link to an existing file above the root",
                "See [x](../../outside.md).\n",
            ),
            (
                "link to a missing file above the root",
                "See [x](../../nowhere.md).\n",
            ),
            ("code span escaping the root", "Read `../../outside.md`.\n"),
        ];
        for (name, src) in escaping {
            let target = agents_target_under(&root, &sub, src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert!(
                res.findings.is_empty(),
                "{name}: {:?}",
                rule_ids(&res.findings)
            );
        }

        // Control: a missing reference that stays inside the tree is still
        // reported, so the cases above are not passing vacuously.
        let target = agents_target_under(&root, &sub, "See [x](./nowhere.md).\n");
        let res = Linter::new(generous_config(), None).file(&target).unwrap();
        assert_eq!(rule_ids(&res.findings), vec![RULE_FILE_REFERENCE_MISSING]);
    }

    #[test]
    fn file_reference_rule_nested_fences() {
        // A fence closes only on the same marker, at least as long, with no
        // info string. Each case pairs a source file with the rules it should
        // produce, so both directions of the old toggle bug are covered: the
        // block body must stay unscanned, and the tracker must not latch open
        // and swallow the prose that follows.
        let cases: &[(&str, &str, Vec<&str>)] = &[
            (
                "inner ``` does not close an outer ````",
                "````md\n```\n[x](./missing.md)\n```\nstill inside: [y](./gone.md)\n````\n",
                vec![],
            ),
            (
                "~~~ does not close a ``` block",
                "```\n~~~\n[x](./missing.md)\n~~~\n```\n",
                vec![],
            ),
            (
                "a fence carrying an info string does not close",
                "```\n```rust\n[x](./missing.md)\n```\n",
                vec![],
            ),
            (
                "prose after a closed block is still scanned",
                "````\n```\n````\n\nSee [y](./gone.md).\n",
                vec![RULE_FILE_REFERENCE_MISSING],
            ),
            (
                "a longer run closes a shorter block",
                "```\n[x](./missing.md)\n````\n\nSee [y](./gone.md).\n",
                vec![RULE_FILE_REFERENCE_MISSING],
            ),
            (
                "an indented closing fence still closes",
                "```\n[x](./missing.md)\n  ```\n\nSee [y](./gone.md).\n",
                vec![RULE_FILE_REFERENCE_MISSING],
            ),
        ];

        for (name, src, want) in cases {
            let dir = tempfile::tempdir().unwrap();
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert_eq!(rule_ids(&res.findings), *want, "{name}");
        }
    }

    #[test]
    fn file_reference_rule_code_spans() {
        {
            // Path-shaped inline code span pointing at a missing file inside
            // the tree.
            let dir = tempfile::tempdir().unwrap();
            let sub = dir.path().join("sub");
            fs::create_dir_all(&sub).unwrap();
            let src = "Read instructions from `../planner_instructions.md`.\n";
            let target = agents_target_under(dir.path(), &sub, src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert_eq!(rule_ids(&res.findings), vec![RULE_FILE_REFERENCE_MISSING]);
        }
        {
            // Same reference, but the file exists. A `../` hop that lands back
            // inside the linted tree must still be checked -- this is the
            // guard that skipping escapes did not swallow ordinary references.
            let dir = tempfile::tempdir().unwrap();
            let sub = dir.path().join("sub");
            fs::create_dir_all(&sub).unwrap();
            write_file(dir.path(), "planner_instructions.md", "notes");
            let src = "Read instructions from `../planner_instructions.md`.\n";
            let target = agents_target_under(dir.path(), &sub, src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert!(res.findings.is_empty());
        }
        {
            // Code spans that are not path-shaped are left alone.
            let dir = tempfile::tempdir().unwrap();
            let src = "Run `cargo test`, check `--strict`, or see `notes` and `./bin/lint`.\n";
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert!(res.findings.is_empty());
        }
        {
            // A link and a code span pointing at the same missing target on
            // one line report once, not twice.
            let dir = tempfile::tempdir().unwrap();
            let src = "See [it](./missing.md) or `./missing.md`.\n";
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert_eq!(rule_ids(&res.findings), vec![RULE_FILE_REFERENCE_MISSING]);
        }
        {
            // Code spans inside fenced code blocks are still ignored.
            let dir = tempfile::tempdir().unwrap();
            let src = "Example:\n\n```md\nSee `../missing.md`.\n```\n";
            let target = agents_target(dir.path(), src);
            let res = Linter::new(generous_config(), None).file(&target).unwrap();
            assert!(res.findings.is_empty());
        }
    }
}
