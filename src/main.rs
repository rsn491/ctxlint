//! ctxlint lints agent instruction files (AGENTS.md and SKILL.md) for
//! front-matter correctness and token budget overruns.

mod cli;
mod config;
mod discover;
mod lint;
mod parse;
mod report;
mod tokens;

fn main() {
    use std::io::IsTerminal;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let is_terminal = std::io::stdout().is_terminal();
    let code = cli::run(
        &args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        is_terminal,
    );
    std::process::exit(code);
}
