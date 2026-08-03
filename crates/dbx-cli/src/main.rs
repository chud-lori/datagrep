//! `dbx` — the CLI face of the engine (design §4 killer feature #4: "Same
//! core, three faces: GUI, TUI, CLI \[...\] `dbx query -f q.sql --format
//! json` pipes into jq"). Parses args, wires `tracing-subscriber` to stderr
//! only under `--verbose`, dispatches to `cmd::*`, and maps [`CliError`] to
//! the ticket's exit codes (0 ok, 1 query error, 2 usage error, 130
//! cancelled).
//!
//! Nothing here connects to a database, opens the profile store, or inits
//! TLS before a subcommand actually needs it — [`Context::new`] is
//! documented cheap, so `dbx --help` and `dbx profiles list` stay near the
//! design's ≤250ms cold-start target (P1).

mod cli;
mod cmd;
mod context;
mod drivers;
mod duration;
mod exit;
mod format;
mod paths;
mod value_text;

use clap::Parser;

use cli::{Cli, Command, HistoryCommand, ProfilesCommand};
use context::Context;
use exit::{CliError, ExitKind};

fn main() {
    let cli = Cli::parse();
    if cli.verbose {
        let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "dbx=debug,warn".to_string());
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .init();
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(1)
        .clamp(2, 4);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: could not start the async runtime: {err}");
            std::process::exit(ExitKind::QueryError.code() as i32);
        }
    };

    let result = runtime.block_on(async {
        let ctx = Context::new();
        dispatch(&ctx, cli.command).await
    });

    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(err.kind.code() as i32);
        }
    }
}

/// Runs the command, racing it against Ctrl-C. Design §3.3's "the stop
/// button always returns control instantly" is about `CoreApi::cancel`
/// inside a running query; the process-level analogue here is: cancel
/// whatever query is in flight (if any) — reporting the real
/// `CancelReport::message`, not a canned one — and unwind through the normal
/// `Result` → exit-code path (never a raw `std::process::exit` bypassing it)
/// so a Ctrl-C during `query`/`export`/`catalog`/`doctor` all get the same
/// honest 130.
async fn dispatch(ctx: &Context, command: Command) -> Result<(), CliError> {
    tokio::select! {
        result = run_command(ctx, command) => result,
        message = wait_for_ctrl_c(ctx) => Err(CliError::cancelled(message)),
    }
}

async fn wait_for_ctrl_c(ctx: &Context) -> String {
    if tokio::signal::ctrl_c().await.is_err() {
        // The signal handler itself failed to install; nothing more to wait
        // on, but this branch must still resolve for `select!` to make
        // progress if the command hangs forever.
        std::future::pending::<()>().await;
    }
    match ctx.current_query() {
        Some(qid) => match ctx.core.cancel(qid).await {
            Ok(report) => report.message.to_string(),
            Err(_) => "interrupted".to_string(),
        },
        None => "interrupted".to_string(),
    }
}

async fn run_command(ctx: &Context, command: Command) -> Result<(), CliError> {
    match command {
        Command::Query(args) => cmd::query::run(ctx, &args).await,
        Command::Export(args) => cmd::export::run(ctx, &args).await,
        Command::Profiles { command } => match command {
            ProfilesCommand::List => cmd::profiles::list(ctx).await,
            ProfilesCommand::Add { name, url } => cmd::profiles::add(ctx, &name, &url).await,
            ProfilesCommand::Remove { name } => cmd::profiles::remove(ctx, &name).await,
            ProfilesCommand::Show { name } => cmd::profiles::show(ctx, &name).await,
            ProfilesCommand::Export { out } => cmd::profiles::export(ctx, out.as_deref()).await,
            ProfilesCommand::Import {
                file,
                merge,
                replace,
            } => cmd::profiles::import(ctx, &file, merge, replace).await,
        },
        Command::Catalog(args) => cmd::catalog::run(ctx, &args).await,
        Command::History { command } => match command {
            HistoryCommand::List { limit, profile } => {
                cmd::history::list(ctx, limit, profile.as_deref()).await
            }
            HistoryCommand::Search {
                text,
                limit,
                profile,
            } => cmd::history::search(ctx, &text, limit, profile.as_deref()).await,
        },
        Command::Doctor(args) => cmd::doctor::run(ctx, &args).await,
    }
}
