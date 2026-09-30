use serde::Deserialize;

use crate::model::SafetyDecision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    Markdown,
    Sql,
}

impl ExportFormat {
    /// What the engine offers for this driver; SQL only where an INSERT can recreate the rows.
    pub fn parse_list(json: &str) -> Vec<ExportFormat> {
        serde_json::from_str::<Vec<String>>(json)
            .unwrap_or_default()
            .iter()
            .filter_map(|name| ExportFormat::parse(name))
            .collect()
    }

    pub fn parse(name: &str) -> Option<ExportFormat> {
        match name {
            "csv" => Some(ExportFormat::Csv),
            "json" => Some(ExportFormat::Json),
            "markdown" => Some(ExportFormat::Markdown),
            "sql" => Some(ExportFormat::Sql),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::Markdown => "markdown",
            ExportFormat::Sql => "sql",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Json => "JSON",
            ExportFormat::Markdown => "Markdown table",
            ExportFormat::Sql => "SQL INSERT statements",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "md",
            other => other.as_str(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum ExportState {
    #[default]
    Running,
    Done,
    Cancelled,
    Failed,
}

impl From<String> for ExportState {
    fn from(state: String) -> Self {
        match state.as_str() {
            "running" => ExportState::Running,
            "done" => ExportState::Done,
            "cancelled" => ExportState::Cancelled,
            _ => ExportState::Failed,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExportStatus {
    #[serde(default)]
    pub state: ExportState,
    #[serde(default)]
    pub rows_written: u64,
    #[serde(default)]
    pub error: Option<String>,
    // Non-null on a failed state means the ladder refused and nothing was sent.
    #[serde(default)]
    pub safety: Option<SafetyDecision>,
}

impl ExportStatus {
    pub fn parse(json: &str) -> Self {
        serde_json::from_str(json).unwrap_or_else(|e| ExportStatus {
            state: ExportState::Failed,
            error: Some(format!("unreadable export status: {e}")),
            ..ExportStatus::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_formats_the_engine_offers_are_listed() {
        assert_eq!(
            ExportFormat::parse_list(r#"["csv","json","markdown"]"#),
            [
                ExportFormat::Csv,
                ExportFormat::Json,
                ExportFormat::Markdown
            ]
        );
        assert_eq!(
            ExportFormat::parse_list(r#"["sql","xlsx"]"#),
            [ExportFormat::Sql]
        );
        assert!(ExportFormat::parse_list("not json").is_empty());
    }

    #[test]
    fn a_markdown_file_is_named_md() {
        assert_eq!(ExportFormat::Markdown.extension(), "md");
        assert_eq!(ExportFormat::Sql.extension(), "sql");
    }

    #[test]
    fn a_safety_refusal_rides_along_on_a_failed_export() {
        let status = ExportStatus::parse(
            r#"{"state":"failed","rows_written":0,"error":"refused",
                "safety":{"profile":"prod","level":"auth_all","requires":"authenticate",
                          "challenge":"c1","statements":[]}}"#,
        );
        assert_eq!(status.state, ExportState::Failed);
        assert_eq!(
            status.safety.and_then(|d| d.challenge).as_deref(),
            Some("c1")
        );
    }

    #[test]
    fn an_unreadable_status_is_a_failure_not_a_hang() {
        let status = ExportStatus::parse("{");
        assert_eq!(status.state, ExportState::Failed);
        assert!(status.error.is_some());
    }
}
