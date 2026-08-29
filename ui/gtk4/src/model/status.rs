use serde::Deserialize;

use crate::model::mutation::EditableResult;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum QueryState {
    #[default]
    Streaming,
    Parked,
    Capped,
    Done,
    Cancelled,
    Failed,
}

impl From<String> for QueryState {
    fn from(state: String) -> Self {
        match state.as_str() {
            "streaming" => QueryState::Streaming,
            "parked" => QueryState::Parked,
            "capped" => QueryState::Capped,
            "done" => QueryState::Done,
            "cancelled" => QueryState::Cancelled,
            _ => QueryState::Failed,
        }
    }
}

impl QueryState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, QueryState::Streaming | QueryState::Parked)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(default, rename = "type")]
    pub ty: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct QueryStatus {
    #[serde(default)]
    pub state: QueryState,
    #[serde(default)]
    pub rows_loaded: u64,
    #[serde(default)]
    pub affected_rows: Option<u64>,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub columns: Vec<Column>,
    #[serde(default)]
    pub total_known: bool,
    #[serde(default)]
    pub editable: Option<EditableResult>,
}

impl QueryStatus {
    pub fn parse(json: &str) -> Self {
        let mut status: Self =
            serde_json::from_str(json).unwrap_or_else(|e| Self::failed(e.to_string()));
        // An editable block naming no identity field could not address a write.
        if status
            .editable
            .as_ref()
            .is_some_and(|e| e.identity.is_empty())
        {
            status.editable = None;
        }
        status
    }

    pub fn failed(error: String) -> Self {
        Self {
            state: QueryState::Failed,
            error: Some(error),
            ..Self::default()
        }
    }

    pub fn is_streaming(&self) -> bool {
        !self.state.is_terminal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streaming_snapshot_decodes() {
        let s = QueryStatus::parse(
            r#"{"state":"streaming","rows_loaded":4096,"affected_rows":null,
                "elapsed_ms":12,"error":null,"total_known":false,
                "columns":[{"name":"id","type":"int8"},{"name":"note","type":"text"}]}"#,
        );
        assert_eq!(s.state, QueryState::Streaming);
        assert!(s.is_streaming());
        assert_eq!(s.rows_loaded, 4096);
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.columns[1].ty, "text");
    }

    #[test]
    fn capped_and_cancelled_are_terminal() {
        assert!(!QueryStatus::parse(r#"{"state":"capped"}"#).is_streaming());
        assert!(!QueryStatus::parse(r#"{"state":"cancelled"}"#).is_streaming());
        assert!(QueryStatus::parse(r#"{"state":"parked"}"#).is_streaming());
    }

    #[test]
    fn the_editable_block_rides_along_and_an_identityless_one_is_dropped() {
        let s = QueryStatus::parse(
            r#"{"state":"done","editable":{"identity":["_index","_id"],
                "guard":["_seq_no","_primary_term"],"root":"_source","atomic_batch":false}}"#,
        );
        let editable = s.editable.expect("an editable result");
        assert_eq!(editable.guard, ["_seq_no", "_primary_term"]);
        assert!(
            QueryStatus::parse(r#"{"state":"done","editable":{"identity":[]}}"#)
                .editable
                .is_none()
        );
    }

    #[test]
    fn keys_this_build_does_not_know_are_ignored_not_fatal() {
        let s = QueryStatus::parse(
            r#"{"state":"done","rows_loaded":3,"read_only":{"enforcement":"client",
                "server_confirmed":false},"editable":null,"future_key":[1,2]}"#,
        );
        assert_eq!(s.state, QueryState::Done);
        assert_eq!(s.rows_loaded, 3);
    }

    #[test]
    fn an_unknown_state_degrades_to_failed_rather_than_a_parse_error() {
        assert_eq!(
            QueryStatus::parse(r#"{"state":"teleporting"}"#).state,
            QueryState::Failed
        );
    }

    #[test]
    fn unparseable_json_becomes_a_failure_carrying_the_reason() {
        let s = QueryStatus::parse("not json");
        assert_eq!(s.state, QueryState::Failed);
        assert!(s.error.is_some());
    }
}
