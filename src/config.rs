//! Reads run settings from a YAML config file, so a repository can pin its
//! budgets and rule switches once instead of repeating flags on every
//! invocation.
//!
//! Every setting mirrors the flag of the same name. Flags win over the file,
//! and the file wins over the built-in defaults, so a config file sets the
//! project's baseline without preventing a one-off override on the command
//! line.

use std::path::{Path, PathBuf};

use saphyr::{LoadableYamlNode, MarkedYaml, YamlData};

use crate::lint::RULES;
use crate::parse::scalar_string;

/// File names looked for when `--config` is not given, in priority order.
pub const FILE_NAMES: &[&str] = &[".ctxlint.yaml", ".ctxlint.yml"];

/// Every setting a config file may carry, in the order the usage text lists
/// them. Backs both parsing and the "unknown setting" message.
const KNOWN_KEYS: &[&str] = &[
    "max-agents-tokens",
    "max-skill-tokens",
    "max-skill-name-tokens",
    "max-skill-description-tokens",
    "exclude",
    "rules",
    "strict",
    "quiet",
    "format",
    "color",
];

/// The settings one config file asked for. Every scalar is optional: absent
/// means "leave it to the flag or the default", which is what makes merging
/// unambiguous.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub max_agents_tokens: Option<i64>,
    pub max_skill_tokens: Option<i64>,
    pub max_skill_name_tokens: Option<i64>,
    pub max_skill_description_tokens: Option<i64>,
    pub strict: Option<bool>,
    pub quiet: Option<bool>,
    pub format: Option<String>,
    pub color: Option<String>,
    /// Globs of paths to skip, appended to any `--exclude`.
    pub excludes: Vec<String>,
    /// Rule ids switched off under `rules:`, appended to any `--disable`.
    pub disabled: Vec<String>,
}

/// Returns the config file governing `start`: the first of [`FILE_NAMES`]
/// found in `start` or any ancestor directory. Walking up means running
/// ctxlint from a subdirectory still picks up the repository's settings.
pub fn discover(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        for name in FILE_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Reads and parses a config file.
pub fn load(path: &Path) -> Result<Settings, String> {
    let display = path.to_string_lossy().replace('\\', "/");
    let src = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
    parse(&src, &display)
}

/// Parses config text. `path` only labels the error messages.
pub fn parse(src: &str, path: &str) -> Result<Settings, String> {
    let mut cfg = Settings::default();

    let docs = MarkedYaml::load_from_str(src).map_err(|e| {
        format!(
            "{path}:{}: invalid YAML: {}",
            e.marker().line(),
            e.info().trim()
        )
    })?;
    // A file that is empty or holds only comments parses to no document, and
    // one holding a bare `---` to a null document. Both mean "no settings".
    let Some(root) = docs.into_iter().next() else {
        return Ok(cfg);
    };
    if is_null(&root) {
        return Ok(cfg);
    }
    let YamlData::Mapping(map) = &root.data else {
        return Err(format!(
            "{path}:{}: config must be a mapping of settings to values",
            root.span.start.line()
        ));
    };

    for (key, value) in map.iter() {
        let line = key.span.start.line();
        let Some(name) = scalar_string(&key.data) else {
            return Err(format!("{path}:{line}: setting names must be strings"));
        };
        match name.as_str() {
            "max-agents-tokens" => cfg.max_agents_tokens = Some(budget(path, &name, value)?),
            "max-skill-tokens" => cfg.max_skill_tokens = Some(budget(path, &name, value)?),
            "max-skill-name-tokens" => {
                cfg.max_skill_name_tokens = Some(budget(path, &name, value)?)
            }
            "max-skill-description-tokens" => {
                cfg.max_skill_description_tokens = Some(budget(path, &name, value)?);
            }
            "strict" => cfg.strict = Some(boolean(path, &name, value)?),
            "quiet" => cfg.quiet = Some(boolean(path, &name, value)?),
            "format" => cfg.format = Some(string(path, &name, value)?),
            "color" => cfg.color = Some(string(path, &name, value)?),
            "exclude" => cfg.excludes = string_list(path, &name, value)?,
            "rules" => cfg.disabled = rules(path, value)?,
            _ => {
                return Err(format!(
                    "{path}:{line}: unknown setting {name:?}: want one of {}",
                    KNOWN_KEYS.join(", ")
                ));
            }
        }
    }

    Ok(cfg)
}

/// Reads `rules:`, a mapping of rule id to whether it runs. Only the ids
/// switched off are returned; naming a rule that ctxlint does not have is an
/// error rather than a silent no-op, the same way `--disable` rejects typos.
fn rules(path: &str, node: &MarkedYaml) -> Result<Vec<String>, String> {
    let line = node.span.start.line();
    let YamlData::Mapping(map) = &node.data else {
        return Err(format!(
            "{path}:{line}: rules must be a mapping of rule id to true or false"
        ));
    };

    let mut disabled = Vec::new();
    for (key, value) in map.iter() {
        let key_line = key.span.start.line();
        let Some(rule) = scalar_string(&key.data) else {
            return Err(format!("{path}:{key_line}: rule ids must be strings"));
        };
        if !RULES.contains(&rule.as_str()) {
            return Err(format!(
                "{path}:{key_line}: unknown rule {rule:?}: run --list-rules to see them all"
            ));
        }
        if !boolean(path, &format!("rules.{rule}"), value)? {
            disabled.push(rule);
        }
    }
    Ok(disabled)
}

/// Reads a token budget. Zero disables the check it belongs to, matching the
/// flags.
fn budget(path: &str, key: &str, node: &MarkedYaml) -> Result<i64, String> {
    let line = node.span.start.line();
    let text = scalar_string(&node.data)
        .ok_or_else(|| format!("{path}:{line}: {key} must be an integer"))?;
    let n: i64 = text
        .parse()
        .map_err(|_| format!("{path}:{line}: invalid value {text:?} for {key}: not an integer"))?;
    if n < 0 {
        return Err(format!(
            "{path}:{line}: {key} must be zero or more (0 disables the check)"
        ));
    }
    Ok(n)
}

/// Reads a boolean. YAML 1.2 only spells these `true` and `false`, but config
/// files are hand-written, so the usual `yes`/`no` and `on`/`off` spellings
/// are accepted rather than reported as a type error.
fn boolean(path: &str, key: &str, node: &MarkedYaml) -> Result<bool, String> {
    let line = node.span.start.line();
    let text = scalar_string(&node.data)
        .ok_or_else(|| format!("{path}:{line}: {key} must be true or false"))?;
    match text.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Ok(true),
        "false" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "{path}:{line}: invalid value {text:?} for {key}: want true or false"
        )),
    }
}

fn string(path: &str, key: &str, node: &MarkedYaml) -> Result<String, String> {
    let line = node.span.start.line();
    match scalar_string(&node.data) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(format!("{path}:{line}: {key} must be a non-empty string")),
    }
}

/// Reads a list of strings, accepting a lone scalar as a one-element list so
/// `exclude: testdata` works as well as the sequence form.
fn string_list(path: &str, key: &str, node: &MarkedYaml) -> Result<Vec<String>, String> {
    match &node.data {
        YamlData::Sequence(items) => items.iter().map(|item| string(path, key, item)).collect(),
        _ => Ok(vec![string(path, key, node)?]),
    }
}

fn is_null(node: &MarkedYaml) -> bool {
    matches!(&node.data, YamlData::Value(saphyr::Scalar::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parse_str(src: &str) -> Result<Settings, String> {
        parse(src, ".ctxlint.yaml")
    }

    #[test]
    fn parses_every_setting() {
        let src = r#"
max-agents-tokens: 1200
max-skill-tokens: 0
max-skill-name-tokens: 8
max-skill-description-tokens: 64
strict: true
quiet: false
format: json
color: never
exclude:
  - testdata
  - "examples/**"
rules:
  name.dir-mismatch: false
  frontmatter.unknown-key: off
  tokens.content: true
"#;
        let got = parse_str(src).unwrap();
        assert_eq!(
            got,
            Settings {
                max_agents_tokens: Some(1200),
                max_skill_tokens: Some(0),
                max_skill_name_tokens: Some(8),
                max_skill_description_tokens: Some(64),
                strict: Some(true),
                quiet: Some(false),
                format: Some("json".to_string()),
                color: Some("never".to_string()),
                excludes: vec!["testdata".to_string(), "examples/**".to_string()],
                // Only the rules switched off are collected, in file order.
                disabled: vec![
                    "name.dir-mismatch".to_string(),
                    "frontmatter.unknown-key".to_string(),
                ],
            }
        );
    }

    #[test]
    fn empty_documents_yield_no_settings() {
        for src in ["", "\n\n", "# just a comment\n", "---\n"] {
            let got = parse_str(src).unwrap_or_else(|e| panic!("{src:?}: {e}"));
            assert_eq!(got, Settings::default(), "{src:?}");
        }
    }

    #[test]
    fn scalar_shorthands() {
        let got = parse_str("exclude: testdata\nstrict: yes\n").unwrap();
        assert_eq!(got.excludes, vec!["testdata".to_string()]);
        assert_eq!(got.strict, Some(true));
    }

    #[test]
    fn errors_point_at_the_offending_line() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "unknown setting",
                "max-skill-tokens: 10\nmax-skil-tokens: 10\n",
                ".ctxlint.yaml:2: unknown setting \"max-skil-tokens\"",
            ),
            (
                "not an integer",
                "max-agents-tokens: lots\n",
                ".ctxlint.yaml:1: invalid value \"lots\" for max-agents-tokens",
            ),
            (
                "negative budget",
                "\nmax-skill-tokens: -1\n",
                ".ctxlint.yaml:2: max-skill-tokens must be zero or more",
            ),
            (
                "budget is a list",
                "max-skill-tokens:\n  - 10\n",
                ".ctxlint.yaml:2: max-skill-tokens must be an integer",
            ),
            (
                "bad boolean",
                "strict: sometimes\n",
                ".ctxlint.yaml:1: invalid value \"sometimes\" for strict",
            ),
            (
                "empty string",
                "format: \"\"\n",
                ".ctxlint.yaml:1: format must be a non-empty string",
            ),
            (
                "exclude entry is a mapping",
                "exclude:\n  - drop: true\n",
                ".ctxlint.yaml:2: exclude must be a non-empty string",
            ),
            (
                "rules is not a mapping",
                "rules:\n  - name.format\n",
                ".ctxlint.yaml:2: rules must be a mapping",
            ),
            (
                "unknown rule",
                "rules:\n  name.format: true\n  no.such.rule: false\n",
                ".ctxlint.yaml:3: unknown rule \"no.such.rule\"",
            ),
            (
                "rule value is not a boolean",
                "rules:\n  name.format: maybe\n",
                ".ctxlint.yaml:2: invalid value \"maybe\" for rules.name.format",
            ),
            (
                "not a mapping",
                "- max-skill-tokens\n",
                ".ctxlint.yaml:1: config must be a mapping",
            ),
            (
                "invalid yaml",
                "strict: true\n  quiet: true\n",
                "invalid YAML",
            ),
        ];
        for (name, src, want) in cases {
            let err = parse_str(src).unwrap_err();
            assert!(err.contains(want), "{name}: got {err:?}, want {want:?}");
            assert!(!err.contains('\n'), "{name}: message should be one line");
        }
    }

    #[test]
    fn load_reads_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".ctxlint.yaml");
        fs::write(&path, "max-skill-tokens: 42\n").unwrap();

        let got = load(&path).unwrap();
        assert_eq!(got.max_skill_tokens, Some(42));

        let err = load(&dir.path().join("nope.yaml")).unwrap_err();
        assert!(err.contains("cannot read config"), "{err}");

        // Errors name the file they came from.
        fs::write(&path, "max-skill-tokens: nope\n").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.contains(".ctxlint.yaml:1:"), "{err}");
    }

    #[test]
    fn discover_walks_up_from_the_starting_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("skills/deep");
        fs::create_dir_all(&nested).unwrap();
        assert!(discover(&nested).is_none());

        let root_cfg = dir.path().join(".ctxlint.yml");
        fs::write(&root_cfg, "strict: true\n").unwrap();
        assert_eq!(discover(&nested), Some(root_cfg));

        // .yaml wins over .yml in the same directory, and the nearest
        // directory wins over an ancestor.
        let preferred = dir.path().join(".ctxlint.yaml");
        fs::write(&preferred, "strict: true\n").unwrap();
        assert_eq!(discover(dir.path()), Some(preferred));

        let nested_cfg = nested.join(".ctxlint.yaml");
        fs::write(&nested_cfg, "strict: false\n").unwrap();
        assert_eq!(discover(&nested), Some(nested_cfg));
    }

    #[test]
    fn discover_ignores_a_directory_named_like_the_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".ctxlint.yaml")).unwrap();
        assert!(discover(dir.path()).is_none());
    }
}
