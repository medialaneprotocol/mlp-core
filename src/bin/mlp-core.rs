//! The `mlp-core` command-line tool: the Media Lane Protocol's front door.
//!
//! The tool is a thin shell around [`mlp_core::cli`]: it hands the argument
//! vector to the tool's dispatcher, prints what it produced, and exits with
//! the status the dispatcher decided.
//!
//! ```text
//! mlp-core --help
//! mlp-core fragment build --class media --priority 6 --flags fin --body 00010203
//! mlp-core fragment inspect --hex 4d4c5000...
//! mlp-core limits --json
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let outcome = mlp_core::cli::run(&args);
    print!("{}", outcome.stdout);
    eprint!("{}", outcome.stderr);
    ExitCode::from(outcome.code)
}
