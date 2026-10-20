//! The `mlp-core` command-line tool.
//!
//! The tool is the protocol's front door: it builds and reads the bytes the
//! protocol defines, assembles objects, builds and reads the receiver's
//! acknowledgement, and prints the protocol's hard limits.
//!
//! Two output forms, and they are two different contracts:
//!
//! * the **human** output is a table, for a person reading a terminal;
//! * the **`--json`** output is the machine contract. Its shape is documented
//!   in `docs/cli.md` and pinned by a test, so a script may depend on it.
//!
//! The exit status is a contract as well:
//!
//! | status | meaning |
//! |---|---|
//! | `0` | the command succeeded |
//! | `1` | the check failed — a measurement past its budget |
//! | `2` | the usage was wrong — an unknown verb, a missing or unknown flag |
//! | `3` | the input could not be read — bad hex, a truncated fragment, a body that does not decode |
//!
//! No input makes the tool panic: every refusal is a typed error with a named
//! reason.

pub mod ack;
pub mod fragment;
pub mod limits;
pub mod object;
pub mod wire;

use std::fmt::Write as _;

/// Exit status: the command succeeded.
pub const EXIT_OK: u8 = 0;
/// Exit status: the check failed.
pub const EXIT_CHECK_FAILED: u8 = 1;
/// Exit status: the usage was wrong.
pub const EXIT_USAGE: u8 = 2;
/// Exit status: the input could not be read.
pub const EXIT_INPUT: u8 = 3;

/// Why a command refused to produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// The command line was not understood. Exit status `2`.
    Usage(String),
    /// The input could not be read or could not be decoded. Exit status `3`.
    Input(String),
    /// A check ran and did not pass. Exit status `1`.
    Check(String),
}

impl CliError {
    /// The exit status this error carries.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => EXIT_USAGE,
            Self::Input(_) => EXIT_INPUT,
            Self::Check(_) => EXIT_CHECK_FAILED,
        }
    }

    /// The reason's stable name, for the JSON output.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::Input(_) => "input",
            Self::Check(_) => "check_failed",
        }
    }

    /// The message a person reads.
    pub fn message(&self) -> &str {
        match self {
            Self::Usage(message) | Self::Input(message) | Self::Check(message) => message,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for CliError {}

/// The outcome of one invocation: what to print, and with which status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The exit status.
    pub code: u8,
    /// What goes to standard output.
    pub stdout: String,
    /// What goes to standard error.
    pub stderr: String,
}

impl Outcome {
    /// A successful outcome carrying `stdout`.
    pub fn ok(stdout: String) -> Self {
        Self {
            code: EXIT_OK,
            stdout,
            stderr: String::new(),
        }
    }

    /// A refusal: the message goes to standard error and the status is the
    /// error's own.
    pub fn failed(error: &CliError) -> Self {
        Self {
            code: error.exit_code(),
            stdout: String::new(),
            stderr: format!("{}: {}\n", error.reason(), error.message()),
        }
    }
}

/// What a command produced: the text to print, and the status to exit with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Produced {
    /// The text that goes to standard output.
    pub text: String,
    /// The exit status.
    pub code: u8,
}

impl Produced {
    /// The command succeeded.
    pub fn ok(text: String) -> Self {
        Self {
            text,
            code: EXIT_OK,
        }
    }

    /// The command ran, and the check it ran did not pass.
    pub fn failed(text: String) -> Self {
        Self {
            text,
            code: EXIT_CHECK_FAILED,
        }
    }
}

/// One flag of a command.
#[derive(Debug, Clone, Copy)]
pub struct FlagSpec {
    /// The flag's name, without the leading dashes.
    pub name: &'static str,
    /// Whether the flag takes a value.
    pub takes_value: bool,
    /// What the flag means.
    pub help: &'static str,
}

/// One command of the grammar.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    /// The verb path, for example `["fragment", "build"]`.
    pub path: &'static [&'static str],
    /// What the command does.
    pub summary: &'static str,
    /// The command's flags.
    pub flags: &'static [FlagSpec],
    /// A worked example, printed by `--help`.
    pub example: &'static str,
}

/// The flag every command accepts.
pub const JSON_FLAG: FlagSpec = FlagSpec {
    name: "json",
    takes_value: false,
    help: "print the machine-readable JSON form instead of the table",
};

/// The whole grammar: every command, in the order `--help` lists them.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        path: &["fragment", "build"],
        summary: "Build one fragment from its fields and print its bytes.",
        flags: &[
            FlagSpec {
                name: "class",
                takes_value: true,
                help: "the transport class: control, cursor, media, feedback or path",
            },
            FlagSpec {
                name: "priority",
                takes_value: true,
                help: "the priority field, 0 (highest) to 7",
            },
            FlagSpec {
                name: "flags",
                takes_value: true,
                help: "the flag bits: fin, anchor, retransmit, dup, echo-requested, probe, urgent",
            },
            FlagSpec {
                name: "object-id",
                takes_value: true,
                help: "the object's id",
            },
            FlagSpec {
                name: "fragment-index",
                takes_value: true,
                help: "this fragment's index inside the object",
            },
            FlagSpec {
                name: "fragment-count",
                takes_value: true,
                help: "the object's fragment count",
            },
            FlagSpec {
                name: "body",
                takes_value: true,
                help: "the class body, as hex",
            },
            FlagSpec {
                name: "body-file",
                takes_value: true,
                help: "a file holding the class body as raw bytes",
            },
            FlagSpec {
                name: "tlv",
                takes_value: true,
                help: "a header-extension TLV, as NAME=HEX; repeatable",
            },
            FlagSpec {
                name: "tlv-raw",
                takes_value: true,
                help: "a header-extension TLV, as TYPE=HEX; repeatable",
            },
            JSON_FLAG,
        ],
        example: "mlp-core fragment build --class media --priority 6 --flags fin \
                  --object-id 7 --tlv UPDATE_ID=0100000000000000 --body 00010203",
    },
    CommandSpec {
        path: &["fragment", "inspect"],
        summary: "Read a fragment's bytes and print its header, its TLVs and its body.",
        flags: &[
            FlagSpec {
                name: "hex",
                takes_value: true,
                help: "the fragment's bytes, as hex",
            },
            FlagSpec {
                name: "file",
                takes_value: true,
                help: "a file holding the fragment's bytes, as hex",
            },
            FlagSpec {
                name: "stdin",
                takes_value: false,
                help: "read the fragment's bytes from standard input, as hex",
            },
            FlagSpec {
                name: "carrier",
                takes_value: true,
                help: "the carrier the bytes arrived on: ws, grpc, dc, dc-repair or dc-control",
            },
            JSON_FLAG,
        ],
        example: "mlp-core fragment inspect --hex 4d4c5000010200010700000000010000 \
                  00000000000000deadbeef",
    },
    CommandSpec {
        path: &["object", "assemble"],
        summary: "Assemble fragments into one object and report what the assembler decided.",
        flags: &[
            FlagSpec {
                name: "hex",
                takes_value: true,
                help: "one fragment's bytes, as hex; repeatable",
            },
            FlagSpec {
                name: "file",
                takes_value: true,
                help: "a file holding one fragment's bytes per line, as hex",
            },
            FlagSpec {
                name: "stdin",
                takes_value: false,
                help: "read the fragments' bytes from standard input, one hex fragment per line",
            },
            FlagSpec {
                name: "carrier",
                takes_value: true,
                help: "the carrier the bytes arrived on: ws, grpc, dc, dc-repair or dc-control",
            },
            FlagSpec {
                name: "session-ms",
                takes_value: true,
                help: "the receiver's session-relative clock, for the deadline test",
            },
            JSON_FLAG,
        ],
        example: "mlp-core object assemble --file fragments.hex",
    },
    CommandSpec {
        path: &["ack", "build"],
        summary: "Build the receiver's acknowledgement and print its bytes.",
        flags: &[
            FlagSpec {
                name: "highest-applied-update-id",
                takes_value: true,
                help: "the highest update id the receiver has applied",
            },
            FlagSpec {
                name: "ack-bitmap",
                takes_value: true,
                help: "the selective bitmap over the 64-update window, as hex",
            },
            FlagSpec {
                name: "chain-epoch",
                takes_value: true,
                help: "the receiver's chain epoch",
            },
            FlagSpec {
                name: "applied-cell-count",
                takes_value: true,
                help: "the receiver's cell grid size",
            },
            FlagSpec {
                name: "stale-cells",
                takes_value: true,
                help: "the cells the receiver believes stale",
            },
            FlagSpec {
                name: "converged",
                takes_value: true,
                help: "the receiver's belief: true or false",
            },
            FlagSpec {
                name: "render-latency-ms",
                takes_value: true,
                help: "the measured render latency, in milliseconds",
            },
            FlagSpec {
                name: "echo-of-input-seq",
                takes_value: true,
                help: "the highest input sequence visible in the applied content",
            },
            FlagSpec {
                name: "ack-epoch-ms",
                takes_value: true,
                help: "the sender-relative millisecond the ack was produced at",
            },
            JSON_FLAG,
        ],
        example: "mlp-core ack build --highest-applied-update-id 10 --ack-bitmap 15 \
                  --applied-cell-count 510 --stale-cells 0 --converged true",
    },
    CommandSpec {
        path: &["ack", "inspect"],
        summary: "Read an acknowledgement's bytes and print its fields and its window.",
        flags: &[
            FlagSpec {
                name: "hex",
                takes_value: true,
                help: "the acknowledgement's bytes, as hex",
            },
            FlagSpec {
                name: "file",
                takes_value: true,
                help: "a file holding the acknowledgement's bytes, as hex",
            },
            FlagSpec {
                name: "stdin",
                takes_value: false,
                help: "read the acknowledgement's bytes from standard input, as hex",
            },
            JSON_FLAG,
        ],
        example: "mlp-core ack inspect --hex 080208102a1000",
    },
    CommandSpec {
        path: &["limits"],
        summary: "Print the protocol's hard limits: the caps, the timeout table and the invariants.",
        flags: &[JSON_FLAG],
        example: "mlp-core limits --json",
    },
];

/// The name the tool reports in `--version`.
pub const TOOL_NAME: &str = env!("CARGO_PKG_NAME");
/// The version the tool reports in `--version`.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the tool with `args` — the whole argument vector, program name first.
pub fn run(args: &[String]) -> Outcome {
    match dispatch(args) {
        Ok(outcome) => outcome,
        Err(error) => Outcome::failed(&error),
    }
}

fn dispatch(args: &[String]) -> Result<Outcome, CliError> {
    let rest = args.get(1..).unwrap_or(&[]);
    if rest.is_empty() {
        return Ok(Outcome {
            code: EXIT_USAGE,
            stdout: USAGE.to_string(),
            stderr: String::new(),
        });
    }
    match rest[0].as_str() {
        "--help" | "-h" | "help" => return Ok(Outcome::ok(USAGE.to_string())),
        "--version" | "-V" => {
            return Ok(Outcome::ok(format!("{TOOL_NAME} {TOOL_VERSION}\n")));
        }
        _ => {}
    }

    // The verb path is the leading arguments that are not flags.
    let verb_len = rest
        .iter()
        .take_while(|argument| !argument.starts_with('-'))
        .count();
    if verb_len == 0 {
        return Err(CliError::Usage(format!(
            "expected a command; run `{TOOL_NAME} --help` for the list"
        )));
    }
    let verb: Vec<&str> = rest[..verb_len].iter().map(String::as_str).collect();
    let spec = COMMANDS
        .iter()
        .find(|command| command.path == verb.as_slice())
        .ok_or_else(|| {
            CliError::Usage(format!(
                "unknown command `{}`; run `{TOOL_NAME} --help` for the list",
                verb.join(" ")
            ))
        })?;

    let mut options = Options::new(spec.flags);
    options.parse(&rest[verb_len..])?;

    if options.flag("help").is_some() {
        return Ok(Outcome::ok(command_help(spec)));
    }

    let json = options.flag("json").is_some();
    let produced = match spec.path {
        ["fragment", "build"] => fragment::build(&options, json)?,
        ["fragment", "inspect"] => fragment::inspect(&options, json)?,
        ["object", "assemble"] => object::assemble(&options, json)?,
        ["ack", "build"] => ack::build(&options, json)?,
        ["ack", "inspect"] => ack::inspect(&options, json)?,
        ["limits"] => limits::limits(&options, json)?,
        _ => {
            return Err(CliError::Usage(format!(
                "unknown command `{}`",
                verb.join(" ")
            )));
        }
    };
    Ok(Outcome {
        code: produced.code,
        stdout: produced.text,
        stderr: String::new(),
    })
}

/// The top-level help text.
pub const USAGE: &str = "\
mlp-core — the Media Lane Protocol's command-line tool.

Usage:
  mlp-core <command> [flags]

Commands:
  fragment build     Build one fragment from its fields and print its bytes.
  fragment inspect   Read a fragment's bytes and print its header, its TLVs and its body.
  object assemble    Assemble fragments into one object and report the assembler\'s decision.
  ack build          Build the receiver\'s acknowledgement and print its bytes.
  ack inspect        Read an acknowledgement\'s bytes and print its fields and its window.
  limits             Print the protocol\'s hard limits: the caps, the timeouts, the invariants.

Global flags:
  --json             Print the machine-readable JSON form instead of the table.
  --help             Print this help, or a command\'s own help.
  --version          Print the tool\'s name and version.

Exit status:
  0  the command succeeded
  1  the check failed (a measurement past its budget)
  2  the usage was wrong
  3  the input could not be read

Run `mlp-core <command> --help` for a command\'s flags and a worked example.
";

/// The help text of one command.
pub fn command_help(spec: &CommandSpec) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "{}", spec.summary);
    let _ = writeln!(text);
    let _ = writeln!(text, "Usage:\n  {} {}", TOOL_NAME, spec.path.join(" "));
    let _ = writeln!(text);
    let _ = writeln!(text, "Flags:");
    for flag in spec.flags {
        let name = if flag.takes_value {
            format!("--{} <value>", flag.name)
        } else {
            format!("--{}", flag.name)
        };
        let _ = writeln!(text, "  {name:<26} {}", flag.help);
    }
    let _ = writeln!(text, "  {:<26} print this help", "--help");
    let _ = writeln!(text);
    let _ = writeln!(text, "Example:");
    let _ = writeln!(text, "  {}", spec.example);
    text
}

/// A command's parsed flags.
#[derive(Debug)]
pub struct Options {
    flags: Vec<FlagSpec>,
    values: Vec<(String, Option<String>)>,
    positionals: Vec<String>,
}

impl Options {
    /// An empty option set for a command.
    pub fn new(flags: &[FlagSpec]) -> Self {
        Self {
            flags: flags.to_vec(),
            values: Vec::new(),
            positionals: Vec::new(),
        }
    }

    fn parse(&mut self, arguments: &[String]) -> Result<(), CliError> {
        let mut index = 0usize;
        while index < arguments.len() {
            let argument = &arguments[index];
            if let Some(body) = argument.strip_prefix("--") {
                let (name, inline) = match body.split_once('=') {
                    Some((name, value)) => (name.to_string(), Some(value.to_string())),
                    None => (body.to_string(), None),
                };
                let spec = self.spec_flag(&name);
                if spec.is_none() && name != "help" && name != "version" {
                    return Err(CliError::Usage(format!("unknown flag `--{name}`")));
                }
                let takes_value = spec.map(|flag| flag.takes_value).unwrap_or(false);
                let value = match inline {
                    Some(value) => Some(value),
                    None if takes_value => {
                        index += 1;
                        match arguments.get(index) {
                            Some(value) => Some(value.clone()),
                            None => {
                                return Err(CliError::Usage(format!(
                                    "flag `--{name}` needs a value"
                                )));
                            }
                        }
                    }
                    None => None,
                };
                self.values.push((name, value));
            } else {
                self.positionals.push(argument.clone());
            }
            index += 1;
        }
        Ok(())
    }

    fn spec_flag(&self, name: &str) -> Option<&FlagSpec> {
        if name == "help" || name == "version" {
            return None;
        }
        self.flags.iter().find(|flag| flag.name == name)
    }

    /// The value of a flag, when it was given.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.as_deref())
    }

    /// Whether a flag was given.
    pub fn flag(&self, name: &str) -> Option<&Option<String>> {
        self.values
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// Every value of a repeatable flag, in the order they were given.
    pub fn values(&self, name: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(key, _)| key == name)
            .filter_map(|(_, value)| value.as_deref())
            .collect()
    }

    /// The positional arguments.
    pub fn positionals(&self) -> &[String] {
        &self.positionals
    }

    /// Record a flag's value, as the parser would.
    pub fn insert(&mut self, name: &str, value: Option<&str>) {
        self.values
            .push((name.to_string(), value.map(str::to_string)));
    }

    /// A required flag's value, or a usage error naming it.
    pub fn required(&self, name: &str) -> Result<&str, CliError> {
        self.value(name)
            .ok_or_else(|| CliError::Usage(format!("flag `--{name}` is required")))
    }

    /// A flag's value parsed as an unsigned integer.
    pub fn unsigned<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, CliError>
    where
        T::Err: std::fmt::Display,
    {
        match self.value(name) {
            None => Ok(None),
            Some(text) => text.parse::<T>().map(Some).map_err(|error| {
                CliError::Usage(format!(
                    "flag `--{name}`: {text:?} is not a number ({error})"
                ))
            }),
        }
    }

    /// A flag's value parsed as a boolean: `true` or `false`.
    pub fn boolean(&self, name: &str) -> Result<Option<bool>, CliError> {
        match self.value(name) {
            None => Ok(None),
            Some(text) => match text {
                "true" => Ok(Some(true)),
                "false" => Ok(Some(false)),
                other => Err(CliError::Usage(format!(
                    "flag `--{name}`: {other:?} is not `true` or `false`"
                ))),
            },
        }
    }
}

/// Build an option set directly, for a caller that drives a verb without
/// going through the command line.
///
/// Every flag of every command is accepted, so a caller may hand a verb any
/// flag it understands.
pub fn test_options(values: &[(&str, Option<&str>)]) -> Options {
    let mut flags = Vec::new();
    for command in COMMANDS {
        for flag in command.flags {
            if !flags.iter().any(|known: &FlagSpec| known.name == flag.name) {
                flags.push(*flag);
            }
        }
    }
    let mut options = Options::new(&flags);
    for (name, value) in values {
        options.insert(name, *value);
    }
    options
}

/// Render a table: a header row, then the rows, aligned in columns.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let columns = headers.len();
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(columns) {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let mut text = String::new();
    let render = |text: &mut String, cells: &[String]| {
        for (index, cell) in cells.iter().enumerate() {
            if index + 1 == columns {
                let _ = writeln!(text, "{cell}");
            } else {
                let _ = write!(text, "{cell:<width$}  ", width = widths[index]);
            }
        }
    };
    let header_cells: Vec<String> = headers.iter().map(|header| (*header).to_string()).collect();
    render(&mut text, &header_cells);
    for row in rows {
        render(&mut text, row);
    }
    text
}

/// Print the JSON form of a value, with a trailing newline.
pub fn json_line(value: &serde_json::Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|error| {
        format!("{{\"error\":\"the JSON form could not be produced: {error}\"}}")
    });
    text.push('\n');
    text
}

/// Read standard input as text.
pub fn read_stdin() -> Result<String, CliError> {
    use std::io::Read as _;
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| CliError::Input(format!("standard input could not be read: {error}")))?;
    Ok(text)
}

/// A short hexadecimal form of a byte slice, in lower case, without separators.
pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn an_unknown_command_is_a_usage_error() {
        let outcome = run(&args(&["mlp-core", "nonsense"]));
        assert_eq!(outcome.code, EXIT_USAGE);
        assert!(outcome.stderr.contains("unknown command"));
    }

    #[test]
    fn an_unknown_flag_is_a_usage_error() {
        let outcome = run(&args(&["mlp-core", "limits", "--nope"]));
        assert_eq!(outcome.code, EXIT_USAGE);
    }

    #[test]
    fn no_arguments_prints_the_usage_and_fails_with_the_usage_status() {
        let outcome = run(&args(&["mlp-core"]));
        assert_eq!(outcome.code, EXIT_USAGE);
        assert!(outcome.stdout.contains("fragment build"));
    }

    #[test]
    fn the_version_names_the_tool() {
        let outcome = run(&args(&["mlp-core", "--version"]));
        assert_eq!(outcome.code, EXIT_OK);
        assert!(outcome.stdout.starts_with("mlp-core "));
    }

    #[test]
    fn the_usage_documents_the_exit_status_contract() {
        assert!(USAGE.contains("0  the command succeeded"));
        assert!(USAGE.contains("1  the check failed"));
        assert!(USAGE.contains("2  the usage was wrong"));
        assert!(USAGE.contains("3  the input could not be read"));
    }

    #[test]
    fn every_command_documents_its_flags_and_an_example() {
        for command in COMMANDS {
            let help = command_help(command);
            assert!(
                help.contains("Flags:"),
                "{} has no flags",
                command.path.join(" ")
            );
            assert!(
                help.contains("Example:"),
                "{} has no example",
                command.path.join(" ")
            );
            for flag in command.flags {
                assert!(
                    help.contains(flag.name),
                    "{} is missing {}",
                    command.path.join(" "),
                    flag.name,
                );
            }
        }
    }

    #[test]
    fn a_flag_may_take_its_value_inline_or_apart() {
        let inline = run(&args(&["mlp-core", "limits", "--json"]));
        assert_eq!(inline.code, EXIT_OK);
        assert!(inline.stdout.starts_with('{'));
    }

    #[test]
    fn a_flag_without_its_value_is_a_usage_error() {
        let outcome = run(&args(&["mlp-core", "fragment", "build", "--class"]));
        assert_eq!(outcome.code, EXIT_USAGE);
        assert!(outcome.stderr.contains("needs a value"));
    }

    #[test]
    fn a_required_flag_that_is_missing_is_a_usage_error() {
        let outcome = run(&args(&["mlp-core", "ack", "inspect"]));
        assert_eq!(outcome.code, EXIT_USAGE);
        assert!(outcome.stderr.contains("required"));
    }

    #[test]
    fn the_table_aligns_its_columns() {
        let text = table(&["a", "bb"], &[vec!["1".into(), "2".into()]]);
        assert_eq!(text, "a  bb\n1  2\n");
    }

    #[test]
    fn hex_is_lower_case_and_unseparated() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
