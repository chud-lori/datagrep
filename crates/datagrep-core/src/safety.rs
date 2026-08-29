use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use datagrep_api::error::DbError;
use datagrep_api::request::{Op, Request};
use datagrep_api::safety::{Attestation, Requirement, SafetyLevel};
use datagrep_api::LanguageId;
use datagrep_lang::StatementClass;

use crate::api::ProfileId;
use crate::lock;

const CHALLENGE_TTL: Duration = Duration::from_secs(300);
const GRANT_TTL: Duration = Duration::from_secs(120);
const MAX_PENDING: usize = 16;
const MAX_GRANTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyStatement {
    pub text: String,
    pub class: StatementClass,
    pub requirement: Requirement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyDecision {
    pub profile: Arc<str>,
    pub level: SafetyLevel,
    pub requirement: Requirement,
    pub statements: Vec<SafetyStatement>,
    pub challenge: Option<Arc<str>>,
}

struct Pending {
    decision: SafetyDecision,
    bindings: Vec<String>,
    born: Instant,
}

struct Grant {
    binding: String,
    requirement: Requirement,
    born: Instant,
}

struct GateState {
    level: SafetyLevel,
    pending: Vec<Pending>,
    grants: Vec<Grant>,
}

impl GateState {
    fn prune(&mut self) {
        let now = Instant::now();
        self.pending
            .retain(|p| now.duration_since(p.born) < CHALLENGE_TTL);
        self.grants
            .retain(|g| now.duration_since(g.born) < GRANT_TTL);
    }
}

// One rung of the ladder, for one connection. Nothing outside this type can mint a grant.
pub struct SafetyGate {
    profile: ProfileId,
    name: Arc<str>,
    language: LanguageId,
    state: Mutex<GateState>,
    seed: u64,
    next: AtomicU64,
}

impl SafetyGate {
    pub fn new(
        profile: ProfileId,
        name: impl Into<Arc<str>>,
        language: LanguageId,
        level: SafetyLevel,
    ) -> Arc<Self> {
        Arc::new(Self {
            profile,
            name: name.into(),
            language,
            state: Mutex::new(GateState {
                level,
                pending: Vec::new(),
                grants: Vec::new(),
            }),
            seed: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0),
            next: AtomicU64::new(1),
        })
    }

    pub fn profile(&self) -> ProfileId {
        self.profile
    }

    pub fn name(&self) -> &Arc<str> {
        &self.name
    }

    pub fn level(&self) -> SafetyLevel {
        lock(&self.state).level
    }

    // Raising or lowering the rung invalidates everything the old rung cleared.
    pub fn set_level(&self, level: SafetyLevel) {
        let mut state = lock(&self.state);
        if state.level == level {
            return;
        }
        state.level = level;
        state.pending.clear();
        state.grants.clear();
    }

    // What running `sql` on this connection would require, and the challenge that clears it.
    pub fn plan(&self, sql: &str) -> SafetyDecision {
        let language = datagrep_lang::language_for(self.language);
        let mut statements = Vec::new();
        let mut bindings = Vec::new();
        let mut requirement = Requirement::None;

        for span in language.split(sql) {
            let text = span.text(sql).trim().to_string();
            if text.is_empty() {
                continue;
            }
            let class = language.classify(&text);
            let stmt = self.level().requirement(class == StatementClass::Read);
            requirement = requirement.max(stmt);
            bindings.push(text.clone());
            statements.push(SafetyStatement {
                text,
                class,
                requirement: stmt,
            });
        }

        // A caller that submits the whole script as one request binds to the whole script.
        if bindings.len() > 1 {
            bindings.push(sql.trim().to_string());
        }

        let mut decision = SafetyDecision {
            profile: self.name.clone(),
            level: self.level(),
            requirement,
            statements,
            challenge: None,
        };
        if requirement > Requirement::None {
            decision.challenge = Some(self.mint(decision.clone(), bindings));
        }
        decision
    }

    pub fn pending(&self) -> Vec<SafetyDecision> {
        let mut state = lock(&self.state);
        state.prune();
        state.pending.iter().map(|p| p.decision.clone()).collect()
    }

    pub fn decision(&self, challenge: &str) -> Option<SafetyDecision> {
        let mut state = lock(&self.state);
        state.prune();
        state
            .pending
            .iter()
            .find(|p| p.decision.challenge.as_deref() == Some(challenge))
            .map(|p| p.decision.clone())
    }

    // The only way a grant comes into being: an engine-minted challenge plus evidence the engine judges.
    pub fn satisfy(&self, challenge: &str, attestation: &Attestation) -> Result<(), DbError> {
        let mut state = lock(&self.state);
        state.prune();
        let Some(index) = state
            .pending
            .iter()
            .position(|p| p.decision.challenge.as_deref() == Some(challenge))
        else {
            return Err(DbError::Auth(format!(
                "no open safety challenge `{challenge}` on `{}` — it expired, was already used, or was never issued",
                self.name
            )));
        };

        let requirement = state.pending[index].decision.requirement;
        if !attestation.satisfies(requirement, &self.name) {
            return Err(DbError::Auth(format!(
                "`{}` requires {requirement} and this did not provide it",
                self.name
            )));
        }

        let entry = state.pending.remove(index);
        let born = Instant::now();
        for binding in entry.bindings {
            state.grants.push(Grant {
                binding,
                requirement,
                born,
            });
        }
        while state.grants.len() > MAX_GRANTS {
            state.grants.remove(0);
        }
        Ok(())
    }

    // Called on the one path every request takes; a caller that knows nothing about safe mode is refused here.
    pub(crate) fn admit(&self, req: &Request) -> Result<(), DbError> {
        let binding = binding_of(req);
        let requirement = self.level().requirement(self.is_read(req));
        if requirement == Requirement::None {
            return Ok(());
        }

        let mut state = lock(&self.state);
        state.prune();
        if let Some(index) = state
            .grants
            .iter()
            .position(|g| g.binding == binding && g.requirement >= requirement)
        {
            state.grants.remove(index);
            return Ok(());
        }
        drop(state);

        let decision = self.decision_for(req, requirement);
        let challenge = self.mint(decision, vec![binding]);
        Err(DbError::Safety {
            profile: self.name.to_string(),
            requirement,
            challenge: challenge.to_string(),
        })
    }

    fn decision_for(&self, req: &Request, requirement: Requirement) -> SafetyDecision {
        let (text, class) = match req {
            Request::Native { text, .. } => (text.trim().to_string(), self.classify(text)),
            Request::Op(op) => (
                describe_op(op),
                if self.op_is_read(op) {
                    StatementClass::Read
                } else {
                    StatementClass::Write
                },
            ),
        };
        SafetyDecision {
            profile: self.name.clone(),
            level: self.level(),
            requirement,
            statements: vec![SafetyStatement {
                text,
                class,
                requirement,
            }],
            challenge: None,
        }
    }

    fn mint(&self, mut decision: SafetyDecision, bindings: Vec<String>) -> Arc<str> {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let id: Arc<str> = Arc::from(format!("{:x}-{:x}", self.seed, n).as_str());
        decision.challenge = Some(id.clone());

        let mut state = lock(&self.state);
        state.prune();
        state.pending.push(Pending {
            decision,
            bindings,
            born: Instant::now(),
        });
        while state.pending.len() > MAX_PENDING {
            state.pending.remove(0);
        }
        id
    }

    fn classify(&self, stmt: &str) -> StatementClass {
        datagrep_lang::language_for(self.language).classify(stmt)
    }

    // Only a statement datagrep-lang calls Read is a read; a script must be read all the way through.
    fn is_read(&self, req: &Request) -> bool {
        match req {
            Request::Native { text, .. } => {
                let language = datagrep_lang::language_for(self.language);
                let spans = language.split(text);
                let mut seen = false;
                for span in spans {
                    let stmt = span.text(text).trim();
                    if stmt.is_empty() {
                        continue;
                    }
                    if language.classify(stmt) != StatementClass::Read {
                        return false;
                    }
                    seen = true;
                }
                seen
            }
            Request::Op(op) => self.op_is_read(op),
        }
    }

    fn op_is_read(&self, op: &Op) -> bool {
        match op {
            Op::Scan { .. } | Op::Count { .. } => true,
            // EXPLAIN ANALYZE runs the statement it explains.
            Op::Explain { inner, analyze } => !*analyze || self.is_read(inner),
            Op::Mutate(_) | Op::Ddl(_) => false,
        }
    }
}

impl fmt::Debug for SafetyGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("SafetyGate")
            .field("profile", &self.profile)
            .field("name", &self.name)
            .field("level", &state.level)
            .field("pending", &state.pending.len())
            .field("grants", &state.grants.len())
            .finish()
    }
}

// A grant is bound to exactly what will be sent, so one confirmation clears one statement and no other.
fn binding_of(req: &Request) -> String {
    match req {
        Request::Native { text, params, .. } if params.is_empty() => text.trim().to_string(),
        Request::Native { text, params, .. } => format!("{}\u{0}{params:?}", text.trim()),
        Request::Op(op) => format!("op\u{0}{op:?}"),
    }
}

fn describe_op(op: &Op) -> String {
    match op {
        Op::Scan { path, .. } => format!("scan {path}"),
        Op::Count { path, .. } => format!("count {path}"),
        Op::Mutate(batch) => format!("write {} document(s)", batch.mutations.len()),
        Op::Explain { .. } => "explain".to_string(),
        Op::Ddl(ddl) => format!("{ddl:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::request::{ExecOpts, MutationBatch};
    use datagrep_api::SqlDialect;

    fn gate(level: SafetyLevel) -> Arc<SafetyGate> {
        SafetyGate::new(
            ProfileId(1),
            "prod",
            LanguageId::Sql(SqlDialect::Postgres),
            level,
        )
    }

    fn native(sql: &str) -> Request {
        Request::Native {
            text: Arc::from(sql),
            params: Vec::new(),
            opts: ExecOpts::default(),
        }
    }

    fn challenge_of(err: DbError) -> String {
        match err {
            DbError::Safety { challenge, .. } => challenge,
            other => panic!("expected a safety refusal, got {other:?}"),
        }
    }

    #[test]
    fn silent_admits_everything_without_a_ceremony() {
        let gate = gate(SafetyLevel::Silent);
        assert!(gate.admit(&native("select 1")).is_ok());
        assert!(gate.admit(&native("delete from users")).is_ok());
        assert!(gate.pending().is_empty(), "nothing was asked, nothing kept");
    }

    #[test]
    fn a_frontend_that_never_asks_gets_refused() {
        let gate = gate(SafetyLevel::WarnWrites);
        assert!(gate.admit(&native("select 1")).is_ok(), "reads are exempt");
        let err = gate
            .admit(&native("delete from users"))
            .expect_err("a write must not slip through unasked");
        assert!(!challenge_of(err).is_empty());
    }

    #[test]
    fn every_query_rungs_gate_reads_too() {
        let warn = gate(SafetyLevel::WarnAll);
        assert!(warn.admit(&native("select 1")).is_err());
        let auth = gate(SafetyLevel::AuthAll);
        match auth.admit(&native("select 1")) {
            Err(DbError::Safety { requirement, .. }) => {
                assert_eq!(requirement, Requirement::Authenticate)
            }
            other => panic!("expected authentication, got {other:?}"),
        }
    }

    #[test]
    fn a_grant_clears_one_statement_once_and_no_other() {
        let gate = gate(SafetyLevel::WarnWrites);
        let id = challenge_of(gate.admit(&native("delete from users")).unwrap_err());
        gate.satisfy(&id, &Attestation::Acknowledged).expect("warn");

        assert!(gate.admit(&native("delete from users")).is_ok());
        assert!(
            gate.admit(&native("delete from users")).is_err(),
            "a grant is single use"
        );

        let id = challenge_of(gate.admit(&native("delete from users")).unwrap_err());
        gate.satisfy(&id, &Attestation::Acknowledged).unwrap();
        assert!(
            gate.admit(&native("drop table users")).is_err(),
            "a grant is bound to the statement it was issued for"
        );
    }

    #[test]
    fn an_acknowledgement_cannot_clear_an_authenticate_rung() {
        let gate = gate(SafetyLevel::AuthWrites);
        let id = challenge_of(gate.admit(&native("delete from users")).unwrap_err());
        let err = gate
            .satisfy(&id, &Attestation::Acknowledged)
            .expect_err("a warning is not authentication");
        assert!(matches!(err, DbError::Auth(_)));

        let wrong = gate.satisfy(
            &id,
            &Attestation::TypedPhrase {
                typed: "staging".to_string(),
            },
        );
        assert!(wrong.is_err(), "the phrase must be this connection's name");

        gate.satisfy(
            &id,
            &Attestation::TypedPhrase {
                typed: "prod".to_string(),
            },
        )
        .expect("the connection name clears it");
        assert!(gate.admit(&native("delete from users")).is_ok());
    }

    #[test]
    fn an_invented_challenge_id_clears_nothing() {
        let gate = gate(SafetyLevel::WarnAll);
        let err = gate
            .satisfy("0-1", &Attestation::Acknowledged)
            .expect_err("ids are minted here, not guessed");
        assert!(matches!(err, DbError::Auth(_)));
    }

    #[test]
    fn a_used_challenge_cannot_be_replayed() {
        let gate = gate(SafetyLevel::WarnAll);
        let id = challenge_of(gate.admit(&native("select 1")).unwrap_err());
        gate.satisfy(&id, &Attestation::Acknowledged).unwrap();
        assert!(gate.satisfy(&id, &Attestation::Acknowledged).is_err());
    }

    #[test]
    fn planning_a_script_clears_each_statement_it_listed() {
        let gate = gate(SafetyLevel::WarnWrites);
        let sql = "select 1; delete from users; update users set a = 1";
        let decision = gate.plan(sql);
        assert_eq!(decision.requirement, Requirement::Warn);
        assert_eq!(decision.statements.len(), 3);
        assert_eq!(decision.statements[0].requirement, Requirement::None);

        let id = decision.challenge.clone().expect("a challenge was minted");
        gate.satisfy(&id, &Attestation::Acknowledged).unwrap();
        assert!(gate.admit(&native("delete from users")).is_ok());
        assert!(gate.admit(&native("update users set a = 1")).is_ok());
        assert!(
            gate.admit(&native("delete from orders")).is_err(),
            "a statement the plan never listed was never cleared"
        );
    }

    #[test]
    fn a_generated_write_is_gated_like_a_typed_one() {
        let gate = gate(SafetyLevel::WarnWrites);
        let batch = Request::Op(Op::Mutate(MutationBatch::default()));
        assert!(
            gate.admit(&batch).is_err(),
            "the generated-write path must not inherit an exemption"
        );
        assert!(gate
            .admit(&Request::Op(Op::Count {
                path: datagrep_api::shape::ObjectPath::root(),
                filter: None,
                exact: false,
            }))
            .is_ok());
    }

    #[test]
    fn a_script_is_a_read_only_when_every_statement_is() {
        let gate = gate(SafetyLevel::WarnWrites);
        assert!(gate.admit(&native("select 1; select 2")).is_ok());
        assert!(gate.admit(&native("select 1; delete from users")).is_err());
        assert!(gate.admit(&native("   ")).is_err(), "empty is not a read");
    }

    #[test]
    fn changing_the_rung_invalidates_what_the_old_one_cleared() {
        let gate = gate(SafetyLevel::WarnWrites);
        let id = challenge_of(gate.admit(&native("delete from users")).unwrap_err());
        gate.satisfy(&id, &Attestation::Acknowledged).unwrap();
        gate.set_level(SafetyLevel::AuthWrites);
        assert!(gate.admit(&native("delete from users")).is_err());
    }
}
