//! End-to-end mongosh run. Needs a real `mongosh` on PATH and a MongoDB at
//! `$DATAGREP_TEST_MONGO`, so it is `#[ignore]` and skipped by
//! `cargo test --workspace`. Run it with both present:
//!
//! ```text
//! DATAGREP_TEST_MONGO=mongodb://localhost:27017/testdb \
//!   cargo test -p datagrep-drv-mongo --test mongosh_live -- --ignored
//! ```

use datagrep_api::value::Value;
use datagrep_drv_mongo::mongosh;

#[test]
#[ignore = "needs a live mongosh + MongoDB; see module docs"]
fn runs_a_refused_script_through_mongosh() {
    let uri = std::env::var("DATAGREP_TEST_MONGO").expect("set DATAGREP_TEST_MONGO");
    let mongosh = mongosh::find_mongosh().expect("mongosh must be on PATH");

    // A script with arithmetic/variables the native parser refuses on purpose.
    let script = "const floor = 35; db.users.find({age: {$gt: floor}}).toArray()";
    let value = mongosh::run(&mongosh, &uri, script).expect("mongosh run");

    let Value::Array(rows) = value else {
        panic!("expected an array of documents, got {value:?}");
    };
    assert!(!rows.is_empty(), "expected matching documents");
    for row in rows.iter() {
        assert!(matches!(row, Value::Document(_)));
    }
}
