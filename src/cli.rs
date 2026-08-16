//! Wires flags, discovery, linting and reporting together.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config;
use crate::discover;
use crate::lint::{self, Config};
use crate::report;

/// No errors were found; warnings alone still exit OK.
pub const EXIT_OK: i32 = 0;
/// At least one error-severity finding was reported.
pub const EXIT_FINDINGS: i32 = 1;
/// The run could not happen: bad flags or unreadable files.
pub const EXIT_USAGE: i32 = 2;

/// The reported build version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const DEFAULT_MAX_AGENTS_TOKENS: i64 = 2500;
const DEFAULT_MAX_SKILL_TOKENS: i64 = 5000;
const DEFAULT_MAX_SKILL_NAME_TOKENS: i64 = 16;
const DEFAULT_MAX_SKILL_DESCRIPTION_TOKENS: i64 = 100;

/// What the command line asked for. Every setting a config file can also
/// carry is optional here, so an unset flag is distinguishable from one set to
/// its default value and the file can supply it instead.
#[derive(Default)]
struct Flags {
    max_agents_tokens: Option<i64>,
    max_skill_tokens: Option<i64>,
    max_skill_name_tokens: Option<i64>,
    max_skill_description_tokens: Option<i64>,
    format: Option<String>,
    color: Option<String>,
    strict: Option<bool>,
    quiet: Option<bool>,
    show_version: bool,
    list_rules: bool,
    excludes: Vec<String>,
    disabled: Vec<String>,
    config: Option<String>,
    no_config: bool,
    paths: Vec<String>,
}

/// The settings a run actually uses, after the command line, the config file
/// and the defaults have been merged in that order of precedence.
struct Resolved {
    max_agents_tokens: i64,
    max_skill_tokens: i64,
    max_skill_name_tokens: i64,
    max_skill_description_tokens: i64,
    format: String,
    color: String,
    strict: bool,
    quiet: bool,
    excludes: Vec<String>,
    disabled: Vec<String>,
    paths: Vec<String>,
}

/// Merges flags over config-file settings over the built-in defaults. Lists
/// accumulate instead of overriding: `--exclude` and `--disable` add to
/// whatever the file already asked for, since narrowing a run further on the
/// command line is the common case.
fn resolve(f: Flags, cfg: config::Settings) -> Resolved {
    let mut excludes = cfg.excludes;
    excludes.extend(f.excludes);
    let mut disabled = cfg.disabled;
    disabled.extend(f.disabled);

    Resolved {
        max_agents_tokens: f
            .max_agents_tokens
            .or(cfg.max_agents_tokens)
            .unwrap_or(DEFAULT_MAX_AGENTS_TOKENS),
        max_skill_tokens: f
            .max_skill_tokens
            .or(cfg.max_skill_tokens)
            .unwrap_or(DEFAULT_MAX_SKILL_TOKENS),
        max_skill_name_tokens: f
            .max_skill_name_tokens
            .or(cfg.max_skill_name_tokens)
            .unwrap_or(DEFAULT_MAX_SKILL_NAME_TOKENS),
        max_skill_description_tokens: f
            .max_skill_description_tokens
            .or(cfg.max_skill_description_tokens)
            .unwrap_or(DEFAULT_MAX_SKILL_DESCRIPTION_TOKENS),
        format: f
            .format
            .or(cfg.format)
            .unwrap_or_else(|| "text".to_string()),
        color: f.color.or(cfg.color).unwrap_or_else(|| "auto".to_string()),
        strict: f.strict.or(cfg.strict).unwrap_or(false),
        quiet: f.quiet.or(cfg.quiet).unwrap_or(false),
        excludes,
        disabled,
        paths: if f.paths.is_empty() {
            vec![".".to_string()]
        } else {
            f.paths
        },
    }
}

/// Finds the config file for this run: the one named by `--config`, or the
/// nearest `.ctxlint.yaml` at or above the working directory. `--no-config`
/// skips the search, and finding nothing is not an error.
fn load_config(f: &Flags, cwd: &Path) -> Result<config::Settings, String> {
    if let Some(path) = &f.config {
        if f.no_config {
            return Err("--config and --no-config cannot be used together".to_string());
        }
        return config::load(Path::new(path));
    }
    if f.no_config {
        return Ok(config::Settings::default());
    }
    match config::discover(cwd) {
        Some(path) => config::load(&path),
        None => Ok(config::Settings::default()),
    }
}

enum ParseOutcome {
    Flags(Flags),
    Help,
    Err(String),
}

/// Splits a token's flag name from any inline `=value`, stripping one or two
/// leading dashes.
fn split_flag(token: &str) -> (&str, Option<&str>) {
    let stripped = token
        .strip_prefix("--")
        .or_else(|| token.strip_prefix('-'))
        .unwrap_or(token);
    match stripped.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (stripped, None),
    }
}

fn parse_args(args: &[String]) -> ParseOutcome {
    let mut f = Flags::default();
    let mut negative: Vec<String> = Vec::new();
    let mut i = 0;

    macro_rules! next_value {
        ($name:expr, $inline:expr) => {
            match $inline {
                Some(v) => v.to_string(),
                None => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => v.clone(),
                        None => {
                            return ParseOutcome::Err(format!(
                                "flag needs an argument: --{}",
                                $name
                            ));
                        }
                    }
                }
            }
        };
    }

    macro_rules! parse_int {
        ($name:expr, $raw:expr) => {
            match $raw.parse::<i64>() {
                Ok(n) => {
                    if n < 0 {
                        negative.push(format!("--{}", $name));
                    }
                    n
                }
                Err(_) => {
                    return ParseOutcome::Err(format!(
                        "invalid value {:?} for flag --{}: not an integer",
                        $raw, $name
                    ))
                }
            }
        };
    }

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }

        let (name, inline) = split_flag(arg);
        match name {
            "h" | "help" => return ParseOutcome::Help,
            "version" => f.show_version = true,
            "list-rules" => f.list_rules = true,
            "strict" => f.strict = Some(parse_bool_inline(inline)),
            "quiet" => f.quiet = Some(parse_bool_inline(inline)),
            "no-config" => f.no_config = parse_bool_inline(inline),
            "max-agents-tokens" => {
                let raw = next_value!("max-agents-tokens", inline);
                f.max_agents_tokens = Some(parse_int!("max-agents-tokens", raw));
            }
            "max-skill-tokens" => {
                let raw = next_value!("max-skill-tokens", inline);
                f.max_skill_tokens = Some(parse_int!("max-skill-tokens", raw));
            }
            "max-skill-name-tokens" => {
                let raw = next_value!("max-skill-name-tokens", inline);
                f.max_skill_name_tokens = Some(parse_int!("max-skill-name-tokens", raw));
            }
            "max-skill-description-tokens" => {
                let raw = next_value!("max-skill-description-tokens", inline);
                f.max_skill_description_tokens =
                    Some(parse_int!("max-skill-description-tokens", raw));
            }
            "format" => f.format = Some(next_value!("format", inline)),
            "color" => f.color = Some(next_value!("color", inline)),
            "config" => {
                let v = next_value!("config", inline);
                if v.is_empty() {
                    return ParseOutcome::Err("--config value must not be empty".to_string());
                }
                f.config = Some(v);
            }
            "exclude" => {
                let v = next_value!("exclude", inline);
                if v.is_empty() {
                    return ParseOutcome::Err("--exclude value must not be empty".to_string());
                }
                f.excludes.push(v);
            }
            "disable" => {
                let v = next_value!("disable", inline);
                if v.is_empty() {
                    return ParseOutcome::Err("--disable value must not be empty".to_string());
                }
                f.disabled.push(v);
            }
            other => return ParseOutcome::Err(format!("flag provided but not defined: -{other}")),
        }
        i += 1;
    }

    f.paths = args[i..].to_vec();

    if !negative.is_empty() {
        negative.sort();
        return ParseOutcome::Err(format!(
            "{} must be zero or more (0 disables the check)",
            negative.join(", ")
        ));
    }

    ParseOutcome::Flags(f)
}

fn parse_bool_inline(inline: Option<&str>) -> bool {
    match inline {
        Some(v) => v != "false" && v != "0",
        None => true,
    }
}

/// Executes ctxlint and returns the process exit code. Findings go to stdout;
/// usage and I/O problems go to stderr. `is_terminal` decides whether
/// `--color auto` colorizes output; callers pass whether their real stdout
/// is a terminal rather than this function inspecting the process's actual
/// file descriptors, so tests can pin the "auto" behavior instead of it
/// depending on however the test binary's stdout happens to be attached.
pub fn run(
    args: &[String],
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    is_terminal: bool,
) -> i32 {
    let f = match parse_args(args) {
        ParseOutcome::Flags(f) => f,
        ParseOutcome::Help => {
            print_usage(stderr);
            return EXIT_OK;
        }
        ParseOutcome::Err(msg) => {
            let _ = writeln!(stderr, "ctxlint: {msg}");
            print_usage(stderr);
            return EXIT_USAGE;
        }
    };

    if f.show_version {
        let _ = writeln!(stdout, "ctxlint {VERSION}");
        return EXIT_OK;
    }
    if f.list_rules {
        for rule in lint::RULES {
            let _ = writeln!(stdout, "{rule}");
        }
        return EXIT_OK;
    }

    // Typos in --disable are caught before the config file is read so the
    // error names the flag the user just typed.
    if let Err(msg) = check_rule_names(&f.disabled) {
        let _ = writeln!(stderr, "ctxlint: {msg}");
        return EXIT_USAGE;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file_cfg = match load_config(&f, &cwd) {
        Ok(cfg) => cfg,
        Err(msg) => {
            let _ = writeln!(stderr, "ctxlint: {msg}");
            return EXIT_USAGE;
        }
    };

    let f = resolve(f, file_cfg);

    if f.format != "text" && f.format != "json" {
        let _ = writeln!(
            stderr,
            "ctxlint: unknown format {:?}: want text or json",
            f.format
        );
        return EXIT_USAGE;
    }
    if f.color != "auto" && f.color != "always" && f.color != "never" {
        let _ = writeln!(
            stderr,
            "ctxlint: unknown color {:?}: want auto, always, or never",
            f.color
        );
        return EXIT_USAGE;
    }

    let targets = match discover::find(&f.paths, &f.excludes) {
        Ok(t) => t,
        Err(msg) => {
            let _ = writeln!(stderr, "ctxlint: {msg}");
            return EXIT_USAGE;
        }
    };

    let linter = lint::Linter::new(
        Config {
            max_agents_tokens: f.max_agents_tokens,
            max_skill_tokens: f.max_skill_tokens,
            max_skill_name_tokens: f.max_skill_name_tokens,
            max_skill_description_tokens: f.max_skill_description_tokens,
            disabled: f.disabled,
            strict: f.strict,
        },
        None,
    );

    let mut results = Vec::with_capacity(targets.len());
    for t in &targets {
        match linter.file(t) {
            Ok(res) => results.push(res),
            Err(msg) => {
                let _ = writeln!(stderr, "ctxlint: {msg}");
                return EXIT_USAGE;
            }
        }
    }

    let write_result = if f.format == "json" {
        report::json(stdout, &results, f.quiet)
    } else {
        report::text(
            stdout,
            &results,
            f.quiet,
            resolve_color(&f.color, is_terminal),
        )
    };
    if let Err(e) = write_result {
        let _ = writeln!(stderr, "ctxlint: {e}");
        return EXIT_USAGE;
    }

    if report::summarize(&results).files_with_errors > 0 {
        EXIT_FINDINGS
    } else {
        EXIT_OK
    }
}

/// Decides whether text output gets colorized and decorated with symbols.
/// `--color=always`/`never` are absolute; `auto` (the default) follows the
/// [`NO_COLOR`](https://no-color.org) and `CLICOLOR_FORCE` conventions and
/// otherwise colors only when the caller's stdout is a terminal.
fn resolve_color(choice: &str, is_terminal: bool) -> bool {
    match choice {
        "always" => return true,
        "never" => return false,
        _ => {}
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
        return true;
    }
    is_terminal
}

/// Rejects typos in `--disable` rather than silently doing nothing.
fn check_rule_names(rules: &[String]) -> Result<(), String> {
    let known: std::collections::HashSet<&str> = lint::RULES.iter().copied().collect();
    let mut unknown: Vec<&String> = rules
        .iter()
        .filter(|r| !known.contains(r.as_str()))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();
    let quoted: Vec<String> = unknown.iter().map(|r| format!("{r:?}")).collect();
    Err(format!(
        "unknown rule {} in --disable: run --list-rules to see them all",
        quoted.join(", ")
    ))
}

fn print_usage(w: &mut impl Write) {
    let _ = write!(
        w,
        r#"ctxlint lints agent instruction files: AGENTS.md and SKILL.md.

Usage:
  ctxlint [flags] [path...]

Paths may be files or directories; directories are walked recursively for
AGENTS.md and SKILL.md. With no path given, the current directory is used.

For skills, YAML front matter is validated against the skill spec. For both
kinds, token budgets are enforced on the content, and on a skill's name and
description.

Settings can also live in a config file: the nearest .ctxlint.yaml (or
.ctxlint.yml) at or above the working directory is read automatically. Its
keys are the flag names below without the leading dashes, plus a rules mapping
of rule id to true or false. Flags win over the file.

  max-skill-tokens: 3000
  exclude:
    - testdata
  rules:
    name.dir-mismatch: false

Exit codes: 0 clean (warnings still exit 0), 1 errors found, 2 bad usage.

Flags:
  --max-agents-tokens int              token budget for AGENTS.md content, 0 disables (default {DEFAULT_MAX_AGENTS_TOKENS})
  --max-skill-tokens int                token budget for SKILL.md content, 0 disables (default {DEFAULT_MAX_SKILL_TOKENS})
  --max-skill-name-tokens int           token budget for a skill's name, 0 disables (default {DEFAULT_MAX_SKILL_NAME_TOKENS})
  --max-skill-description-tokens int    token budget for a skill's description, 0 disables (default {DEFAULT_MAX_SKILL_DESCRIPTION_TOKENS})
  --exclude glob                        glob of paths to skip; repeatable
  --disable rule                        rule id to skip; repeatable
  --config path                         read settings from this file instead of searching
  --no-config                           ignore any config file
  --strict                              treat warnings as errors
  --quiet                               report errors only
  --format text|json                    output format (default "text")
  --color auto|always|never             colorize and decorate text output (default "auto")
  --list-rules                          print every rule id and exit
  --version                             print the version and exit
"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(parts: &[&str]) -> String {
        let mut p = PathBuf::from("testdata");
        for part in parts {
            p.push(part);
        }
        p.to_string_lossy().to_string()
    }

    /// Runs the CLI verbatim, config discovery included.
    fn run_raw(args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&args, &mut out, &mut err, false);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// Runs the CLI with config discovery off, so these cases keep testing
    /// flags and defaults no matter what .ctxlint.yaml happens to sit above
    /// the directory the tests run in.
    fn run_args(args: &[&str]) -> (i32, String, String) {
        let mut with_flag = vec!["--no-config"];
        with_flag.extend_from_slice(args);
        run_raw(&with_flag)
    }

    fn write_config(dir: &tempfile::TempDir, body: &str) -> String {
        let path = dir.path().join(".ctxlint.yaml");
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn clean_tree_exits_zero() {
        let (code, stdout, stderr) = run_args(&[&fixture(&["clean"])]);
        assert_eq!(code, EXIT_OK, "stdout: {stdout} stderr: {stderr}");
        assert!(
            stdout.contains("2 files checked, 0 files with errors, 0 files with warnings"),
            "{stdout}"
        );
    }

    #[test]
    fn broken_tree_exits_one() {
        let (code, stdout, _) = run_args(&[&fixture(&["broken"])]);
        assert_eq!(code, EXIT_FINDINGS);
        for want in [
            lint::RULE_FRONTMATTER_MISSING,
            lint::RULE_FRONTMATTER_UNTERMINATED,
            lint::RULE_NAME_FORMAT,
            lint::RULE_NAME_DIR_MISMATCH,
            lint::RULE_FRONTMATTER_UNKNOWN_KEY,
            lint::RULE_DESCRIPTION_LENGTH,
            lint::RULE_TOKENS_DESCRIPTION,
        ] {
            assert!(stdout.contains(want), "stdout missing {want}:\n{stdout}");
        }
        assert!(!stdout.contains("node_modules"), "{stdout}");
    }

    #[test]
    fn text_output_format() {
        let path = fixture(&["broken", "bad-name", "SKILL.md"]);
        let (code, stdout, _) = run_args(&[&path]);
        assert_eq!(code, EXIT_FINDINGS);

        let mut header = "";
        let mut name_format = "";
        for line in stdout.split('\n') {
            if line.ends_with("SKILL.md") {
                header = line;
            }
            if line.contains(lint::RULE_NAME_FORMAT) {
                name_format = line;
            }
        }
        assert!(!header.is_empty(), "{stdout}");
        assert!(!name_format.is_empty(), "{stdout}");
        let trimmed = name_format.strip_prefix("  ").unwrap();
        let parts: Vec<&str> = trimmed.splitn(4, ": ").collect();
        assert_eq!(parts.len(), 4, "{name_format}");
        assert_eq!(parts[0], "2");
        assert_eq!(parts[1], "error");
        assert_eq!(parts[2], lint::RULE_NAME_FORMAT);
    }

    #[test]
    fn json_output() {
        let (code, stdout, _) = run_args(&["--format", "json", &fixture(&["broken"])]);
        assert_eq!(code, EXIT_FINDINGS);
        let got: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
        assert_eq!(got["version"], report::SCHEMA_VERSION);
        let files = got["files"].as_array().unwrap();
        assert_eq!(got["summary"]["files"], files.len() as u64);
        assert!(got["summary"]["files_with_errors"].as_u64().unwrap() > 0);

        let mut paths = Vec::new();
        for file in files {
            paths.push(file["path"].as_str().unwrap().to_string());
            assert!(!file["kind"].as_str().unwrap().is_empty());
            for finding in file["findings"].as_array().unwrap() {
                assert_eq!(finding["file"], file["path"]);
                assert!(!finding["message"].as_str().unwrap().is_empty());
            }
        }
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn json_reports_token_counts() {
        let (_, stdout, _) = run_args(&[
            "--format",
            "json",
            &fixture(&["clean", "skills", "well-formed", "SKILL.md"]),
        ]);
        let got: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let files = got["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let tok = &files[0]["tokens"];
        assert!(tok["content"].as_u64().unwrap() > 0);
        assert!(tok["name"].as_u64().unwrap() > 0);
        assert!(tok["description"].as_u64().unwrap() > 0);
    }

    #[test]
    fn content_budget_flags() {
        let clean = fixture(&["clean"]);

        let (code, stdout, _) = run_args(&[
            "--max-agents-tokens",
            "5",
            "--max-skill-tokens",
            "0",
            &clean,
        ]);
        assert_eq!(code, EXIT_FINDINGS, "{stdout}");
        assert!(
            stdout.contains("AGENTS.md") && !stdout.contains("SKILL.md"),
            "{stdout}"
        );

        let (code, stdout, _) = run_args(&[
            "--max-agents-tokens",
            "0",
            "--max-skill-tokens",
            "5",
            &clean,
        ]);
        assert_eq!(code, EXIT_FINDINGS, "{stdout}");
        assert!(
            stdout.contains("SKILL.md") && !stdout.contains("AGENTS.md"),
            "{stdout}"
        );

        let (code, stdout, _) = run_args(&[
            "--max-agents-tokens",
            "0",
            "--max-skill-tokens",
            "0",
            &clean,
        ]);
        assert_eq!(code, EXIT_OK, "{stdout}");
    }

    #[test]
    fn name_and_description_budget_flags() {
        let skill = fixture(&["clean", "skills", "well-formed", "SKILL.md"]);

        let (code, stdout, _) = run_args(&[
            "--max-skill-tokens",
            "0",
            "--max-skill-name-tokens",
            "1",
            &skill,
        ]);
        assert_eq!(code, EXIT_FINDINGS);
        assert!(stdout.contains(lint::RULE_TOKENS_NAME));

        let (code, stdout, _) = run_args(&[
            "--max-skill-tokens",
            "0",
            "--max-skill-description-tokens",
            "5",
            &skill,
        ]);
        assert_eq!(code, EXIT_FINDINGS);
        assert!(stdout.contains(lint::RULE_TOKENS_DESCRIPTION));

        let (code, ..) = run_args(&[&skill]);
        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn warnings_alone_exit_zero() {
        let skill = fixture(&["broken", "bad-name", "SKILL.md"]);

        let (code, stdout, _) = run_args(&["--disable", lint::RULE_NAME_FORMAT, &skill]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains(lint::RULE_NAME_DIR_MISMATCH));

        let (code, ..) = run_args(&["--strict", "--disable", lint::RULE_NAME_FORMAT, &skill]);
        assert_eq!(code, EXIT_FINDINGS);
    }

    #[test]
    fn quiet_suppresses_warnings() {
        let skill = fixture(&["broken", "bad-name", "SKILL.md"]);

        let (_, stdout, _) = run_args(&["--quiet", &skill]);
        assert!(!stdout.contains(lint::RULE_NAME_DIR_MISMATCH));
        assert!(stdout.contains(lint::RULE_NAME_FORMAT));
        assert!(stdout.contains("warning"));

        let (_, stdout, _) = run_args(&["--quiet", "--format", "json", &skill]);
        assert!(!stdout.contains(lint::RULE_NAME_DIR_MISMATCH));
        let got: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert!(got["summary"]["files_with_warnings"].as_u64().unwrap() > 0);
    }

    #[test]
    fn color_flag_controls_decoration() {
        let skill = fixture(&["broken", "bad-name", "SKILL.md"]);

        // run_args passes is_terminal=false, so "auto" (the default) stays
        // plain here.
        let (_, stdout, _) = run_args(&[&skill]);
        assert!(!stdout.contains('\u{1b}'), "{stdout}");

        let (_, stdout, _) = run_args(&["--color", "always", &skill]);
        assert!(stdout.contains('\u{1b}'), "{stdout}");
        assert!(
            stdout.contains("\u{274c}") || stdout.contains("\u{26a0}"),
            "{stdout}"
        );

        let (_, stdout, _) = run_args(&["--color", "never", &skill]);
        assert!(!stdout.contains('\u{1b}'), "{stdout}");

        let (code, stdout, stderr) = run_args(&["--color", "rainbow", &skill]);
        assert_eq!(code, EXIT_USAGE, "stdout={stdout} stderr={stderr}");
        assert!(stderr.contains("unknown color"), "{stderr}");
    }

    #[test]
    fn exclude_prunes_paths() {
        let (code, stdout, _) = run_args(&[
            "--exclude",
            "verbose-description",
            "--exclude",
            "no-frontmatter",
            "--exclude",
            "unterminated",
            "--exclude",
            "bad-name",
            &fixture(&["broken"]),
        ]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains("1 file checked"), "{stdout}");
    }

    #[test]
    fn usage_errors() {
        let clean = fixture(&["clean"]);
        let cases: &[(&str, Vec<&str>, &str)] = &[
            ("unknown format", vec!["--format", "xml"], "unknown format"),
            (
                "unknown rule",
                vec!["--disable", "no.such.rule"],
                "unknown rule",
            ),
            (
                "negative budget",
                vec!["--max-agents-tokens", "-5"],
                "must be zero or more",
            ),
            (
                "bad exclude glob",
                vec!["--exclude", "["],
                "invalid exclude",
            ),
        ];
        for (name, extra, want) in cases {
            let mut args: Vec<&str> = extra.clone();
            args.push(&clean);
            let (code, stdout, stderr) = run_args(&args);
            assert_eq!(code, EXIT_USAGE, "{name}: stdout={stdout} stderr={stderr}");
            assert!(stderr.contains(want), "{name}: stderr={stderr}");
            assert!(stdout.is_empty(), "{name}: stdout={stdout}");
        }

        let (code, stdout, stderr) = run_args(&["nope-does-not-exist"]);
        assert_eq!(code, EXIT_USAGE, "stdout={stdout} stderr={stderr}");
        assert!(stderr.contains("cannot read"), "{stderr}");

        let (code, stdout, stderr) = run_args(&["Cargo.toml"]);
        assert_eq!(code, EXIT_USAGE, "stdout={stdout} stderr={stderr}");
        assert!(stderr.contains("not an AGENTS.md or SKILL.md"), "{stderr}");

        let (code, stdout, stderr) = run_args(&["--nope"]);
        assert_eq!(code, EXIT_USAGE, "stdout={stdout} stderr={stderr}");
        assert!(stderr.contains("flag provided but not defined"), "{stderr}");
    }

    #[test]
    fn config_file_supplies_settings() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_config(
            &dir,
            "max-agents-tokens: 5\nmax-skill-tokens: 0\nquiet: true\n",
        );

        let (code, stdout, stderr) = run_raw(&["--config", &cfg, &fixture(&["clean"])]);
        assert_eq!(code, EXIT_FINDINGS, "stdout={stdout} stderr={stderr}");
        assert!(
            stdout.contains("AGENTS.md") && !stdout.contains("SKILL.md"),
            "{stdout}"
        );
    }

    #[test]
    fn config_file_disables_rules() {
        let dir = tempfile::tempdir().unwrap();
        let skill = fixture(&["broken", "bad-name", "SKILL.md"]);
        let cfg = write_config(
            &dir,
            "rules:\n  name.format: false\n  name.dir-mismatch: false\n",
        );

        let (code, stdout, _) = run_raw(&["--config", &cfg, &skill]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(!stdout.contains(lint::RULE_NAME_FORMAT), "{stdout}");
        assert!(!stdout.contains(lint::RULE_NAME_DIR_MISMATCH), "{stdout}");
    }

    #[test]
    fn flags_win_over_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let clean = fixture(&["clean"]);
        let cfg = write_config(&dir, "max-agents-tokens: 5\nstrict: true\nformat: json\n");

        // The flag overrides the file's budget, so the tree comes back clean,
        // and --format text overrides the file's json.
        let (code, stdout, stderr) = run_raw(&[
            "--config",
            &cfg,
            "--max-agents-tokens",
            "0",
            "--format",
            "text",
            &clean,
        ]);
        assert_eq!(code, EXIT_OK, "stdout={stdout} stderr={stderr}");
        assert!(stdout.contains("2 files checked"), "{stdout}");

        // Booleans too: --strict=false undoes the file's strict.
        let skill = fixture(&["broken", "bad-name", "SKILL.md"]);
        let cfg = write_config(&dir, "strict: true\nrules:\n  name.format: false\n");
        let (code, stdout, _) = run_raw(&["--config", &cfg, &skill]);
        assert_eq!(code, EXIT_FINDINGS, "{stdout}");
        let (code, stdout, _) = run_raw(&["--config", &cfg, "--strict=false", &skill]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains(lint::RULE_NAME_DIR_MISMATCH), "{stdout}");
    }

    #[test]
    fn excludes_and_disables_accumulate() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_config(
            &dir,
            "exclude:\n  - verbose-description\n  - no-frontmatter\nrules:\n  name.format: false\n",
        );

        let (code, stdout, _) = run_raw(&[
            "--config",
            &cfg,
            "--exclude",
            "unterminated",
            "--exclude",
            "bad-name",
            &fixture(&["broken"]),
        ]);
        assert_eq!(code, EXIT_OK, "{stdout}");
        assert!(stdout.contains("1 file checked"), "{stdout}");
    }

    #[test]
    fn no_config_ignores_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_config(&dir, "max-agents-tokens: 5\n");
        let clean = fixture(&["clean"]);

        let (code, ..) = run_raw(&["--config", &cfg, &clean]);
        assert_eq!(code, EXIT_FINDINGS);

        let (code, stdout, stderr) = run_raw(&["--no-config", &clean]);
        assert_eq!(code, EXIT_OK, "stdout={stdout} stderr={stderr}");
    }

    #[test]
    fn config_usage_errors() {
        let dir = tempfile::tempdir().unwrap();
        let clean = fixture(&["clean"]);
        let bad = write_config(&dir, "max-skill-tokens: -1\n");
        let missing = dir.path().join("absent.yaml").to_string_lossy().to_string();

        let cases: &[(&str, Vec<&str>, &str)] = &[
            (
                "unreadable config",
                vec!["--config", &missing],
                "cannot read config",
            ),
            (
                "invalid config",
                vec!["--config", &bad],
                "must be zero or more",
            ),
            (
                "config and no-config",
                vec!["--config", &bad, "--no-config"],
                "cannot be used together",
            ),
            (
                "empty config path",
                vec!["--config", ""],
                "must not be empty",
            ),
        ];
        for (name, extra, want) in cases {
            let mut args: Vec<&str> = extra.clone();
            args.push(&clean);
            let (code, stdout, stderr) = run_raw(&args);
            assert_eq!(code, EXIT_USAGE, "{name}: stdout={stdout} stderr={stderr}");
            assert!(stderr.contains(want), "{name}: stderr={stderr}");
            assert!(stdout.is_empty(), "{name}: stdout={stdout}");
        }
    }

    #[test]
    fn config_is_discovered_from_the_working_directory() {
        // The walk starts at the process's working directory, which tests must
        // not mutate, so exercise the discovery and merge steps directly.
        let dir = tempfile::tempdir().unwrap();
        write_config(&dir, "max-skill-tokens: 7\n");
        let nested = dir.path().join("skills/deep");
        std::fs::create_dir_all(&nested).unwrap();

        let flags = Flags::default();
        let cfg = load_config(&flags, &nested).unwrap();
        assert_eq!(cfg.max_skill_tokens, Some(7));

        let resolved = resolve(flags, cfg);
        assert_eq!(resolved.max_skill_tokens, 7);
        assert_eq!(resolved.max_agents_tokens, DEFAULT_MAX_AGENTS_TOKENS);
        assert_eq!(resolved.paths, vec![".".to_string()]);

        let skipped = load_config(
            &Flags {
                no_config: true,
                ..Default::default()
            },
            &nested,
        )
        .unwrap();
        assert_eq!(skipped, config::Settings::default());
    }

    #[test]
    fn version_and_list_rules() {
        let (code, stdout, _) = run_args(&["--version"]);
        assert_eq!(code, EXIT_OK);
        assert!(stdout.starts_with("ctxlint "), "{stdout}");

        let (code, stdout, _) = run_args(&["--list-rules"]);
        assert_eq!(code, EXIT_OK);
        let listed: Vec<&str> = stdout.split_whitespace().collect();
        assert_eq!(listed.len(), lint::RULES.len());
        for rule in lint::RULES {
            assert!(stdout.contains(rule), "{stdout}");
        }
    }

    #[test]
    fn help_exits_zero() {
        let (code, _, stderr) = run_args(&["-h"]);
        assert_eq!(code, EXIT_OK);
        assert!(
            stderr.contains("ctxlint lints agent instruction files"),
            "{stderr}"
        );
    }

    #[test]
    fn no_paths_defaults_to_current_directory() {
        // Avoid mutating the process-wide working directory here since cargo
        // runs tests concurrently; instead confirm the no-args fallback
        // matches passing "." explicitly.
        let (code_default, stdout_default, _) = run_args(&[]);
        let (code_dot, stdout_dot, _) = run_args(&["."]);
        assert_eq!(code_default, code_dot);
        assert_eq!(stdout_default, stdout_dot);
    }
}
