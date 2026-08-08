//! `datagrep history` (ticket item 5): the FTS5-backed history already living in
//! `datagrep-profiles`. `datagrep query` records one entry per statement it runs (see
//! `query.rs`) so this command has something real to show end to end.

use crate::context::Context;
use crate::exit::CliError;

pub async fn list(ctx: &Context, limit: u32, profile: Option<&str>) -> Result<(), CliError> {
    let profile_id = resolve_profile_id(ctx, profile).await?;
    let entries = ctx.store.recent_history(profile_id, limit).await?;
    print_entries(&entries);
    Ok(())
}

pub async fn search(
    ctx: &Context,
    text: &str,
    limit: u32,
    profile: Option<&str>,
) -> Result<(), CliError> {
    let profile_id = resolve_profile_id(ctx, profile).await?;
    let entries = ctx
        .store
        .search_history(profile_id, text.to_string(), limit)
        .await?;
    print_entries(&entries);
    Ok(())
}

async fn resolve_profile_id(
    ctx: &Context,
    profile: Option<&str>,
) -> Result<Option<String>, CliError> {
    match profile {
        Some(name) => Ok(Some(ctx.find_profile(name).await?.id)),
        None => Ok(None),
    }
}

fn print_entries(entries: &[datagrep_profiles::HistoryEntry]) {
    if entries.is_empty() {
        println!("(no history)");
        return;
    }
    for e in entries {
        let duration = e
            .duration_ms
            .map(|d| format!("{d}ms"))
            .unwrap_or_else(|| "-".to_string());
        let rows = e
            .row_count
            .map(|r| r.to_string())
            .unwrap_or_else(|| "-".to_string());
        let one_line = e.text.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "{}\t{}\t{duration}\t{rows} rows\t{one_line}",
            e.started_at,
            e.status.as_str()
        );
    }
}
