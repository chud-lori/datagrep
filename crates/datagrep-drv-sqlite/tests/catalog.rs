mod common;

use std::sync::Arc;

use datagrep_api::{Completion, CompletionCtx, ListOpts, ObjectKind, ObjectPath, Request};

fn name_of(node: &datagrep_api::ObjectNode) -> String {
    node.path
        .parts()
        .last()
        .map(|p| p.to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn catalog_lists_tables_columns_and_completes() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL, age INTEGER)",
    ))
    .await
    .expect("create table failed");
    conn.execute(Request::native(
        "CREATE VIEW active_users AS SELECT * FROM users",
    ))
    .await
    .expect("create view failed");

    let catalog = conn.catalog();
    assert_eq!(
        catalog.levels().len(),
        3,
        "database -> table/view -> column"
    );

    // --- database level ---
    let dbs = catalog
        .children(&ObjectPath::root(), ListOpts::default())
        .await
        .expect("list databases failed");
    assert!(
        dbs.items
            .iter()
            .any(|n| name_of(n) == "main" && n.kind == ObjectKind::Database),
        "expected a `main` database node, got {dbs:?}"
    );

    // --- table/view level ---
    let main = ObjectPath::new(vec![Arc::from("main")]);
    let objects = catalog
        .children(&main, ListOpts::default())
        .await
        .expect("list tables failed");
    let names: Vec<String> = objects.items.iter().map(name_of).collect();
    assert!(names.contains(&"users".to_string()), "{names:?}");
    assert!(names.contains(&"active_users".to_string()), "{names:?}");
    let view_node = objects
        .items
        .iter()
        .find(|n| name_of(n) == "active_users")
        .expect("active_users node");
    assert_eq!(view_node.kind, ObjectKind::View);
    let table_node = objects
        .items
        .iter()
        .find(|n| name_of(n) == "users")
        .expect("users node");
    assert_eq!(table_node.kind, ObjectKind::Table);

    // Prefix filtering.
    let filtered = catalog
        .children(
            &main,
            ListOpts {
                prefix: Some(Arc::from("active")),
                ..ListOpts::default()
            },
        )
        .await
        .expect("prefix-filtered list failed");
    assert_eq!(filtered.items.len(), 1);
    assert_eq!(name_of(&filtered.items[0]), "active_users");

    // --- column level ---
    let users_path = ObjectPath::new(vec![Arc::from("main"), Arc::from("users")]);
    let cols = catalog
        .children(&users_path, ListOpts::default())
        .await
        .expect("list columns failed");
    let col_names: Vec<String> = cols.items.iter().map(name_of).collect();
    assert_eq!(col_names, vec!["id", "email", "age"]);
    assert!(cols.items.iter().all(|n| n.kind == ObjectKind::Column));

    // --- describe ---
    let detail = catalog
        .describe(&users_path)
        .await
        .expect("describe failed");
    assert_eq!(detail.node.kind, ObjectKind::Table);
    let schema = detail.schema.expect("table describe carries a schema");
    assert_eq!(schema.fields.len(), 3);
    let identity = schema.identity.expect("users has a declared PRIMARY KEY");
    assert_eq!(identity.field_indices, vec![0], "`id` is field 0");

    // --- complete ---
    let ctx = CompletionCtx {
        text: Arc::from("SELECT * FROM us"),
        offset: 16,
        scope: None,
    };
    let completions: Vec<Completion> = catalog.complete(ctx).await.expect("complete failed");
    assert!(
        completions.iter().any(|c| &*c.label == "users"),
        "{completions:?}"
    );
    assert!(completions.len() <= 50, "complete() must stay bounded");
}

#[tokio::test]
async fn infer_shape_samples_real_storage_classes() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("CREATE TABLE t(v)"))
        .await
        .expect("create table failed");
    conn.execute(Request::native("INSERT INTO t VALUES (1), ('two'), (3.0)"))
        .await
        .expect("seed rows failed");

    let inferred = conn
        .catalog()
        .infer_shape(&ObjectPath::new(vec![Arc::from("t")]), 10)
        .await
        .expect("infer_shape failed");
    assert_eq!(inferred.sampled, 3);
    let (_, trie) = inferred
        .root
        .iter()
        .find(|(name, _)| &**name == "v")
        .expect("column v");
    assert_eq!(trie.present, 3);
    assert!(
        trie.types.len() >= 2,
        "heterogeneous column should show more than one observed type, got {:?}",
        trie.types
    );
}
