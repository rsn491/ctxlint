//! Splits a markdown file into YAML front matter and body, keeping enough
//! position information to anchor lint findings to the right line.

use std::collections::HashMap;

use regex::Regex;
use saphyr::{LoadableYamlNode, MarkedYaml, Scalar, YamlData};
use std::sync::LazyLock;

/// Classifies a malformed front-matter block. The linter maps each kind onto a
/// rule id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// The fence exists but something precedes it.
    NotFirst,
    /// The opening fence is never closed.
    Unterminated,
    /// The block is not parseable YAML.
    InvalidYaml,
    /// The block parses but is not a key/value mapping.
    NotMapping,
}

/// Describes why a front-matter block could not be used.
#[derive(Debug, Clone)]
pub struct Error {
    pub kind: ErrKind,
    /// 1-based line in the file, 0 when unknown.
    pub line: usize,
    pub msg: String,
}

/// An owned, simplified YAML value: enough shape information for ctxlint's
/// rules without borrowing from the source text.
#[derive(Debug, Clone)]
pub enum Value {
    Scalar(String),
    Sequence(Vec<Value>),
    Mapping,
    Other,
}

struct Entry {
    key_line: usize,
    value: Value,
}

/// A parsed front-matter block.
#[derive(Default)]
pub struct Frontmatter {
    /// Whether the file opened with a front-matter fence.
    pub present: bool,
    /// 1-based line of the closing fence, 0 when absent.
    pub end_line: usize,

    keys: Vec<String>,
    entries: HashMap<String, Entry>,
}

impl Frontmatter {
    /// Returns the mapping keys in document order.
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// Reports whether key is present, regardless of its value.
    pub fn has(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Returns the 1-based line where key is declared, or end_line as a
    /// fallback when the key is absent.
    pub fn line(&self, key: &str) -> usize {
        self.entries
            .get(key)
            .map(|e| e.key_line)
            .unwrap_or(self.end_line)
    }

    /// Returns the value node for key, or None when the key is absent.
    pub fn node(&self, key: &str) -> Option<&Value> {
        self.entries.get(key).map(|e| &e.value)
    }

    /// Returns the scalar value of key. None when the key is absent or its
    /// value is not a scalar.
    pub fn string(&self, key: &str) -> Option<String> {
        match self.node(key) {
            Some(Value::Scalar(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// Returns the value of key as a list of strings, accepting either a YAML
    /// sequence of scalars or a single comma-separated scalar. None when the
    /// key is absent or holds neither shape.
    pub fn string_slice(&self, key: &str) -> Option<Vec<String>> {
        match self.node(key)? {
            Value::Scalar(s) => Some(
                s.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
            Value::Sequence(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Value::Scalar(s) => out.push(s.clone()),
                        _ => return None,
                    }
                }
                Some(out)
            }
            Value::Mapping | Value::Other => None,
        }
    }
}

const BOM: char = '\u{feff}';

static OPEN_FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-{3,}[ \t]*$").unwrap());
static CLOSE_FENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(-{3,}|\.{3,})[ \t]*$").unwrap());

fn fence_line(line: &str) -> &str {
    line.trim_end_matches('\r')
}

/// Separates the front matter of a markdown file from its body.
///
/// A file with no front matter is not an error: `Frontmatter::present` is
/// false and body is the whole file. A malformed block yields an `Error` and a
/// best-effort body so token budgets can still be reported.
pub fn split(src: &[u8]) -> (Frontmatter, String, Option<Error>) {
    let text = String::from_utf8_lossy(src);
    let text = text.strip_prefix(BOM).unwrap_or(&text).to_string();
    let lines: Vec<&str> = text.split('\n').collect();

    let mut open: isize = -1;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if OPEN_FENCE_RE.is_match(fence_line(line)) {
            open = i as isize;
        }
        break;
    }
    if open == -1 {
        return (Frontmatter::default(), text, None);
    }
    if open > 0 {
        return (
            Frontmatter::default(),
            text.clone(),
            Some(Error {
                kind: ErrKind::NotFirst,
                line: (open + 1) as usize,
                msg: "front matter must start on the first line of the file".to_string(),
            }),
        );
    }
    let open = open as usize;

    let close_idx = lines
        .iter()
        .enumerate()
        .skip(open + 1)
        .find(|(_, line)| CLOSE_FENCE_RE.is_match(fence_line(line)))
        .map(|(i, _)| i);
    let Some(close_idx) = close_idx else {
        return (
            Frontmatter {
                present: true,
                ..Default::default()
            },
            text.clone(),
            Some(Error {
                kind: ErrKind::Unterminated,
                line: open + 1,
                msg: "front matter is not closed by a --- fence".to_string(),
            }),
        );
    };

    let block = lines[open + 1..close_idx].join("\n");
    let body = if close_idx + 1 < lines.len() {
        lines[close_idx + 1..].join("\n")
    } else {
        String::new()
    };

    let mut fm = Frontmatter {
        present: true,
        end_line: close_idx + 1,
        ..Default::default()
    };

    let docs = match MarkedYaml::load_from_str(&block) {
        Ok(docs) => docs,
        Err(e) => {
            return (
                fm,
                body,
                Some(Error {
                    kind: ErrKind::InvalidYaml,
                    line: e.marker().line() + open + 1,
                    msg: format!("invalid YAML in front matter: {}", clean_yaml_err(e.info())),
                }),
            );
        }
    };

    let Some(root) = docs.into_iter().next() else {
        return (
            fm,
            body,
            Some(Error {
                kind: ErrKind::NotMapping,
                line: open + 1,
                msg: "front matter is empty".to_string(),
            }),
        );
    };

    let YamlData::Mapping(map) = &root.data else {
        return (
            fm,
            body,
            Some(Error {
                kind: ErrKind::NotMapping,
                line: root.span.start.line() + open + 1,
                msg: "front matter must be a mapping of keys to values".to_string(),
            }),
        );
    };

    for (key, value) in map.iter() {
        let Some(key_str) = scalar_string(&key.data) else {
            continue;
        };
        let key_line = key.span.start.line() + open + 1;
        if !fm.entries.contains_key(&key_str) {
            fm.keys.push(key_str.clone());
        }
        fm.entries.insert(
            key_str,
            Entry {
                key_line,
                value: to_value(value),
            },
        );
    }

    (fm, body, None)
}

/// Renders a scalar YAML node as a string. Shared with the config loader,
/// which reads the same flavor of YAML.
pub fn scalar_string<'a>(data: &YamlData<'a, MarkedYaml<'a>>) -> Option<String> {
    match data {
        YamlData::Value(scalar) => Some(stringify_scalar(scalar)),
        YamlData::Representation(cow, ..) => Some(cow.to_string()),
        _ => None,
    }
}

fn stringify_scalar(scalar: &Scalar) -> String {
    match scalar {
        Scalar::Null => "null".to_string(),
        Scalar::Boolean(b) => b.to_string(),
        Scalar::Integer(i) => i.to_string(),
        Scalar::FloatingPoint(f) => f.into_inner().to_string(),
        Scalar::String(s) => s.to_string(),
    }
}

fn to_value(node: &MarkedYaml) -> Value {
    match &node.data {
        YamlData::Value(scalar) => Value::Scalar(stringify_scalar(scalar)),
        YamlData::Representation(cow, ..) => Value::Scalar(cow.to_string()),
        YamlData::Sequence(items) => Value::Sequence(items.iter().map(to_value).collect()),
        YamlData::Mapping(_) => Value::Mapping,
        YamlData::Alias(_) | YamlData::BadValue | YamlData::Tagged(..) => Value::Other,
    }
}

/// Trims saphyr's positional suffix so a finding stays on one line and does
/// not repeat the line number the caller already reports separately.
fn clean_yaml_err(info: &str) -> String {
    info.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_valid() {
        let src =
            "---\nname: my-skill\ndescription: Does a thing.\n---\n\n# Heading\n\nBody text.\n";
        let (fm, body, err) = split(src.as_bytes());
        assert!(err.is_none());
        assert!(fm.present);
        assert_eq!(fm.keys(), &["name".to_string(), "description".to_string()]);
        assert_eq!(fm.string("name").as_deref(), Some("my-skill"));
        assert_eq!(fm.line("name"), 2);
        assert_eq!(fm.line("description"), 3);
        assert_eq!(fm.end_line, 4);
        assert!(body.starts_with("\n# Heading"));
        assert!(!body.contains("name:"));
    }

    #[test]
    fn split_no_frontmatter() {
        let src = "# Just markdown\n\nNo front matter here.\n";
        let (fm, body, err) = split(src.as_bytes());
        assert!(err.is_none());
        assert!(!fm.present);
        assert_eq!(body, src);
    }

    #[test]
    fn split_bom() {
        let src = "\u{feff}---\nname: bom-skill\n---\nbody\n";
        let (fm, _, err) = split(src.as_bytes());
        assert!(err.is_none());
        assert_eq!(fm.string("name").as_deref(), Some("bom-skill"));
    }

    #[test]
    fn split_crlf() {
        let src = "---\r\nname: crlf-skill\r\n---\r\nbody\r\n";
        let (fm, _, err) = split(src.as_bytes());
        assert!(err.is_none());
        assert_eq!(fm.string("name").as_deref(), Some("crlf-skill"));
    }

    #[test]
    fn split_errors() {
        let cases: &[(&str, &str, ErrKind, usize)] = &[
            (
                "fence not first",
                "\n---\nname: late\n---\n",
                ErrKind::NotFirst,
                2,
            ),
            (
                "unterminated",
                "---\nname: open-ended\nbody without a closing fence\n",
                ErrKind::Unterminated,
                1,
            ),
            (
                "invalid yaml",
                "---\nname: ok\n  bad: indent\n---\nbody\n",
                ErrKind::InvalidYaml,
                3,
            ),
            (
                "not a mapping",
                "---\n- one\n- two\n---\nbody\n",
                ErrKind::NotMapping,
                2,
            ),
            ("empty block", "---\n---\nbody\n", ErrKind::NotMapping, 1),
        ];
        for (name, src, want_kind, want_line) in cases {
            let (_, body, err) = split(src.as_bytes());
            let err = err.unwrap_or_else(|| panic!("{name}: want an error"));
            assert_eq!(err.kind, *want_kind, "{name}: kind");
            assert_eq!(err.line, *want_line, "{name}: line (message: {})", err.msg);
            assert!(!body.is_empty(), "{name}: body should be non-empty");
            assert!(
                !err.msg.contains('\n'),
                "{name}: message should be one line"
            );
        }
    }

    #[test]
    fn string_slice_variants() {
        let cases: &[(&str, &str, Option<&[&str]>)] = &[
            (
                "sequence",
                "---\nallowed-tools:\n  - Read\n  - Bash\n---\n",
                Some(&["Read", "Bash"]),
            ),
            (
                "comma separated",
                "---\nallowed-tools: Read, Bash\n---\n",
                Some(&["Read", "Bash"]),
            ),
            (
                "flow sequence",
                "---\nallowed-tools: [Read, Bash]\n---\n",
                Some(&["Read", "Bash"]),
            ),
            (
                "mapping is rejected",
                "---\nallowed-tools:\n  read: true\n---\n",
                None,
            ),
            (
                "nested sequence is rejected",
                "---\nallowed-tools:\n  - [Read]\n---\n",
                None,
            ),
        ];
        for (name, src, want) in cases {
            let (fm, _, err) = split(src.as_bytes());
            assert!(err.is_none(), "{name}: {:?}", err.map(|e| e.msg));
            let got = fm.string_slice("allowed-tools");
            match want {
                Some(want) => {
                    let got = got.unwrap_or_else(|| panic!("{name}: want Some, got None"));
                    assert_eq!(got, *want, "{name}");
                }
                None => assert!(got.is_none(), "{name}: got {got:?}"),
            }
        }
    }

    #[test]
    fn accessors_on_absent_keys() {
        let (fm, _, err) = split(b"---\nname: only-name\n---\n");
        assert!(err.is_none());
        assert!(!fm.has("license"));
        assert!(fm.node("license").is_none());
        assert!(fm.string("license").is_none());
        assert_eq!(fm.line("license"), fm.end_line);
    }

    #[test]
    fn node_kind() {
        let (fm, _, err) = split(b"---\nmetadata:\n  owner: infra\n---\n");
        assert!(err.is_none());
        assert!(matches!(fm.node("metadata"), Some(Value::Mapping)));
    }
}
