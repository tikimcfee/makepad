//! `makepad-index`: the `makepad index` verbs as a standalone binary.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(makepad_repo_index::cli::main(&args));
}
