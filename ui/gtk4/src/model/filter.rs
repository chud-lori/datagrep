use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FilterOperator {
    pub op: String,
    pub label: String,
    pub needs_value: bool,
}

impl FilterOperator {
    /// What the engine offers for this driver; empty where it cannot wrap the statement.
    pub fn parse_list(json: &str) -> Vec<FilterOperator> {
        serde_json::from_str(json).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowFilter {
    pub column: String,
    pub op: String,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_list_parses_and_a_non_sql_engine_offers_nothing() {
        let sql = FilterOperator::parse_list(&crate::ffi::filter_operators_json("sqlite").unwrap());
        assert_eq!(
            sql.first(),
            Some(&FilterOperator {
                op: "eq".into(),
                label: "=".into(),
                needs_value: true
            })
        );
        assert!(sql.iter().any(|o| o.op == "is_null" && !o.needs_value));
        let mongo =
            FilterOperator::parse_list(&crate::ffi::filter_operators_json("mongodb").unwrap());
        assert!(mongo.is_empty());
    }
}
