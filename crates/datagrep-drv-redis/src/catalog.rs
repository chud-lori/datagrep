use std::sync::Arc;

use async_trait::async_trait;

use datagrep_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use datagrep_api::driver::ResumeToken;
use datagrep_api::error::DbError;
use datagrep_api::shape::ObjectPath;
use datagrep_api::Bytes;

use crate::cmd::{derive_prefixes, prefix_glob};
use crate::error::map_redis_error;

pub const KEY_ENUMERATION_DBSIZE_THRESHOLD: i64 = 100_000;

pub fn key_enumeration_from_dbsize(dbsize: i64) -> bool {
    dbsize <= KEY_ENUMERATION_DBSIZE_THRESHOLD
}

pub(crate) const NO_PREFIX_BUCKET: &str = "(no prefix)";

const SAMPLE_COUNT: u32 = 200;

const EMPTY_INDEXES_JSON: &str = "[]";

pub struct RedisCatalog {
    manager: redis::aio::ConnectionManager,
}

impl RedisCatalog {
    pub fn new(manager: redis::aio::ConnectionManager) -> Self {
        Self { manager }
    }

    async fn list_db_indexes(&self, opts: ListOpts) -> Result<Page<ObjectNode>, DbError> {
        let mut mgr = self.manager.clone();
        let count = databases_count(&mut mgr).await;
        let prefix = opts.prefix.as_deref().unwrap_or("");
        let items = (0..count)
            .map(|i| i.to_string())
            .filter(|s| s.starts_with(prefix))
            .map(|s| ObjectNode {
                path: ObjectPath::new(vec![Arc::from(s)]),
                kind: ObjectKind::Database,
                has_children: true,
                comment: None,
            })
            .collect();
        Ok(Page { items, next: None })
    }

    async fn list_prefixes(
        &self,
        _db_index: &Arc<str>,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        let scoped = opts.prefix.ok_or_else(missing_prefix_error)?;
        let mut mgr = self.manager.clone();
        let match_glob = if scoped.is_empty() {
            None
        } else {
            Some(prefix_glob(&scoped))
        };
        let sampled_keys = scan_once(
            &mut mgr,
            opts.resume.as_ref(),
            match_glob.as_deref(),
            SAMPLE_COUNT,
        )
        .await?
        .1;
        let mut prefixes = derive_prefixes(&sampled_keys);
        if sampled_keys.iter().any(|k| !k.contains(':')) {
            prefixes.push(Arc::from(NO_PREFIX_BUCKET));
        }
        let items = prefixes
            .into_iter()
            .take(opts.limit as usize)
            .map(|p| ObjectNode {
                path: ObjectPath::new(vec![_db_index.clone(), p]),
                kind: ObjectKind::Other, // "prefix" has no ObjectKind of its own
                has_children: true,
                comment: None,
            })
            .collect();
        Ok(Page { items, next: None })
    }

    async fn list_keys(
        &self,
        db_index: &Arc<str>,
        prefix: &Arc<str>,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        let scoped = opts.prefix.as_deref().unwrap_or("");
        let match_glob = if &**prefix == NO_PREFIX_BUCKET {
            None
        } else {
            Some(prefix_glob(&format!("{prefix}{scoped}")))
        };
        let mut mgr = self.manager.clone();
        let (next, raw_keys) = scan_once(
            &mut mgr,
            opts.resume.as_ref(),
            match_glob.as_deref(),
            opts.limit,
        )
        .await?;
        let items = raw_keys
            .into_iter()
            .filter(|k| !(&**prefix == NO_PREFIX_BUCKET && k.contains(':')))
            .map(|k| ObjectNode {
                path: ObjectPath::new(vec![db_index.clone(), prefix.clone(), Arc::from(k)]),
                kind: ObjectKind::Key,
                has_children: false,
                comment: None,
            })
            .collect();
        Ok(Page { items, next })
    }

    async fn describe_key(&self, key: &str) -> Result<ObjectDetail, DbError> {
        let mut mgr = self.manager.clone();
        let ty: String = redis::cmd("TYPE")
            .arg(key)
            .query_async(&mut mgr)
            .await
            .map_err(map_redis_error)?;
        let ttl: i64 = redis::cmd("TTL")
            .arg(key)
            .query_async(&mut mgr)
            .await
            .map_err(map_redis_error)?;
        let encoding: Option<String> = redis::cmd("OBJECT")
            .arg("ENCODING")
            .arg(key)
            .query_async(&mut mgr)
            .await
            .ok();
        let memory_bytes: Option<i64> = redis::cmd("MEMORY")
            .arg("USAGE")
            .arg(key)
            .arg("SAMPLES")
            .arg(10)
            .query_async(&mut mgr)
            .await
            .ok();

        let mut extra = vec![
            (Arc::from("indexes"), Arc::from(EMPTY_INDEXES_JSON)),
            (Arc::from("type"), Arc::from(ty.as_str())),
        ];
        extra.push((
            Arc::from("ttl_seconds"),
            Arc::from(if ttl < 0 {
                "no expiry".to_string()
            } else {
                ttl.to_string()
            }),
        ));
        if let Some(enc) = &encoding {
            extra.push((Arc::from("object_encoding"), Arc::from(enc.as_str())));
        }
        if let Some(mem) = memory_bytes {
            extra.push((
                Arc::from("memory_bytes_estimate"),
                Arc::from(mem.to_string()),
            ));
        }

        Ok(ObjectDetail {
            node: ObjectNode {
                path: ObjectPath::new(vec![Arc::from(key)]),
                kind: ObjectKind::Key,
                has_children: false,
                comment: None,
            },
            schema: None, // SCHEMA_DECLARED is false — no declared schema to report
            extra,
        })
    }
}

#[async_trait]
impl Catalog for RedisCatalog {
    fn levels(&self) -> Vec<LevelDef> {
        vec![
            LevelDef {
                name: Arc::from("db-index"),
                kind: ObjectKind::Database,
                enumeration: Enumeration::Cheap,
            },
            LevelDef {
                name: Arc::from("keyspace-prefix"),
                kind: ObjectKind::Other,
                enumeration: Enumeration::ScanOnly {
                    requires_prefix: true,
                },
            },
            LevelDef {
                name: Arc::from("key"),
                kind: ObjectKind::Key,
                enumeration: Enumeration::ScanOnly {
                    requires_prefix: true,
                },
            },
        ]
    }

    async fn children(
        &self,
        parent: &ObjectPath,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        match parent.parts() {
            [] => self.list_db_indexes(opts).await,
            [db] => self.list_prefixes(db, opts).await,
            [db, prefix] => self.list_keys(db, prefix, opts).await,
            _ => Err(DbError::Unsupported {
                feature: "catalog path deeper than the key level (3 levels: db-index, \
                          keyspace-prefix, key)"
                    .into(),
            }),
        }
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        match path.parts() {
            [db, _prefix, key] => {
                let _ = db; // Op::Scan-style SELECT-across-db is out of scope for v1 (see connection.rs)
                self.describe_key(key).await
            }
            [db] => Ok(ObjectDetail {
                node: ObjectNode {
                    path: path.clone(),
                    kind: ObjectKind::Database,
                    has_children: true,
                    comment: None,
                },
                schema: None,
                extra: vec![(Arc::from("db_index"), db.clone())],
            }),
            [db, prefix] => Ok(ObjectDetail {
                node: ObjectNode {
                    path: path.clone(),
                    kind: ObjectKind::Other,
                    has_children: true,
                    comment: None,
                },
                schema: None,
                extra: vec![
                    (Arc::from("db_index"), db.clone()),
                    (Arc::from("prefix"), prefix.clone()),
                ],
            }),
            _ => Err(DbError::Unsupported {
                feature: "describe() needs a db-index[/prefix[/key]] path".into(),
            }),
        }
    }

    async fn infer_shape(
        &self,
        _path: &ObjectPath,
        _sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        Ok(InferredSchema {
            sampled: 0,
            root: Vec::new(),
        })
    }

    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        let prefix = prefix_at_caret(&ctx.text, ctx.offset as usize);
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let mut mgr = self.manager.clone();
        let (_next, keys) = scan_once(&mut mgr, None, Some(&prefix_glob(&prefix)), 100).await?;
        Ok(keys
            .into_iter()
            .take(50)
            .map(|k| Completion {
                label: Arc::from(k),
                kind: ObjectKind::Key,
                detail: None,
            })
            .collect())
    }
}

fn missing_prefix_error() -> DbError {
    DbError::Unsupported {
        feature: "listing this level requires an explicit prefix (Enumeration::ScanOnly{\
                  requires_prefix: true}) — pass ListOpts::prefix, even Some(\"\")"
            .into(),
    }
}

async fn scan_once(
    manager: &mut redis::aio::ConnectionManager,
    resume: Option<&ResumeToken>,
    match_glob: Option<&str>,
    count: u32,
) -> Result<(Option<ResumeToken>, Vec<String>), DbError> {
    let cursor = resume
        .and_then(|t| std::str::from_utf8(&t.0).ok())
        .unwrap_or("0")
        .to_string();
    let mut cmd = redis::cmd("SCAN");
    cmd.arg(&cursor);
    if let Some(glob) = match_glob {
        cmd.arg("MATCH").arg(glob);
    }
    cmd.arg("COUNT").arg(count.max(1));
    let reply: redis::Value = cmd.query_async(manager).await.map_err(map_redis_error)?;
    let redis::Value::Array(mut top) = reply else {
        return Err(DbError::Protocol(
            "expected SCAN's [cursor, items] array".into(),
        ));
    };
    if top.len() != 2 {
        return Err(DbError::Protocol("expected a 2-element SCAN reply".into()));
    }
    let items = top.pop().expect("len checked above");
    let cursor_v = top.pop().expect("len checked above");
    let next_cursor = match cursor_v {
        redis::Value::BulkString(b) => {
            String::from_utf8(b).map_err(|e| DbError::Protocol(e.to_string()))?
        }
        redis::Value::SimpleString(s) => s,
        redis::Value::Int(i) => i.to_string(),
        other => {
            return Err(DbError::Protocol(format!(
                "unexpected SCAN cursor: {other:?}"
            )))
        }
    };
    let redis::Value::Array(items) = items else {
        return Err(DbError::Protocol("expected SCAN items array".into()));
    };
    let keys: Vec<String> = items
        .into_iter()
        .map(|v| match v {
            redis::Value::BulkString(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
            redis::Value::SimpleString(s) => Ok(s),
            other => Err(DbError::Protocol(format!(
                "SCAN returned a non-string key: {other:?}"
            ))),
        })
        .collect::<Result<_, DbError>>()?;
    let next = if next_cursor == "0" {
        None
    } else {
        Some(ResumeToken(Bytes::from(next_cursor.into_bytes())))
    };
    Ok((next, keys))
}

async fn databases_count(manager: &mut redis::aio::ConnectionManager) -> u32 {
    let reply: Result<redis::Value, _> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("databases")
        .query_async(manager)
        .await;
    let Ok(reply) = reply else {
        return 16;
    };
    let raw = match reply {
        redis::Value::Map(pairs) => pairs
            .into_iter()
            .find_map(|(k, v)| (config_key_matches(&k, "databases")).then_some(v)),
        redis::Value::Array(items) => {
            let mut it = items.into_iter();
            let mut found = None;
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                if config_key_matches(&k, "databases") {
                    found = Some(v);
                    break;
                }
            }
            found
        }
        _ => None,
    };
    raw.and_then(|v| match v {
        redis::Value::BulkString(b) => std::str::from_utf8(&b).ok()?.parse().ok(),
        redis::Value::SimpleString(s) => s.parse().ok(),
        redis::Value::Int(i) => Some(i as u32),
        _ => None,
    })
    .unwrap_or(16)
}

fn config_key_matches(v: &redis::Value, expect: &str) -> bool {
    match v {
        redis::Value::BulkString(b) => b == expect.as_bytes(),
        redis::Value::SimpleString(s) => s == expect,
        _ => false,
    }
}

fn prefix_at_caret(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut start = end;
    while start > 0
        && matches!(bytes[start - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b':')
    {
        start -= 1;
    }
    text[start..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_enumeration_flips_off_past_the_threshold() {
        assert!(key_enumeration_from_dbsize(0));
        assert!(key_enumeration_from_dbsize(100_000));
        assert!(!key_enumeration_from_dbsize(100_001));
        assert!(!key_enumeration_from_dbsize(50_000_000));
    }

    #[test]
    fn prefix_at_caret_includes_colons() {
        assert_eq!(prefix_at_caret("SCAN 0 MATCH user:12", 21), "user:12");
        assert_eq!(prefix_at_caret("", 0), "");
    }

    #[test]
    fn key_describe_reports_an_honest_empty_index_array() {
        assert_eq!(EMPTY_INDEXES_JSON, "[]");
    }

    #[test]
    fn config_key_matches_bulk_and_simple_strings() {
        assert!(config_key_matches(
            &redis::Value::BulkString(b"databases".to_vec()),
            "databases"
        ));
        assert!(config_key_matches(
            &redis::Value::SimpleString("databases".into()),
            "databases"
        ));
        assert!(!config_key_matches(&redis::Value::Int(1), "databases"));
    }
}
