//! Turns command-line paths into the set of agent instruction files to lint.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use glob::Pattern;

/// Distinguishes the two file types ctxlint understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Agents,
    Skill,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Kind::Agents => write!(f, "agents"),
            Kind::Skill => write!(f, "skill"),
        }
    }
}

/// A single file to lint.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: String,
    pub kind: Kind,
    /// The tree this target was found under: the directory argument that
    /// produced it, or the file's own directory when named explicitly. Rules
    /// use it to tell a reference into the linted tree from one that escapes.
    pub root: PathBuf,
}

/// Never walked: holds dependencies and build output, whose instruction files
/// are not the user's to fix.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".next",
    ".venv",
    "build",
    "dist",
    "node_modules",
    "target",
    "vendor",
];

/// Classifies a path by its base name, case-insensitively. Returns None when
/// the name is not an agent instruction file.
pub fn kind_for(path: &Path) -> Option<Kind> {
    let name = path.file_name()?.to_str()?.to_lowercase();
    match name.as_str() {
        "agents.md" => Some(Kind::Agents),
        "skill.md" => Some(Kind::Skill),
        _ => None,
    }
}

/// Collects the files to lint from the given paths. A file argument is taken
/// as-is; a directory is walked recursively. Excludes are shell globs matched
/// against each entry's base name and its path relative to the walk root, and
/// a matching directory is pruned.
///
/// The result is deduplicated by absolute path and sorted, so output is
/// stable across runs.
pub fn find(paths: &[String], excludes: &[String]) -> Result<Vec<Target>, String> {
    let patterns = compile_globs(excludes)?;

    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    let mut add = |path: &Path, kind: Kind, root: &Path| {
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        if seen.insert(abs) {
            targets.push(Target {
                path: to_slash(path),
                kind,
                root: root.to_path_buf(),
            });
        }
    };

    for path in paths {
        let path = Path::new(path);
        let meta =
            fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if !meta.is_dir() {
            // A file named outright has no walk root, so it stands as its own:
            // its directory is the tree we were pointed at.
            let root = path.parent().unwrap_or(Path::new("."));
            match kind_for(path) {
                Some(kind) => add(path, kind, root),
                None => {
                    return Err(format!(
                        "{} is not an AGENTS.md or SKILL.md",
                        path.display()
                    ));
                }
            }
            continue;
        }
        walk(path, path, &patterns, &mut add)?;
    }

    targets.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(targets)
}

fn walk(
    root: &Path,
    dir: &Path,
    patterns: &[Pattern],
    add: &mut impl FnMut(&Path, Kind, &Path),
) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e: std::io::Error| format!("cannot read {}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let rel = || to_slash(path.strip_prefix(root).unwrap_or(&path));

        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref())
                || (!patterns.is_empty() && matches_any(patterns, &name, &rel()))
            {
                continue;
            }
            walk(root, &path, patterns, add)?;
            continue;
        }
        let Some(kind) = kind_for(&path) else {
            continue;
        };
        if !patterns.is_empty() && matches_any(patterns, &name, &rel()) {
            continue;
        }
        add(&path, kind, root);
    }
    Ok(())
}

fn matches_any(patterns: &[Pattern], name: &str, rel: &str) -> bool {
    patterns.iter().any(|p| p.matches(name) || p.matches(rel))
}

/// Rejects malformed patterns up front rather than silently matching nothing
/// on every path.
fn compile_globs(globs: &[String]) -> Result<Vec<Pattern>, String> {
    globs
        .iter()
        .map(|g| Pattern::new(g).map_err(|e| format!("invalid --exclude pattern {g:?}: {e}")))
        .collect()
}

fn to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, contents) in files {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        dir
    }

    fn rel_paths(root: &Path, targets: &[Target]) -> Vec<String> {
        targets
            .iter()
            .map(|t| {
                let rel = Path::new(&t.path).strip_prefix(root).unwrap();
                format!("{} ({})", to_slash(rel), t.kind)
            })
            .collect()
    }

    #[test]
    fn kind_for_matches_expected_names() {
        let cases: &[(&str, Option<Kind>)] = &[
            ("AGENTS.md", Some(Kind::Agents)),
            ("agents.md", Some(Kind::Agents)),
            ("a/b/Agents.MD", Some(Kind::Agents)),
            ("SKILL.md", Some(Kind::Skill)),
            ("skill.md", Some(Kind::Skill)),
            ("SKILLS.md", None),
            ("README.md", None),
            ("AGENT.md", None),
            ("agents.markdown", None),
            ("AGENTS.md.bak", None),
        ];
        for (path, want) in cases {
            assert_eq!(kind_for(Path::new(path)), *want, "{path}");
        }
    }

    #[test]
    fn find_walks_recursively() {
        let dir = tree(&[
            ("AGENTS.md", "root instructions"),
            ("README.md", "not an instruction file"),
            ("docs/AGENTS.md", "nested instructions"),
            ("skills/alpha/SKILL.md", "alpha"),
            ("skills/beta/skill.md", "beta, lowercase"),
            ("skills/gamma/SKILLS.md", "gamma, plural, not matched"),
            ("node_modules/pkg/SKILL.md", "dependency skill"),
            (".git/hooks/AGENTS.md", "git internals"),
            ("vendor/dep/AGENTS.md", "vendored"),
            ("dist/AGENTS.md", "build output"),
            ("skills/alpha/reference/doc.md", "supporting doc"),
        ]);
        let targets = find(&[dir.path().to_string_lossy().to_string()], &[]).unwrap();
        let want = vec![
            "AGENTS.md (agents)".to_string(),
            "docs/AGENTS.md (agents)".to_string(),
            "skills/alpha/SKILL.md (skill)".to_string(),
            "skills/beta/skill.md (skill)".to_string(),
        ];
        assert_eq!(rel_paths(dir.path(), &targets), want);
    }

    #[test]
    fn find_excludes() {
        let dir = tree(&[
            ("AGENTS.md", "root"),
            ("examples/AGENTS.md", "example"),
            ("skills/keep/SKILL.md", "keep"),
            ("skills/drop/SKILL.md", "drop"),
            ("fixtures/bad/SKILL.md", "fixture"),
        ]);
        let root = dir.path().to_string_lossy().to_string();

        let cases: &[(&[&str], &[&str])] = &[
            (
                &["examples"],
                &[
                    "AGENTS.md (agents)",
                    "fixtures/bad/SKILL.md (skill)",
                    "skills/drop/SKILL.md (skill)",
                    "skills/keep/SKILL.md (skill)",
                ],
            ),
            (
                &["skills/drop"],
                &[
                    "AGENTS.md (agents)",
                    "examples/AGENTS.md (agents)",
                    "fixtures/bad/SKILL.md (skill)",
                    "skills/keep/SKILL.md (skill)",
                ],
            ),
            (
                &["examples", "fixtures"],
                &[
                    "AGENTS.md (agents)",
                    "skills/drop/SKILL.md (skill)",
                    "skills/keep/SKILL.md (skill)",
                ],
            ),
            (
                &["AGENTS.md"],
                &[
                    "fixtures/bad/SKILL.md (skill)",
                    "skills/drop/SKILL.md (skill)",
                    "skills/keep/SKILL.md (skill)",
                ],
            ),
        ];
        for (excludes, want) in cases {
            let excludes: Vec<String> = excludes.iter().map(|s| s.to_string()).collect();
            let targets = find(std::slice::from_ref(&root), &excludes).unwrap();
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                rel_paths(dir.path(), &targets),
                want,
                "excludes = {excludes:?}"
            );
        }
    }

    #[test]
    fn find_explicit_file() {
        let dir = tree(&[
            ("nested/deep/SKILL.md", "a skill somewhere"),
            ("README.md", "not an instruction file"),
        ]);
        let file = dir.path().join("nested/deep/SKILL.md");
        let targets = find(&[file.to_string_lossy().to_string()], &[]).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, Kind::Skill);

        let skipped = tree(&[("node_modules/pkg/SKILL.md", "dependency skill")]);
        let file = skipped.path().join("node_modules/pkg/SKILL.md");
        let targets = find(&[file.to_string_lossy().to_string()], &[]).unwrap();
        assert_eq!(
            targets.len(),
            1,
            "an explicit file inside a skipped directory is still linted"
        );
    }

    #[test]
    fn find_errors() {
        let dir = tree(&[("README.md", "not an instruction file")]);

        let err = find(
            &[dir.path().join("README.md").to_string_lossy().to_string()],
            &[],
        )
        .unwrap_err();
        assert!(err.contains("not an AGENTS.md or SKILL.md"), "{err}");

        assert!(
            find(
                &[dir.path().join("nope").to_string_lossy().to_string()],
                &[]
            )
            .is_err()
        );

        let err = find(
            &[dir.path().to_string_lossy().to_string()],
            &["[".to_string()],
        )
        .unwrap_err();
        assert!(err.contains("invalid --exclude"), "{err}");
    }

    #[test]
    fn find_deduplicates() {
        let dir = tree(&[("AGENTS.md", "root")]);
        let root = dir.path().to_string_lossy().to_string();
        let file = dir.path().join("AGENTS.md").to_string_lossy().to_string();
        let targets = find(&[root, file.clone(), file], &[]).unwrap();
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn find_empty_result() {
        let dir = tree(&[("README.md", "nothing to lint here")]);
        let targets = find(&[dir.path().to_string_lossy().to_string()], &[]).unwrap();
        assert!(targets.is_empty());
    }
}
