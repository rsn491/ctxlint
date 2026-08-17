//! Wires flags, discovery, linting and reporting together.

use std::io::Write;

use crate::discover;
use crate::lint::{
    self, Config, DEFAULT_MAX_AGENTS_TOKENS, DEFAULT_MAX_SKILL_DESCRIPTION_TOKENS,
    DEFAULT_MAX_SKILL_NAME_TOKENS, DEFAULT_MAX_SKILL_TOKENS,
};
use crate::report;

/// No errors were found; warnings alone still exit OK.
pub const EXIT_OK: i32 = 0;
/// At least one error-severity finding was reported.
pub const EXIT_FINDINGS: i32 = 1;
/// The run could not happen: bad flags or unreadable files.
pub const EXIT_USAGE: i32 = 2;

/// The reported build version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Flags {
    max_agents_tokens: i64,
    max_skill_tokens: i64,
    max_skill_name_tokens: i64,
    max_skill_description_tokens: i64,
    format: String,
    color: String,
    strict: bool,
    quiet: bool,
    show_version: bool,
    list_rules: bool,
    excludes: Vec<String>,
    disabled: Vec<String>,
    paths: Vec<String>,
}

impl Default for Flags {
    fn default() -> Self {
        // Budgets come from Config so the flag defaults, the usage text and
        // the library default are one value each.
        let cfg = Config::default();
        Flags {
            max_agents_tokens: cfg.max_agents_tokens,
            max_skill_tokens: cfg.max_skill_tokens,
            max_skill_name_tokens: cfg.max_skill_name_tokens,
            max_skill_description_tokens: cfg.max_skill_description_tokens,
            format: "text".to_string(),
            color: "auto".to_string(),
            strict: false,
            quiet: false,
            show_version: false,
            list_rules: false,
            excludes: Vec::new(),
            disabled: Vec::new(),
            paths: Vec::new(),
        }
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
            "strict" => f.strict = inline.is_none() || parse_bool_inline(inline),
            "quiet" => f.quiet = inline.is_none() || parse_bool_inline(inline),
            "max-agents-tokens" => {
                let raw = next_value!("max-agents-tokens", inline);
                f.max_agents_tokens = parse_int!("max-agents-tokens", raw);
            }
            "max-skill-tokens" => {
                let raw = next_value!("max-skill-tokens", inline);
                f.max_skill_tokens = parse_int!("max-skill-tokens", raw);
            }
            "max-skill-name-tokens" => {
                let raw = next_value!("max-skill-name-tokens", inline);
                f.max_skill_name_tokens = parse_int!("max-skill-name-tokens", raw);
            }
            "max-skill-description-tokens" => {
                let raw = next_value!("max-skill-description-tokens", inline);
                f.max_skill_description_tokens = parse_int!("max-skill-description-tokens", raw);
            }
            "format" => f.format = next_value!("format", inline),
            "color" => f.color = next_value!("color", inline),
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

    if f.format != "text" && f.format != "json" {
        let _ = writeln!(
            stderr,
            "ctxlint: unknown --format {:?}: want text or json",
            f.format
        );
        return EXIT_USAGE;
    }
    if f.color != "auto" && f.color != "always" && f.color != "never" {
        let _ = writeln!(
            stderr,
            "ctxlint: unknown --color {:?}: want auto, always, or never",
            f.color
        );
        return EXIT_USAGE;
    }
    if let Err(msg) = check_rule_names(&f.disabled) {
        let _ = writeln!(stderr, "ctxlint: {msg}");
        return EXIT_USAGE;
    }

    let paths: Vec<String> = if f.paths.is_empty() {
        vec![".".to_string()]
    } else {
        f.paths
    };

    let targets = match discover::find(&paths, &f.excludes) {
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
    // A closed pipe is how `ctxlint . | head` ends, not a failure: report
    // whatever the findings warrant and stay quiet, rather than exiting like a
    // bad flag. Every other write failure is still worth reporting.
    if let Err(e) = write_result
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
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

Exit codes: 0 clean (warnings still exit 0), 1 errors found, 2 bad usage.

Flags:
  --max-agents-tokens int              token budget for AGENTS.md content, 0 disables (default {DEFAULT_MAX_AGENTS_TOKENS})
  --max-skill-tokens int                token budget for SKILL.md content, 0 disables (default {DEFAULT_MAX_SKILL_TOKENS})
  --max-skill-name-tokens int           token budget for a skill's name, 0 disables (default {DEFAULT_MAX_SKILL_NAME_TOKENS})
  --max-skill-description-tokens int    token budget for a skill's description, 0 disables (default {DEFAULT_MAX_SKILL_DESCRIPTION_TOKENS})
  --exclude glob                        glob of paths to skip; repeatable
  --disable rule                        rule id to skip; repeatable
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

    fn run_args(args: &[&str]) -> (i32, String, String) {
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

    /// Fails every write with a fixed `ErrorKind`, standing in for a closed
    /// pipe without needing a real one.
    struct FailingWriter(std::io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(self.0))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(self.0))
        }
    }

    fn run_into_failing_writer(kind: std::io::ErrorKind, path: &str) -> (i32, String) {
        let args = vec![path.to_string()];
        let mut err = Vec::new();
        let code = run(&args, &mut FailingWriter(kind), &mut err, false);
        (code, String::from_utf8(err).unwrap())
    }

    #[test]
    fn broken_pipe_is_not_a_usage_error() {
        use std::io::ErrorKind;

        // Findings still set the exit code, and nothing is said about the pipe.
        let (code, stderr) = run_into_failing_writer(ErrorKind::BrokenPipe, &fixture(&["broken"]));
        assert_eq!(code, EXIT_FINDINGS, "stderr={stderr}");
        assert!(stderr.is_empty(), "stderr={stderr}");

        let (code, stderr) = run_into_failing_writer(ErrorKind::BrokenPipe, &fixture(&["clean"]));
        assert_eq!(code, EXIT_OK, "stderr={stderr}");
        assert!(stderr.is_empty(), "stderr={stderr}");

        // Other write failures are still reported, so this did not widen into
        // swallowing real I/O errors.
        let (code, stderr) =
            run_into_failing_writer(ErrorKind::PermissionDenied, &fixture(&["broken"]));
        assert_eq!(code, EXIT_USAGE);
        assert!(stderr.contains("ctxlint:"), "stderr={stderr}");
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
        assert!(stderr.contains("unknown --color"), "{stderr}");
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
            (
                "unknown format",
                vec!["--format", "xml"],
                "unknown --format",
            ),
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
                "invalid --exclude",
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
