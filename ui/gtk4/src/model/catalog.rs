use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum Enumeration {
    Cheap,
    Paged,
    ScanOnly,
    #[default]
    OnDemand,
}

impl From<String> for Enumeration {
    fn from(value: String) -> Self {
        match value.as_str() {
            "cheap" => Enumeration::Cheap,
            "paged" => Enumeration::Paged,
            "scan_only" => Enumeration::ScanOnly,
            _ => Enumeration::OnDemand,
        }
    }
}

impl Enumeration {
    /// A level this costly is never listed by an arrow click alone — the user asks twice.
    pub fn needs_consent(self) -> bool {
        matches!(self, Enumeration::ScanOnly | Enumeration::OnDemand)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogNode {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub has_children: bool,
    #[serde(default)]
    pub enumeration: Enumeration,
}

impl CatalogNode {
    pub fn parse_list(json: &str) -> Result<Vec<Self>, String> {
        serde_json::from_str(json).map_err(|e| format!("the catalog page did not decode: {e}"))
    }

    /// Kinds whose rows a click can open; a Redis key's shape is not one.
    pub fn browsable_kind(kind: &str) -> bool {
        matches!(kind, "table" | "collection" | "view")
    }

    pub fn icon_name(&self) -> &'static str {
        match self.kind.as_str() {
            "database" => "drive-harddisk-symbolic",
            "schema" => "folder-symbolic",
            "table" | "collection" => "view-list-symbolic",
            "view" => "view-reveal-symbolic",
            "column" | "field" => "format-justify-left-symbolic",
            "index" => "view-sort-ascending-symbolic",
            "key" => "dialog-password-symbolic",
            "function" => "system-run-symbolic",
            _ => "text-x-generic-symbolic",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catalog_page_decodes() {
        let nodes = CatalogNode::parse_list(
            r#"[{"name":"public","kind":"schema","has_children":true,"enumeration":"cheap"},
                {"name":"users","kind":"table","has_children":true,"enumeration":"paged"}]"#,
        )
        .expect("valid page");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].enumeration, Enumeration::Cheap);
        assert!(nodes[1].has_children);
    }

    #[test]
    fn only_the_kinds_with_rows_are_browsable() {
        for kind in ["table", "collection", "view"] {
            assert!(CatalogNode::browsable_kind(kind), "{kind}");
        }
        for kind in ["database", "schema", "key", "column", "function", ""] {
            assert!(!CatalogNode::browsable_kind(kind), "{kind}");
        }
    }

    #[test]
    fn an_unknown_enumeration_degrades_to_the_most_cautious_one() {
        let nodes = CatalogNode::parse_list(r#"[{"name":"k","enumeration":"telepathy"}]"#)
            .expect("valid page");
        assert_eq!(nodes[0].enumeration, Enumeration::OnDemand);
        assert!(nodes[0].enumeration.needs_consent());
    }

    #[test]
    fn a_redis_keyspace_is_never_listed_by_an_arrow_click_alone() {
        assert!(Enumeration::ScanOnly.needs_consent());
        assert!(!Enumeration::Cheap.needs_consent());
        assert!(!Enumeration::Paged.needs_consent());
    }
}
