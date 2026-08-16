# ctxlint

A linter for agents context: `AGENTS.md`, skills, etc.; ensuring it is properly formatted and follows best practices.

Features:
- Detects broken references
- Validates skill frontmatter
- Validates token budgets for skills and AGENTS.md 

## Quick start

Install:
```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/rsn491/ctxlint/releases/latest/download/ctxlint-installer.sh | sh
```

Run in your repo:
```sh
ctxlint .
```

## Usage

```sh
ctxlint [flags] [path...]
```

Paths may be files or directories. Directories are walked recursively for
`AGENTS.md` and `SKILL.md`. 

Examples:
```sh
# lint the whole repository against the default budgets
ctxlint .

# just the skills, with a tighter body budget
ctxlint --max-skill-tokens 2000 skills/

# CI: fail on warnings too, machine-readable output
ctxlint --strict --format json .
```

### Flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `--max-agents-tokens` | `2500` | Body token budget for `AGENTS.md` |
| `--max-skill-tokens` | `5000` | Body token budget for `SKILL.md` |
| `--max-skill-name-tokens` | `16` | Budget for a skill's `name` |
| `--max-skill-description-tokens` | `100` | Budget for a skill's `description` |
| `--exclude` | | Glob of paths to skip; repeatable |
| `--disable` | | Rule id to skip; repeatable |
| `--strict` | `false` | Treat warnings as errors |
| `--quiet` | `false` | Report errors only |
| `--format` | `text` | `text` or `json` |
| `--color` | `auto` | `auto`, `always`, or `never` |
| `--config` | | Read settings from this file instead of searching |
| `--no-config` | `false` | Ignore any config file |

## Configuration file

Rather than repeating flags on every run, put a repository's settings in
`.ctxlint.yaml` (or `.ctxlint.yml`). ctxlint reads the nearest one at or above
the working directory, so it applies whether you run from the repository root
or from a subdirectory.

```yaml
# Token budgets. 0 disables the check.
max-agents-tokens: 2500
max-skill-tokens: 3000
max-skill-name-tokens: 16
max-skill-description-tokens: 100

# Paths to skip, as globs.
exclude:
  - testdata
  - "examples/**"

# Rules, by id. false switches one off.
rules:
  name.dir-mismatch: false
  frontmatter.unknown-key: false

# Run behavior, matching the flags of the same name.
strict: true
quiet: false
format: text
color: auto
```

Every key mirrors the flag of the same name without its leading dashes, plus
the `rules` mapping, which is the file form of `--disable`. Booleans also
accept `yes`/`no` and `on`/`off`, and `exclude` accepts a lone string as well
as a list.

Flags win over the file, and the file wins over the defaults:

```sh
# use the project's settings, but with a tighter skill budget just this once
ctxlint --max-skill-tokens 2000 .

# read a specific file, wherever it lives
ctxlint --config ci/ctxlint.yaml .

# ignore the project's settings entirely
ctxlint --no-config .
```

`--exclude` and `--disable` are the exception: they add to what the file
already lists instead of replacing it.

Unknown settings, unknown rule ids, and values of the wrong type are reported
with the file and line that caused them, and exit 2 rather than being ignored.

### Rules

| Rule | Severity | Check |
| --- | --- | --- |
| `frontmatter.missing` | error | The skill has no front-matter block |
| `frontmatter.not-first` | error | The fence does not open on line 1 |
| `frontmatter.unterminated` | error | The opening fence is never closed |
| `frontmatter.invalid` | error | Not parseable YAML, or not a mapping |
| `frontmatter.unknown-key` | warning | Key outside `name`, `description`, `license`, `compatibility`, `metadata`, `allowed-tools` (the [Agent Skills spec](https://agentskills.io/specification#frontmatter) fields), plus the Claude Code extensions `disable-model-invocation` and `argument-hint` |
| `name.required` | error | `name` is present and a non-empty string |
| `name.format` | error | `name` matches `^[a-z0-9]+(-[a-z0-9]+)*$` |
| `name.length` | error | `name` is at most 64 characters |
| `name.dir-mismatch` | warning | `name` matches the containing directory |
| `description.required` | error | `description` is present and a non-empty string |
| `description.length` | error | `description` is at most 1024 characters |
| `allowed-tools.type` | error | A list of names, or a comma-separated string |
| `metadata.type` | error | A mapping when present |
| `tokens.content` | error | Body within its token budget |
| `tokens.name` | error | `name` within its token budget |
| `tokens.description` | error | `description` within its token budget |
| `file-reference.missing` | error | Every relative markdown link, and every `./`- or `../`-prefixed file path in inline code, resolves to a file that exists |

Switch any of them off by id, on the command line or in the config file:
```sh
ctxlint --disable name.dir-mismatch --disable frontmatter.unknown-key .
```

## Running on CI

```yaml
- name: Lint agent instruction files
  run: |
    cargo install --path .
    ctxlint --strict .
```

With a `.ctxlint.yaml` checked in, the flags can drop out of the workflow
entirely and CI lints with exactly the settings contributors get locally.

## Development

Build:
```sh
cargo build --release
```

Run tests:
```sh
cargo test
```

Format and lint:
```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Pre-commit hooks run `fmt`, `clippy`, `check`, `test`, and ctxlint's own self-lint
automatically. To install them:
```sh
pip install pre-commit
pre-commit install
```
