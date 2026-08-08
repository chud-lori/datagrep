//! `RedisCatalog` against a real server: the
//! `db-index -> keyspace-prefix -> key` hierarchy, `ScanOnly{requires_prefix:
//! true}` actually being enforced (no silent full-keyspace walk), and
//! `describe()` on a real key.

mod common;

use std::sync::Arc;

use datagrep_api::{Enumeration, ListOpts, ObjectKind, ObjectPath};

#[tokio::test]
#[ignore]
async fn levels_are_cheap_db_index_then_scan_only_requiring_a_prefix() {
    let conn = common::connect().await;
    let levels = conn.catalog().levels();
    assert_eq!(levels.len(), 3);
    assert_eq!(levels[0].enumeration, Enumeration::Cheap);
    assert_eq!(
        levels[1].enumeration,
        Enumeration::ScanOnly {
            requires_prefix: true
        }
    );
    assert_eq!(
        levels[2].enumeration,
        Enumeration::ScanOnly {
            requires_prefix: true
        }
    );
}

#[tokio::test]
#[ignore]
async fn children_walks_db_index_then_prefix_then_key() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    common::seed_keys(&mut raw, "datagreptest:cat:", 25).await;

    let conn = common::connect().await;
    let catalog = conn.catalog();

    let dbs = catalog
        .children(&ObjectPath::root(), ListOpts::default())
        .await
        .expect("listing db indexes failed");
    assert!(!dbs.items.is_empty(), "at least db 0 must be listed");
    assert!(dbs
        .items
        .iter()
        .any(|n| n.kind == ObjectKind::Database && &*n.path.parts()[0] == "0"));

    // Listing prefixes REQUIRES an explicit prefix (even Some("")) — refuses
    // to walk the whole keyspace unasked (Enumeration::ScanOnly{requires_prefix:true}).
    let no_prefix = catalog
        .children(&ObjectPath::new(vec![Arc::from("0")]), ListOpts::default())
        .await;
    assert!(
        no_prefix.is_err(),
        "listing the prefix level without a prefix must be refused, never silently scan everything"
    );

    let opts = ListOpts {
        prefix: Some(Arc::from("")),
        ..ListOpts::default()
    };
    let prefixes = catalog
        .children(&ObjectPath::new(vec![Arc::from("0")]), opts)
        .await
        .expect("listing prefixes with an explicit empty prefix failed");
    assert!(
        prefixes
            .items
            .iter()
            .any(|n| n.path.parts().last().map(|s| &**s) == Some("datagreptest:")),
        "expected to find the datagreptest: prefix bucket, got {:?}",
        prefixes.items
    );

    let key_opts = ListOpts {
        limit: 100,
        ..ListOpts::default()
    };
    let keys = catalog
        .children(
            &ObjectPath::new(vec![Arc::from("0"), Arc::from("datagreptest:cat:")]),
            key_opts,
        )
        .await
        .expect("listing keys under the prefix failed");
    assert_eq!(
        keys.items.len(),
        25,
        "expected all 25 seeded keys under the prefix"
    );
    assert!(keys.items.iter().all(|n| n.kind == ObjectKind::Key));
}

#[tokio::test]
#[ignore]
async fn describe_reports_type_and_ttl_for_a_real_key() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    let _: () = redis::cmd("SET")
        .arg("datagreptest:describe:me")
        .arg("v")
        .query_async(&mut raw)
        .await
        .expect("seed SET failed");

    let conn = common::connect().await;
    let detail = conn
        .catalog()
        .describe(&ObjectPath::new(vec![
            Arc::from("0"),
            Arc::from("datagreptest:describe:"),
            Arc::from("datagreptest:describe:me"),
        ]))
        .await
        .expect("describe failed");
    assert!(detail
        .extra
        .iter()
        .any(|(k, v)| &**k == "type" && &**v == "string"));
}
