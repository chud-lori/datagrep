//! `datagrep catalog` (ticket item 4): list ONE level lazily — one
//! `CoreApi::list_catalog` call, which on connect issues exactly one cheap
//! query and expands on demand per node. This command never recurses; each
//! invocation is one `children()` call for one path.
//!
//! `--describe` needs [`datagrep_api::Catalog::describe`], which `CoreApi` does
//! not wrap the way it wraps `children` (as `list_catalog`) — no
//! `describe_catalog`/`infer_shape`/`complete` façade exists. This reaches
//! one level further into the public seam than `list_catalog`, via
//! `CoreApi::session(...).acquire().await?.catalog()` — still entirely
//! `CoreApi`/`datagrep-core` public API (`Session`, `ConnLease`, `Catalog` are all
//! `datagrep-core`/`datagrep-api` types), never a driver crate directly, but it skips
//! the panic-isolation wrapper `list_catalog` gets from `datagrep_core::api`'s
//! internal `guarded(...)` helper. Noted as a gap CoreApi should close.

use std::sync::Arc;

use datagrep_api::catalog::ListOpts;
use datagrep_api::shape::ObjectPath;

use crate::cli::CatalogArgs;
use crate::context::Context;
use crate::exit::CliError;

pub async fn run(ctx: &Context, args: &CatalogArgs) -> Result<(), CliError> {
    let (profile_id, _profile) = ctx.open_profile(&args.profile).await?;

    if let Some(describe) = &args.describe {
        let path = parse_path(describe);
        let session = ctx.core.session(profile_id)?;
        let lease = session.acquire().await?;
        let detail = lease.catalog().describe(&path).await?;
        print_detail(&detail);
        return Ok(());
    }

    let parent = ObjectPath::new(args.path.iter().map(|s| Arc::from(s.as_str())).collect());
    let page = ctx
        .core
        .list_catalog(profile_id, &parent, ListOpts::default())
        .await?;

    if page.items.is_empty() {
        println!("(empty)");
        return Ok(());
    }
    for item in &page.items {
        let marker = if item.has_children { "/" } else { "" };
        match &item.comment {
            Some(comment) => println!("{:?}\t{}{marker}\t{comment}", item.kind, item.path),
            None => println!("{:?}\t{}{marker}", item.kind, item.path),
        }
    }
    if page.next.is_some() {
        println!("… more (raise --limit is not wired up in this build; see README)");
    }
    Ok(())
}

/// `a.b.c` (dotted, matching how paths print) or space-separated — accept
/// either since `--describe` is a single string argument, unlike the
/// positional `path` segments `datagrep catalog` itself takes.
fn parse_path(s: &str) -> ObjectPath {
    let parts: Vec<Arc<str>> = s
        .split(['.', ' '])
        .filter(|p| !p.is_empty())
        .map(Arc::from)
        .collect();
    ObjectPath::new(parts)
}

fn print_detail(detail: &datagrep_api::catalog::ObjectDetail) {
    println!("path: {}", detail.node.path);
    println!("kind: {:?}", detail.node.kind);
    if let Some(comment) = &detail.node.comment {
        println!("comment: {comment}");
    }
    match &detail.schema {
        Some(schema) => {
            println!("columns:");
            for field in &schema.fields {
                let nullable = if field
                    .flags
                    .contains(datagrep_api::shape::FieldFlags::NULLABLE)
                {
                    "NULL"
                } else {
                    "NOT NULL"
                };
                let pk = if field
                    .flags
                    .contains(datagrep_api::shape::FieldFlags::PRIMARY_KEY)
                {
                    " PK"
                } else {
                    ""
                };
                let native = field
                    .native_type
                    .as_deref()
                    .map(|t| format!(" ({t})"))
                    .unwrap_or_default();
                println!(
                    "  {} {:?}{native} {nullable}{pk}",
                    field.name, field.logical
                );
            }
        }
        None => println!("columns: (no declared schema)"),
    }
    for (k, v) in &detail.extra {
        println!("{k}: {v}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_path_splits_on_dot_or_space() {
        let p = parse_path("main.users");
        assert_eq!(p.parts().len(), 2);
        let p = parse_path("main users");
        assert_eq!(p.parts().len(), 2);
        let p = parse_path("");
        assert_eq!(p.parts().len(), 0);
    }
}
