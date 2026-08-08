//! `clap` command surface (ticket "Commands"). Parsing this must stay cheap
//! and side-effect-free — `datagrep --help` / `datagrep profiles list` being
//! near-instant (design P1, ticket "Cold start matters") depends on nothing
//! here touching a socket, a profile database, or TLS roots. `clap` itself
//! exits the process with code 2 on a usage error and 0 on `--help`, which is
//! exactly the ticket's exit-code contract for "the command itself was
//! malformed" — no extra wiring needed.

use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "datagrep",
    version,
    about = "Same core, three faces: GUI, TUI, CLI. datagrep query -f q.sql --format json | jq"
)]
pub struct Cli {
    /// Wire `tracing-subscriber` to stderr. Default is quiet.
    #[arg(long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run SQL (or another connection's native language) and print results.
    Query(QueryArgs),
    /// Stream a query straight to a file — never through the result store.
    Export(ExportArgs),
    /// Manage connection profiles (plain-text, git-committable TOML).
    Profiles {
        #[command(subcommand)]
        command: ProfilesCommand,
    },
    /// Lazily browse one level of a connection's catalog.
    Catalog(CatalogArgs),
    /// Query history (FTS5-backed, local, per-connection).
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Print resolved config, registered drivers, and a connection round trip.
    Doctor(DoctorArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Aligned ASCII table for humans.
    Table,
    /// One JSON array of row objects.
    Json,
    /// One JSON object per line.
    Ndjson,
    Csv,
    Tsv,
}

#[derive(Debug, Parser)]
#[command(group(
    ArgGroup::new("input")
        .args(["file", "command", "stdin"])
        .multiple(false)
))]
pub struct QueryArgs {
    /// Profile to run against (see `datagrep profiles list`).
    #[arg(long)]
    pub profile: String,

    /// Read the statement(s) from a file.
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub file: Option<PathBuf>,

    /// The statement(s), given directly on the command line.
    #[arg(short = 'c', long = "command", value_name = "SQL")]
    pub command: Option<String>,

    /// Read the statement(s) from stdin. Also the default when neither
    /// `-f`/`-c` is given, so `datagrep query --profile p -` and piping with no
    /// flag at all behave the same.
    #[arg(value_name = "-")]
    pub stdin: Option<StdinMarker>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,

    /// Cap the number of rows fetched, same as a trailing `-- @limit N`.
    #[arg(long)]
    pub limit: Option<u64>,

    /// Statement timeout, e.g. `30s`, `500ms`, `5m`.
    #[arg(long)]
    pub timeout: Option<String>,

    /// Write output here instead of stdout.
    #[arg(short = 'o', long = "out", value_name = "FILE")]
    pub out: Option<PathBuf>,
}

/// A positional argument that only ever means "read from stdin" — accepted so
/// `datagrep query --profile p -` matches the ticket's literal `-f file.sql | -c
/// "SQL" | -` grammar. Any value other than the literal `-` is a usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdinMarker;

impl std::str::FromStr for StdinMarker {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "-" {
            Ok(StdinMarker)
        } else {
            Err(format!(
                "unexpected positional argument `{s}` (did you mean -f {s} or -c {s}?)"
            ))
        }
    }
}

#[derive(Debug, Parser)]
pub struct ExportArgs {
    #[arg(long)]
    pub profile: String,

    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub file: Option<PathBuf>,

    #[arg(short = 'c', long = "command", value_name = "SQL")]
    pub command: Option<String>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Csv)]
    pub format: OutputFormat,

    /// Destination file. Streamed to, with a fixed buffer — never through the
    /// result store: "export all" is not "load all".
    #[arg(short = 'o', long = "out", value_name = "FILE")]
    pub out: PathBuf,

    /// Export exactly N rows per statement, then stop. Without this, export
    /// is uncapped and always delivers the complete result.
    #[arg(long)]
    pub limit: Option<u64>,

    #[arg(long)]
    pub timeout: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ProfilesCommand {
    List,
    /// Add a profile from a connection URL. An inline password is split into
    /// a keychain `SecretRef` and never written to the profile.
    Add {
        name: String,
        url: String,
    },
    Remove {
        name: String,
    },
    /// Print one profile. Any secret shows as `••••`, never resolved.
    Show {
        name: String,
    },
    /// Git-committable TOML of every profile (secrets excluded structurally).
    Export {
        #[arg(short = 'o', long = "out", value_name = "FILE")]
        out: Option<PathBuf>,
    },
    Import {
        file: PathBuf,
        #[arg(long, group = "strategy")]
        merge: bool,
        #[arg(long, group = "strategy")]
        replace: bool,
    },
}

#[derive(Debug, Parser)]
pub struct CatalogArgs {
    #[arg(long)]
    pub profile: String,

    /// Path segments identifying the parent to list (e.g. `mydb public`).
    /// Empty = root.
    pub path: Vec<String>,

    /// Full detail (columns, indexes) for one object instead of listing
    /// its children.
    #[arg(long)]
    pub describe: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    List {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Restrict to one profile's history.
        #[arg(long)]
        profile: Option<String>,
    },
    Search {
        text: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Debug, Parser)]
pub struct DoctorArgs {
    /// Also try to connect to this profile and time the round trip.
    #[arg(long)]
    pub profile: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_internally_consistent() {
        // `clap`'s own invariants (no duplicate ids, valid groups, …) —
        // catches a typo'd `ArgGroup` before it ever reaches a user.
        Cli::command().debug_assert();
    }

    #[test]
    fn query_defaults_to_table_format() {
        let cli = Cli::parse_from(["datagrep", "query", "--profile", "p", "-c", "select 1"]);
        let Command::Query(args) = cli.command else {
            panic!("expected Query");
        };
        assert_eq!(args.format, OutputFormat::Table);
        assert_eq!(args.command.as_deref(), Some("select 1"));
    }

    #[test]
    fn query_rejects_both_file_and_command() {
        let err = Cli::try_parse_from([
            "datagrep",
            "query",
            "--profile",
            "p",
            "-f",
            "q.sql",
            "-c",
            "select 1",
        ])
        .unwrap_err();
        assert_eq!(err.exit_code(), 2, "usage errors are exit code 2");
    }

    #[test]
    fn profiles_add_parses_name_and_url() {
        let cli = Cli::parse_from([
            "datagrep",
            "profiles",
            "add",
            "staging",
            "postgres://user@host/db",
        ]);
        let Command::Profiles {
            command: ProfilesCommand::Add { name, url },
        } = cli.command
        else {
            panic!("expected Profiles Add");
        };
        assert_eq!(name, "staging");
        assert_eq!(url, "postgres://user@host/db");
    }
}
