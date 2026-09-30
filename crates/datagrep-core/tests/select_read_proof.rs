use datagrep_api::safety::{Requirement, SafetyLevel};
use datagrep_api::{LanguageId, SqlDialect};
use datagrep_core::safety::SafetyGate;
use datagrep_core::ProfileId;

#[test]
fn auth_writes_authenticates_selects_that_are_not_proven_reads() {
    let gate = SafetyGate::new(
        ProfileId(1),
        "prod",
        LanguageId::Sql(SqlDialect::Postgres),
        SafetyLevel::AuthWrites,
    );
    assert_eq!(
        gate.plan("SELECT * FROM users").requirement,
        Requirement::None
    );
    for stmt in [
        "SELECT * INTO backup_users FROM users",
        "SELECT pg_terminate_backend(4242)",
        "SELECT setval('orders_id_seq', 1)",
        "SELECT * FROM accounts FOR UPDATE",
        "SELECT pg_sleep(3600)",
        "SELECT * FROM users INTO OUTFILE '/tmp/users.csv'",
        "SELECT GET_LOCK('deploy', 0)",
        "SELECT lo_unlink(16401)",
    ] {
        assert_eq!(
            gate.plan(stmt).requirement,
            Requirement::Authenticate,
            "{stmt}"
        );
    }
}
