use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "datagrep",
    version,
    about = "Same core, three faces: GUI, TUI, CLI. datagrep query -f q.sql --format json | jq"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Query(QueryArgs),
    Export(ExportArgs),
    Profiles {
        #[command(subcommand)]
        command: ProfilesCommand,
    },
    Catalog(CatalogArgs),
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    Doctor(DoctorArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
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
    #[arg(long)]
    pub profile: String,

    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub file: Option<PathBuf>,

    #[arg(short = 'c', long = "command", value_name = "SQL")]
    pub command: Option<String>,

    #[arg(value_name = "-")]
    pub stdin: Option<StdinMarker>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,

    #[arg(long)]
    pub limit: Option<u64>,

    #[arg(long)]
    pub timeout: Option<String>,

    #[arg(short = 'o', long = "out", value_name = "FILE")]
    pub out: Option<PathBuf>,
}

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

    #[arg(short = 'o', long = "out", value_name = "FILE")]
    pub out: PathBuf,

    #[arg(long)]
    pub limit: Option<u64>,

    #[arg(long)]
    pub timeout: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ProfilesCommand {
    List,
    Add {
        name: String,
        url: String,
    },
    Remove {
        name: String,
    },
    Show {
        name: String,
    },
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

    pub path: Vec<String>,

    #[arg(long)]
    pub describe: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    List {
        #[arg(long, default_value_t = 50)]
        limit: u32,
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
    #[arg(long)]
    pub profile: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_internally_consistent() {
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
