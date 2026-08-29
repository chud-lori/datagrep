use serde::Deserialize;

use crate::model::format;

/// The decoded `datagrep_catalog_describe_json` payload — one shape for every engine.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ObjectDetail {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub comment: Option<String>,
    /// `None` = the engine declares no schema; `Some([])` = it says there are none.
    #[serde(default)]
    pub columns: Option<Vec<DetailColumn>>,
    #[serde(default)]
    pub indexes: Option<Vec<DetailIndex>>,
    #[serde(default)]
    pub row_estimate: Option<i64>,
    #[serde(default)]
    pub size_bytes: Option<f64>,
    #[serde(default)]
    pub inferred: bool,
    #[serde(default)]
    pub sampled_docs: Option<i64>,
    #[serde(default)]
    pub extra: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DetailColumn {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub native_type: Option<String>,
    #[serde(default)]
    pub logical_type: Option<String>,
    #[serde(default, rename = "type")]
    pub loose_type: Option<String>,
    #[serde(default)]
    pub nullable: Option<bool>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub indexed: bool,
    #[serde(default)]
    pub auto_generated: bool,
    #[serde(default)]
    pub presence_ratio: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IndexColumn {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub order: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DetailIndex {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub columns: Vec<IndexColumn>,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub primary: bool,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<f64>,
    #[serde(default)]
    pub definition: Option<String>,
    #[serde(default)]
    pub sparse: bool,
    #[serde(default)]
    pub expire_after_seconds: Option<i64>,
}

fn joined(parts: Vec<String>) -> String {
    parts.join(" · ")
}

impl ObjectDetail {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("the object detail did not decode: {e}"))
    }

    /// Only facts that arrived: a stat the driver did not report is not printed as zero.
    pub fn stats(&self) -> String {
        let mut parts = Vec::new();
        if !self.kind.is_empty() {
            parts.push(self.kind.clone());
        }
        if let Some(rows) = self.row_estimate {
            // "≈" because a cheap estimate is never a COUNT(*).
            parts.push(format!("≈ {} rows", format::count(rows.max(0) as u64)));
        }
        if let Some(size) = self.size_bytes {
            parts.push(format::bytes(size));
        }
        if self.inferred {
            parts.push(match self.sampled_docs {
                Some(docs) => format!(
                    "inferred from {} sampled docs",
                    format::count(docs.max(0) as u64)
                ),
                None => "inferred".to_owned(),
            });
        }
        joined(parts)
    }
}

impl DetailColumn {
    pub fn details(&self) -> String {
        let mut parts = Vec::new();
        let ty = self
            .native_type
            .as_deref()
            .or(self.logical_type.as_deref())
            .or(self.loose_type.as_deref())
            .unwrap_or_default();
        if !ty.is_empty() {
            parts.push(ty.to_owned());
        }
        if self.primary_key {
            parts.push("primary key".to_owned());
        }
        if self.unique {
            parts.push("unique".to_owned());
        }
        if self.indexed {
            parts.push("indexed".to_owned());
        }
        if self.nullable == Some(false) {
            parts.push("not null".to_owned());
        }
        if self.auto_generated {
            parts.push("auto".to_owned());
        }
        if let Some(default) = self.default.as_deref().filter(|d| !d.is_empty()) {
            parts.push(format!("default {default}"));
        }
        // Sampled documents only: how often the field was actually present.
        if let Some(presence) = self.presence_ratio {
            parts.push(format!("in {}% of sampled docs", (presence * 100.0) as i64));
        }
        joined(parts)
    }
}

impl DetailIndex {
    pub fn details(&self) -> String {
        let mut parts = Vec::new();
        let keys: Vec<String> = self
            .columns
            .iter()
            .map(
                |column| match column.order.as_deref().filter(|o| !o.is_empty()) {
                    Some(order) => format!("{} {order}", column.name),
                    None => column.name.clone(),
                },
            )
            .collect();
        if !keys.is_empty() {
            parts.push(keys.join(", "));
        }
        if self.primary {
            parts.push("primary".to_owned());
        }
        if self.unique {
            parts.push("unique".to_owned());
        }
        if let Some(kind) = self.kind.as_deref().filter(|k| !k.is_empty()) {
            parts.push(kind.to_owned());
        }
        if self.sparse {
            parts.push("sparse".to_owned());
        }
        if self.partial {
            parts.push(match self.filter.as_deref().filter(|f| !f.is_empty()) {
                Some(filter) => format!("partial: {filter}"),
                None => "partial".to_owned(),
            });
        }
        if let Some(seconds) = self.expire_after_seconds {
            parts.push(format!("expires after {seconds} s"));
        }
        if let Some(size) = self.size_bytes {
            parts.push(format::bytes(size));
        }
        joined(parts)
    }
}

/// Re-indented for reading. One parse: the panel never decodes this text twice.
pub fn pretty_json(json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(value) => {
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| json.trim().to_owned())
        }
        Err(_) => json.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#"{
        "path":["main","users"],"name":"users","kind":"table","comment":"people",
        "columns":[{"name":"id","ordinal":0,"native_type":"INTEGER","logical_type":"I64",
                    "type":"I64","nullable":false,"primary_key":true,"indexed":true,
                    "auto_generated":true,"default":null},
                   {"name":"email","native_type":"TEXT","nullable":true,"unique":true,
                    "default":"''","presence_ratio":0.66}],
        "indexes":[{"name":"idx_email","columns":[{"name":"email","order":"asc"}],
                    "unique":true,"primary":false,"type":"btree","partial":true,
                    "filter":"(deleted_at IS NULL)","size_bytes":16384,"sparse":false}],
        "row_estimate":4210,"size_bytes":8192,"inferred":false,"sampled_docs":null,
        "extra":[["engine","sqlite"]]}"#;

    #[test]
    fn a_describe_payload_decodes_into_the_documented_shape() {
        let detail = ObjectDetail::parse(TABLE).expect("valid detail");
        assert_eq!(detail.name, "users");
        assert_eq!(detail.path, ["main", "users"]);
        assert_eq!(detail.columns.as_ref().map(Vec::len), Some(2));
        assert_eq!(detail.indexes.as_ref().map(Vec::len), Some(1));
        assert_eq!(detail.extra, [("engine".to_owned(), "sqlite".to_owned())]);
        assert_eq!(detail.stats(), "table · ≈ 4,210 rows · 8.0 KB");
    }

    #[test]
    fn a_column_reports_the_engine_spelling_and_every_flag_that_is_set() {
        let detail = ObjectDetail::parse(TABLE).expect("valid detail");
        let columns = detail.columns.expect("columns");
        assert_eq!(
            columns[0].details(),
            "INTEGER · primary key · indexed · not null · auto"
        );
        assert_eq!(
            columns[1].details(),
            "TEXT · unique · default '' · in 66% of sampled docs"
        );
    }

    #[test]
    fn an_index_reports_its_keys_first_then_what_makes_it_unusual() {
        let detail = ObjectDetail::parse(TABLE).expect("valid detail");
        let indexes = detail.indexes.expect("indexes");
        assert_eq!(
            indexes[0].details(),
            "email asc · unique · btree · partial: (deleted_at IS NULL) · 16.0 KB"
        );
    }

    #[test]
    fn no_columns_and_no_reported_columns_are_two_different_answers() {
        let none = ObjectDetail::parse(r#"{"name":"k","columns":null,"indexes":[]}"#)
            .expect("valid detail");
        assert!(
            none.columns.is_none(),
            "null means the engine did not report"
        );
        assert_eq!(none.indexes.as_ref().map(Vec::len), Some(0));
    }

    #[test]
    fn a_sampled_schema_says_what_it_was_sampled_from() {
        let detail = ObjectDetail::parse(
            r#"{"name":"logs","kind":"collection","inferred":true,"sampled_docs":500}"#,
        )
        .expect("valid detail");
        assert_eq!(
            detail.stats(),
            "collection · inferred from 500 sampled docs"
        );
    }

    #[test]
    fn a_cell_value_that_is_not_json_is_shown_rather_than_swallowed() {
        assert_eq!(pretty_json("  not json  "), "not json");
        assert_eq!(pretty_json(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
    }
}
