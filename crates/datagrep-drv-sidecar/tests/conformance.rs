// The same fixtures the Go conformance test encodes; here they are decoded.
use serde_json::Value as Json;

use datagrep_api::shape::LogicalType;
use datagrep_drv_sidecar::wire::{
    decode_cell, ExecuteReply, FetchReply, HelloReply, Incoming, WireShape,
};

const CELLS: &str = include_str!("../../../sidecar/conformance/cells.json");
const FRAMES: &str = include_str!("../../../sidecar/conformance/frames.json");

#[test]
fn cells_decode_to_the_values_the_fixtures_name() {
    let cases: Vec<Json> = serde_json::from_str(CELLS).unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let logical: LogicalType = serde_json::from_value(case["logical"].clone()).unwrap();
        let value =
            decode_cell(case["wire"].clone(), logical).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            format!("{value:?}"),
            case["rust"].as_str().unwrap(),
            "{name}"
        );
    }
}

#[test]
fn frames_parse_into_the_reply_types() {
    let frames: serde_json::Map<String, Json> = serde_json::from_str(FRAMES).unwrap();
    let get = |k: &str| frames[k].clone();

    let hello: HelloReply = serde_json::from_value(get("hello")).unwrap();
    assert_eq!(hello.language, serde_json::json!("Unclassified"));
    assert_eq!(hello.caps, datagrep_drv_sidecar::ORACLE.flags.bits());

    let table: ExecuteReply = serde_json::from_value(get("execute_table")).unwrap();
    assert!(matches!(table.shape, WireShape::Table { ref fields } if fields.len() == 2));
    let ack: ExecuteReply = serde_json::from_value(get("execute_ack")).unwrap();
    assert_eq!(ack.cursor, 0);
    assert!(matches!(
        ack.shape,
        WireShape::Ack {
            affected: Some(3),
            ..
        }
    ));

    let rows: FetchReply = serde_json::from_value(get("fetch_rows")).unwrap();
    assert_eq!(rows.batch.unwrap().rows.len(), 2);
    let eof: FetchReply = serde_json::from_value(get("fetch_eof")).unwrap();
    assert!(eof.batch.is_none());

    let err: Incoming = serde_json::from_value(get("error")).unwrap();
    let db = err.err.unwrap().into_db_error();
    assert_eq!(
        db.to_string(),
        "query failed [ORA-00942]: table or view does not exist"
    );
}
