# Working in ctxlint

`ctxlint` lints agent instruction files. It is a Rust CLI; the YAML front
matter is parsed with `saphyr`, everything else is hand-rolled.

## Checks before pushing

```sh
cargo fmt --check       # must report nothing
cargo clippy --all-targets -- -D warnings
cargo test
```

## Layout

- `src/main.rs` — entry point only; all logic lives in the other modules.
- `src/cli.rs` — flags, orchestration, exit codes. `run` takes its writers as
  arguments so tests drive the whole CLI in-process. `Flags` holds `Option`s so
  an unset flag can fall through to the config file; `resolve` merges flags over
  the file over the defaults.
- `src/config.rs` — the `.ctxlint.yaml` loader and its discovery walk.
- `src/discover.rs` — turns paths into targets, prunes dependency directories.
- `src/parse.rs` — splits YAML front matter from the body, keeping line numbers.
- `src/tokens.rs` — heuristic token estimator behind the `Counter` trait.
- `src/lint.rs` — the rules.
- `src/report.rs` — text and JSON renderers.

## Adding a rule

1. Add its id constant in `src/lint.rs` and append it to `RULES`, which backs
   both `--list-rules` and `--disable` validation.
2. Implement the check, emitting through the `Reporter::add` method so
   `--disable` and `--strict` keep working.
3. Add a case to the table in `lint.rs`'s test module asserting the exact rule
   ids the fixture produces.
4. Document it in the README's rule table. Nothing else is needed for
   `--disable` or the config file's `rules:` mapping: both validate against
   `RULES`.

## Adding a setting

A setting that belongs in a project's config file needs three edits: an
`Option` field on `cli.rs`'s `Flags` plus its arm in `parse_args`, a key in
`config.rs`'s `KNOWN_KEYS` and its arm in `parse`, and a line in `resolve` that
picks flag over file over default. Document it in both the README's flag table
and its config file example.

## Conventions

- Prefer errors for anything that breaks a file's contract with the runtime, and
  warnings for stylistic mismatches. Warnings alone exit 0.
- Findings are sorted by rule order, never by the order checks happen to run in,
  so output stays stable.
- Comments explain why a check exists, not what the code does.
