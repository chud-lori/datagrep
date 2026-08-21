//! Structured DDL (`Op::Ddl`) against a real SQLite, addressed by the path
//! and kind the catalog reports.

mod common;

use std::sync::Arc;

use datagrep_api::{
    Connection, DbError, DdlOp, FetchHint, FieldPath, ListOpts, ObjectKind, ObjectPath, Op,
    Request, Value,
};

async fn ddl(conn: &dyn Connection, op: DdlOp) -> Result<(), DbError> {
    let mut cur = conn.execute(Request::Op(Op::Ddl(op))).await?;
    while cur.next_batch(FetchHint::default()).await?.is_some() {}
    Ok(())
}

fn p(parts: &[&str]) -> ObjectPath {
    ObjectPath::new(parts.iter().map(|s| Arc::from(*s)).collect())
}

/// `sqlite_master` count for a name, through the same seam a user would use.
async fn count_named(conn: &dyn Connection, name: &str) -> i64 {
    let mut cur = conn
        .execute(Request::Native {
            text: Arc::from("SELECT COUNT(*) FROM sqlite_master WHERE name = ?"),
            params: vec![Value::Str(Arc::from(name))],
            opts: Default::default(),
        })
        .await
        .expect("count");
    let batch = cur
        .next_batch(FetchHint::default())
        .await
        .expect("batch")
        .expect("a row");
    let datagrep_api::Payload::Rows(rows) = batch.payload else {
        panic!("expected rows")
    };
    match &rows[0][0] {
        Value::I64(n) => *n,
        other => panic!("expected a count, got {other:?}"),
    }
}

#[tokio::test]
async fn structured_ddl_round_trips_through_the_catalog() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native(
        "CREATE TABLE widgets(id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
    ))
    .await
    .expect("create table");
    conn.execute(Request::native(
        "CREATE VIEW widgets_v AS SELECT * FROM widgets",
    ))
    .await
    .expect("create view");

    let listed = conn
        .catalog()
        .children(
            &p(&["main"]),
            ListOpts {
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .expect("list");
    let widgets = listed
        .items
        .iter()
        .find(|n| &*n.path.parts()[1] == "widgets")
        .expect("widgets should be listed")
        .clone();
    assert_eq!(widgets.kind, ObjectKind::Table);

    // `CREATE INDEX main.i ON t(...)` — the index name is schema-qualified
    // and the table is not, which is the reverse of every other statement.
    ddl(
        conn.as_ref(),
        DdlOp::CreateIndex {
            path: widgets.path.clone(),
            name: Arc::from("widgets_name"),
            fields: vec![FieldPath::field("name")],
            unique: true,
            if_not_exists: true,
        },
    )
    .await
    .expect("create index");
    assert_eq!(count_named(conn.as_ref(), "widgets_name").await, 1);
    ddl(
        conn.as_ref(),
        DdlOp::CreateIndex {
            path: widgets.path.clone(),
            name: Arc::from("widgets_name"),
            fields: vec![FieldPath::field("name")],
            unique: true,
            if_not_exists: true,
        },
    )
    .await
    .expect("create index is idempotent with the guard");

    // An index is addressed as its table's path plus the index name, even
    // though SQLite stores it beside the table rather than under it.
    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: widgets.path.child("widgets_name"),
            kind: ObjectKind::Index,
            if_exists: false,
        },
    )
    .await
    .expect("drop index");
    assert_eq!(count_named(conn.as_ref(), "widgets_name").await, 0);

    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: p(&["main", "widgets_v"]),
            kind: ObjectKind::View,
            if_exists: false,
        },
    )
    .await
    .expect("drop view");
    assert_eq!(count_named(conn.as_ref(), "widgets_v").await, 0);

    ddl(
        conn.as_ref(),
        DdlOp::Rename {
            from: widgets.path.clone(),
            to: p(&["main", "gadgets"]),
            kind: ObjectKind::Table,
        },
    )
    .await
    .expect("rename table");
    assert_eq!(count_named(conn.as_ref(), "gadgets").await, 1);

    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: p(&["main", "gadgets"]),
            kind: ObjectKind::Table,
            if_exists: true,
        },
    )
    .await
    .expect("drop table");
    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: p(&["main", "gadgets"]),
            kind: ObjectKind::Table,
            if_exists: true,
        },
    )
    .await
    .expect("dropping it again is a no-op");
    assert!(
        ddl(
            conn.as_ref(),
            DdlOp::Drop {
                path: p(&["main", "gadgets"]),
                kind: ObjectKind::Table,
                if_exists: false,
            },
        )
        .await
        .is_err(),
        "without the guard a missing table must be an error"
    );
}

/// An index path trimmed to `table.index` (no schema) still addresses the
/// index, which is stored beside the table rather than under it.
#[tokio::test]
async fn an_unqualified_index_path_still_resolves() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("CREATE TABLE t(a INTEGER)"))
        .await
        .expect("create table");

    ddl(
        conn.as_ref(),
        DdlOp::CreateIndex {
            path: p(&["t"]),
            name: Arc::from("t_a"),
            fields: vec![FieldPath::field("a")],
            unique: false,
            if_not_exists: false,
        },
    )
    .await
    .expect("create index on an unqualified table");
    assert_eq!(count_named(conn.as_ref(), "t_a").await, 1);

    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: p(&["t", "t_a"]),
            kind: ObjectKind::Index,
            if_exists: false,
        },
    )
    .await
    .expect("drop index by an unqualified path");
    assert_eq!(count_named(conn.as_ref(), "t_a").await, 0);
}

/// SQLite renames tables and nothing else: `ALTER VIEW` and `ALTER INDEX` are
/// syntax errors, and `ALTER TABLE <view> RENAME` is refused by the engine.
#[tokio::test]
async fn only_a_table_can_be_renamed() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("CREATE TABLE t(a INTEGER)"))
        .await
        .expect("create table");
    conn.execute(Request::native("CREATE VIEW v AS SELECT * FROM t"))
        .await
        .expect("create view");

    for kind in [ObjectKind::View, ObjectKind::Index] {
        let err = ddl(
            conn.as_ref(),
            DdlOp::Rename {
                from: p(&["main", "v"]),
                to: p(&["main", "v2"]),
                kind,
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, DbError::Unsupported { .. }),
            "{kind:?}: {err:?}"
        );
    }
    assert_eq!(count_named(conn.as_ref(), "v").await, 1);
}

/// A name that tries to close its own quoting must reach the engine as one
/// name and take nothing else with it.
#[tokio::test]
async fn a_hostile_name_survives_a_structured_drop() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("CREATE TABLE bystander(a INTEGER)"))
        .await
        .expect("create bystander");
    conn.execute(Request::native(
        "CREATE TABLE \"ddl\"\"; DROP TABLE bystander; --\"(a INTEGER)",
    ))
    .await
    .expect("create hostile");

    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: p(&["main", "ddl\"; DROP TABLE bystander; --"]),
            kind: ObjectKind::Table,
            if_exists: false,
        },
    )
    .await
    .expect("drop the hostile table");
    assert_eq!(
        count_named(conn.as_ref(), "bystander").await,
        1,
        "the bystander table must still be there"
    );
}

/// A read-only connection refuses generated DDL, like any other write.
#[tokio::test]
async fn read_only_refuses_structured_ddl() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("CREATE TABLE t(a INTEGER)"))
        .await
        .expect("create table");
    conn.set_read_only(true).await.expect("read-only");

    assert!(ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: p(&["main", "t"]),
            kind: ObjectKind::Table,
            if_exists: false,
        },
    )
    .await
    .is_err());
    conn.set_read_only(false).await.expect("writable again");
    assert_eq!(count_named(conn.as_ref(), "t").await, 1);
}
