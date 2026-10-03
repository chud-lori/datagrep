use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use datagrep_api::caps::{Caps, LanguageId, ParamStyle};
use datagrep_api::config::{ConfigValue, ConnectionConfig};
use datagrep_api::request::Request;
use datagrep_api::safety::{Requirement, SafetyLevel};
use datagrep_api::{DbError, ObjectKind};
use datagrep_core::{CoreApi, Profile, ProfileId, QueryEvent};
use datagrep_drv_sidecar::{EngineManifest, SidecarDriver};

static FAKE: EngineManifest = EngineManifest {
    id: "fake",
    display_name: "Fake",
    program: "unused",
    url_schemes: &["fake"],
    language: LanguageId::Unclassified,
    flags: Caps::SERVER_CANCEL,
    param_style: ParamStyle::None,
    identifier_quote: '"',
    default_port: 1,
    path_key: "database",
    path_label: "Database",
    levels: &[("table", ObjectKind::Table)],
    env: &[],
};

async fn core_with(mode: &'static str, safety: SafetyLevel) -> (CoreApi, ProfileId) {
    let core = CoreApi::new();
    core.register_driver("fake", move || {
        Arc::new(SidecarDriver::with_program(
            &FAKE,
            env!("CARGO_BIN_EXE_datagrep-sidecar-fake"),
            vec![mode.into()],
        ))
    });
    let mut values = BTreeMap::new();
    values.insert("password".into(), ConfigValue::Str("hunter2".into()));
    let id = core
        .add_profile_full(Profile {
            id: ProfileId(0),
            name: Arc::from("pilot"),
            driver: Arc::from("fake"),
            config: ConnectionConfig {
                driver: Arc::from("fake"),
                values,
            },
            read_only: false,
            safety,
        })
        .await;
    (core, id)
}

// The fake exits the moment any execute frame reaches it, so a live ping proves none did.
#[tokio::test]
async fn the_safety_gate_refuses_before_a_frame_is_written_and_fails_closed_on_reads() {
    let (core, id) = core_with("no-execute", SafetyLevel::AuthWrites).await;
    let lease = core.session(id).unwrap().acquire().await.unwrap();
    for stmt in ["UPDATE emp SET sal = 0", "SELECT 1 FROM dual"] {
        match lease.execute(Request::native(stmt)).await {
            Err(DbError::Safety { requirement, .. }) => {
                assert_eq!(requirement, Requirement::Authenticate, "{stmt}")
            }
            other => panic!("{stmt} was not gated: {:?}", other.err()),
        }
    }
    lease
        .ping()
        .await
        .expect("the sidecar never saw an execute");
}

#[tokio::test]
async fn a_query_streams_and_cancels_through_the_normal_core_path() {
    let (core, id) = core_with("ok", SafetyLevel::Silent).await;
    let mut events = core.queries().subscribe();
    let qid = core
        .run_query(id, Request::native("stream 5000"))
        .await
        .unwrap();
    let done = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.unwrap() {
                QueryEvent::Done { qid: q, stats } if q == qid => return stats,
                QueryEvent::Failed { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(done.rows, 5000);

    let qid = core.run_query(id, Request::native("hang")).await.unwrap();
    core.queries().cancel(qid).expect("tracked query");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let QueryEvent::CancelOutcome { qid: q, report } = events.recv().await.unwrap() {
                if q == qid {
                    return report;
                }
            }
        }
    })
    .await
    .expect("the cancel resolved");
    let lease = core.session(id).unwrap().acquire().await.unwrap();
    lease.ping().await.expect("connection usable after cancel");
}
