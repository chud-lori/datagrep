use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value as Json};
use tokio::sync::Mutex;

use datagrep_api::catalog::{
    Catalog, Completion, CompletionCtx, Enumeration, FieldTrie, InferredSchema, LevelDef, ListOpts,
    ObjectDetail, ObjectKind, ObjectNode, Page,
};
use datagrep_api::driver::ResumeToken;
use datagrep_api::error::DbError;
use datagrep_api::shape::{LogicalType, ObjectPath};

use crate::http::{EsHttp, Method};
use crate::json::OrderedJson;
use crate::value::{json_to_value, FieldTypes};

const DEFAULT_SAMPLE_SIZE: u32 = 500;
const MAX_SAMPLE_SIZE: u32 = 10_000;
const COMPLETE_LIMIT: usize = 50;
const PROBLEM_SHARD_CAP: usize = 50;
const CAT_NODES_COLUMNS: &str =
    "name,ip,node.role,master,heap.percent,ram.percent,cpu,load_1m,disk.used_percent,disk.avail,uptime,version";
const CAT_SHARDS_COLUMNS: &str = "index,shard,prirep,state,docs,store,node,unassigned.reason";
const RUNNING_TASK_CAP: usize = 25;
const TEMPLATE_CAP: usize = 50;

pub struct EsCatalog {
    http: Arc<EsHttp>,
    mapping_cache: Arc<Mutex<HashMap<String, Arc<FieldTypes>>>>,
}

impl EsCatalog {
    pub fn new(
        http: Arc<EsHttp>,
        mapping_cache: Arc<Mutex<HashMap<String, Arc<FieldTypes>>>>,
    ) -> Self {
        Self {
            http,
            mapping_cache,
        }
    }

    pub async fn mapping(&self, index: &str) -> Result<Arc<FieldTypes>, DbError> {
        if let Some(hit) = self.mapping_cache.lock().await.get(index) {
            return Ok(hit.clone());
        }
        let json = self
            .http
            .request(
                Method::Get,
                &format!("/{}/_mapping", encode_index_expression(index)?),
                &[],
                None,
                None,
                None,
            )
            .await?;
        let types = Arc::new(merge_mappings(&json));
        self.mapping_cache
            .lock()
            .await
            .insert(index.to_string(), types.clone());
        Ok(types)
    }

    async fn list_indices(&self, opts: &ListOpts) -> Result<Vec<CatIndex>, DbError> {
        let expression = match opts.prefix.as_deref() {
            Some(p) if !p.is_empty() => format!("/{}*", encode_index_expression(p)?),
            _ => String::new(),
        };
        let json = self
            .http
            .request(
                Method::Get,
                &format!("/_cat/indices{expression}"),
                &[
                    ("format", "json".to_string()),
                    (
                        "h",
                        "index,health,status,docs.count,store.size,pri,rep".to_string(),
                    ),
                    ("expand_wildcards", "open".to_string()),
                ],
                None,
                None,
                None,
            )
            .await
            .or_else(not_found_is_empty_array)?;
        Ok(parse_cat_indices(&json))
    }

    async fn list_aliases(&self, opts: &ListOpts) -> Result<Vec<String>, DbError> {
        let expression = match opts.prefix.as_deref() {
            Some(p) if !p.is_empty() => format!("/{}*", encode_index_expression(p)?),
            _ => String::new(),
        };
        let json = self
            .http
            .request(
                Method::Get,
                &format!("/_alias{expression}"),
                &[],
                None,
                None,
                None,
            )
            .await
            .or_else(not_found_is_empty_object)?;
        Ok(parse_aliases(&json))
    }

    async fn list_data_streams(&self, opts: &ListOpts) -> Result<Vec<String>, DbError> {
        let expression = match opts.prefix.as_deref() {
            Some(p) if !p.is_empty() => format!("/{}*", encode_index_expression(p)?),
            _ => String::new(),
        };
        let json = match self
            .http
            .request(
                Method::Get,
                &format!("/_data_stream{expression}"),
                &[],
                None,
                None,
                None,
            )
            .await
        {
            Ok(json) => json,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(json
            .get("data_streams")
            .and_then(Json::as_array)
            .map(|streams| {
                streams
                    .iter()
                    .filter_map(|s| s.get("name").and_then(Json::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn top_level(&self, opts: &ListOpts) -> Result<Page<ObjectNode>, DbError> {
        let indices = self.list_indices(opts).await?;
        let aliases = self.list_aliases(opts).await?;
        let streams = self.list_data_streams(opts).await?;

        let mut nodes: Vec<ObjectNode> = Vec::new();
        for idx in indices {
            nodes.push(ObjectNode {
                path: ObjectPath::new(vec![Arc::from(idx.name.as_str())]),
                kind: ObjectKind::Collection,
                has_children: true,
                comment: Some(Arc::from(
                    format!(
                        "index · {} docs · {} · health {}",
                        idx.docs.as_deref().unwrap_or("?"),
                        idx.store.as_deref().unwrap_or("?"),
                        idx.health.as_deref().unwrap_or("?")
                    )
                    .as_str(),
                )),
            });
        }
        for alias in aliases {
            nodes.push(ObjectNode {
                path: ObjectPath::new(vec![Arc::from(alias.as_str())]),
                kind: ObjectKind::View,
                has_children: true,
                comment: Some(Arc::from("alias")),
            });
        }
        for stream in streams {
            nodes.push(ObjectNode {
                path: ObjectPath::new(vec![Arc::from(stream.as_str())]),
                kind: ObjectKind::Collection,
                has_children: true,
                comment: Some(Arc::from("data stream")),
            });
        }

        Ok(paginate_by_name(nodes, opts))
    }

    async fn list_fields(&self, index: &str, opts: &ListOpts) -> Result<Page<ObjectNode>, DbError> {
        let types = self.mapping(index).await?;
        let mut nodes: Vec<ObjectNode> = types
            .paths()
            .filter(|(path, _, _)| match opts.prefix.as_deref() {
                Some(p) => path.starts_with(p),
                None => true,
            })
            .map(|(path, _, native)| ObjectNode {
                path: ObjectPath::new(vec![Arc::from(index), Arc::from(path)]),
                kind: ObjectKind::Field,
                has_children: false,
                comment: Some(native.clone()),
            })
            .collect();
        nodes.sort_by_key(|a| a.path.to_string());
        Ok(paginate_by_name(nodes, opts))
    }

    async fn index_stats(&self, index: &str) -> Result<Json, DbError> {
        self.http
            .request(
                Method::Get,
                &format!("/{}/_stats/store,docs", encode_index_expression(index)?),
                &[],
                None,
                None,
                None,
            )
            .await
    }

    async fn cluster_health(&self) -> Result<Json, DbError> {
        self.http
            .request(Method::Get, "/_cluster/health", &[], None, None, None)
            .await
    }

    async fn cat_nodes(&self) -> Result<Json, DbError> {
        self.http
            .request(
                Method::Get,
                "/_cat/nodes",
                &cat_unit_params(CAT_NODES_COLUMNS),
                None,
                None,
                None,
            )
            .await
    }

    async fn cat_shards(&self, index: Option<&str>) -> Result<Json, DbError> {
        self.http
            .request(
                Method::Get,
                &cat_shards_path(index)?,
                &cat_unit_params(CAT_SHARDS_COLUMNS),
                None,
                None,
                None,
            )
            .await
    }

    async fn tasks(&self) -> Result<Json, DbError> {
        self.http
            .request(
                Method::Get,
                "/_tasks",
                &[
                    ("detailed", "true".to_string()),
                    ("group_by", "none".to_string()),
                ],
                None,
                None,
                None,
            )
            .await
    }

    async fn templates(&self, endpoint: &str) -> Result<Json, DbError> {
        self.http
            .request(Method::Get, endpoint, &[], None, None, None)
            .await
    }

    async fn describe_root(&self) -> Result<ObjectDetail, DbError> {
        let health = self.cluster_health().await;
        let nodes = self.cat_nodes().await;
        let shards = self.cat_shards(None).await;
        let tasks = self.tasks().await;
        let index_templates = self.templates("/_index_template").await;
        let component_templates = self.templates("/_component_template").await;
        let legacy_templates = self.templates("/_template").await;

        let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        push_cluster_health_extra(&mut extra, &health);
        push_nodes_extra(&mut extra, &nodes);
        push_shards_extra(&mut extra, &shards);
        push_tasks_extra(&mut extra, &tasks);
        push_template_extra(
            &mut extra,
            "index_templates",
            &index_templates,
            parse_index_templates,
        );
        push_template_extra(
            &mut extra,
            "component_templates",
            &component_templates,
            parse_component_templates,
        );
        push_template_extra(
            &mut extra,
            "legacy_templates",
            &legacy_templates,
            parse_legacy_templates,
        );

        Ok(ObjectDetail {
            node: ObjectNode {
                path: ObjectPath::root(),
                kind: ObjectKind::Database,
                has_children: true,
                comment: None,
            },
            schema: None,
            extra,
        })
    }

    async fn sample(&self, index: &str, sample_size: u32) -> Result<InferredSchema, DbError> {
        let size = sample_size.clamp(1, MAX_SAMPLE_SIZE);
        let types = self.mapping(index).await.unwrap_or_default();
        let body = json!({
            "size": size,
            "track_total_hits": false,
            "query": { "function_score": { "query": { "match_all": {} }, "random_score": {} } }
        });
        let (response, _) = self
            .http
            .request_ordered(
                Method::Post,
                &format!("/{}/_search", encode_index_expression(index)?),
                &[],
                Some(&body),
                None,
                None,
            )
            .await?;

        let hits: Vec<OrderedJson> = response
            .get("hits")
            .and_then(|h| h.get("hits"))
            .and_then(OrderedJson::as_array)
            .map(<[OrderedJson]>::to_vec)
            .unwrap_or_default();
        let mut sampled = 0u64;
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        for hit in &hits {
            let Some(source) = hit.get("_source") else {
                continue;
            };
            sampled += 1;
            record_source(&mut root, source, "", &types);
        }
        Ok(InferredSchema { sampled, root })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatIndex {
    pub name: String,
    pub health: Option<String>,
    pub status: Option<String>,
    pub docs: Option<String>,
    pub store: Option<String>,
}

pub fn parse_cat_indices(json: &Json) -> Vec<CatIndex> {
    let Some(rows) = json.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<CatIndex> = rows
        .iter()
        .filter_map(|row| {
            let name = row.get("index").and_then(Json::as_str)?;
            Some(CatIndex {
                name: name.to_string(),
                health: row.get("health").and_then(Json::as_str).map(str::to_string),
                status: row.get("status").and_then(Json::as_str).map(str::to_string),
                docs: row
                    .get("docs.count")
                    .and_then(Json::as_str)
                    .map(str::to_string),
                store: row
                    .get("store.size")
                    .and_then(Json::as_str)
                    .map(str::to_string),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn parse_aliases(json: &Json) -> Vec<String> {
    let Some(map) = json.as_object() else {
        return Vec::new();
    };
    let mut names: Vec<String> = map
        .values()
        .filter_map(|v| v.get("aliases").and_then(Json::as_object))
        .flat_map(|aliases| aliases.keys().cloned())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn cat_unit_params(columns: &str) -> Vec<(&'static str, String)> {
    vec![
        ("format", "json".to_string()),
        ("bytes", "b".to_string()),
        ("time", "ms".to_string()),
        ("h", columns.to_string()),
    ]
}

fn cat_shards_path(index: Option<&str>) -> Result<String, DbError> {
    Ok(match index {
        Some(expr) => format!("/_cat/shards/{}", encode_index_expression(expr)?),
        None => "/_cat/shards".to_string(),
    })
}

fn cat_num(v: Option<&Json>) -> Json {
    let Some(s) = v.and_then(Json::as_str) else {
        // Some servers already emit real numbers under `format=json`.
        return v.cloned().unwrap_or(Json::Null);
    };
    if let Ok(n) = s.parse::<i64>() {
        return json!(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return json!(f);
    }
    json!(s)
}

fn push_pair(extra: &mut Vec<(Arc<str>, Arc<str>)>, k: &str, v: &str) {
    extra.push((Arc::from(k), Arc::from(v)));
}

fn push_cluster_health_extra(
    extra: &mut Vec<(Arc<str>, Arc<str>)>,
    result: &Result<Json, DbError>,
) {
    let health = match result {
        Ok(json) => json,
        Err(e) => {
            push_pair(extra, "cluster_health_error", &e.to_string());
            return;
        }
    };
    for key in ["cluster_name", "status"] {
        if let Some(s) = health.get(key).and_then(Json::as_str) {
            push_pair(extra, key, s);
        }
    }
    for key in [
        "number_of_nodes",
        "number_of_data_nodes",
        "active_primary_shards",
        "active_shards",
        "relocating_shards",
        "initializing_shards",
        "unassigned_shards",
        "delayed_unassigned_shards",
        "number_of_pending_tasks",
        "task_max_waiting_in_queue_millis",
    ] {
        if let Some(n) = health.get(key).and_then(Json::as_i64) {
            push_pair(extra, key, &n.to_string());
        }
    }
    if let Some(n) = health
        .get("active_shards_percent_as_number")
        .and_then(Json::as_f64)
    {
        push_pair(extra, "active_shards_percent_as_number", &format!("{n}"));
    }
}

fn push_nodes_extra(extra: &mut Vec<(Arc<str>, Arc<str>)>, result: &Result<Json, DbError>) {
    match result {
        Ok(json) => push_pair(extra, "nodes", &parse_cat_nodes(json).to_string()),
        Err(e) => push_pair(extra, "nodes_error", &e.to_string()),
    }
}

fn push_shards_extra(extra: &mut Vec<(Arc<str>, Arc<str>)>, result: &Result<Json, DbError>) {
    let json = match result {
        Ok(json) => json,
        Err(e) => {
            push_pair(extra, "shards_error", &e.to_string());
            return;
        }
    };
    let summary = parse_cat_shards(json, PROBLEM_SHARD_CAP);
    push_pair(extra, "shard_count", &summary.total.to_string());
    let mut states = serde_json::Map::new();
    for (state, n) in &summary.by_state {
        states.insert(state.clone(), json!(n));
    }
    push_pair(extra, "shard_states", &Json::Object(states).to_string());
    push_pair(extra, "problem_shards", &summary.problems.to_string());
    if summary.problems_truncated {
        push_pair(
            extra,
            "problem_shards_truncated",
            &format!("listing capped at {PROBLEM_SHARD_CAP}; shard_states has the full counts"),
        );
    }
}

fn push_tasks_extra(extra: &mut Vec<(Arc<str>, Arc<str>)>, result: &Result<Json, DbError>) {
    let json = match result {
        Ok(json) => json,
        Err(e) => {
            push_pair(extra, "tasks_error", &e.to_string());
            return;
        }
    };
    let summary = parse_tasks(json, RUNNING_TASK_CAP);
    push_pair(extra, "task_count", &summary.total.to_string());
    let mut actions = serde_json::Map::new();
    for (action, n) in &summary.by_action {
        actions.insert(action.clone(), json!(n));
    }
    push_pair(extra, "task_actions", &Json::Object(actions).to_string());
    push_pair(extra, "running_tasks", &summary.running.to_string());
    if summary.running_truncated {
        push_pair(
            extra,
            "running_tasks_truncated",
            &format!("listing capped at {RUNNING_TASK_CAP} longest-running; task_actions has the full counts"),
        );
    }
}

fn push_template_extra(
    extra: &mut Vec<(Arc<str>, Arc<str>)>,
    key: &str,
    result: &Result<Json, DbError>,
    parse: fn(&Json, usize) -> TemplateSummary,
) {
    let json = match result {
        Ok(json) => json,
        Err(e) => {
            push_pair(extra, &format!("{key}_error"), &e.to_string());
            return;
        }
    };
    let summary = parse(json, TEMPLATE_CAP);
    push_pair(extra, &format!("{key}_count"), &summary.total.to_string());
    push_pair(extra, key, &summary.listing.to_string());
    if summary.truncated {
        push_pair(
            extra,
            &format!("{key}_truncated"),
            &format!("listing capped at {TEMPLATE_CAP}; {key}_count has the server's full total"),
        );
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TemplateSummary {
    pub total: usize,
    pub listing: Json,
    pub truncated: bool,
}

fn summarise_templates(mut rows: Vec<Json>, cap: usize) -> TemplateSummary {
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let total = rows.len();
    let truncated = total > cap;
    rows.truncate(cap);
    TemplateSummary {
        total,
        listing: Json::Array(rows),
        truncated,
    }
}

fn template_keys(body: Option<&Json>) -> Json {
    let mut keys: Vec<Json> = body
        .and_then(Json::as_object)
        .map(|o| o.keys().map(|k| json!(k)).collect())
        .unwrap_or_default();
    keys.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    Json::Array(keys)
}

pub fn parse_index_templates(json: &Json, cap: usize) -> TemplateSummary {
    let rows = json
        .get("index_templates")
        .and_then(Json::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(Json::as_str)?;
                    let body = entry.get("index_template")?;
                    Some(json!({
                        "name": name,
                        "index_patterns": body.get("index_patterns").cloned(),
                        "priority": body.get("priority").and_then(Json::as_i64),
                        "composed_of": body.get("composed_of").cloned(),
                        "version": body.get("version").and_then(Json::as_i64),
                        "data_stream": body.get("data_stream").is_some(),
                        "template_keys": template_keys(body.get("template")),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    summarise_templates(rows, cap)
}

pub fn parse_component_templates(json: &Json, cap: usize) -> TemplateSummary {
    let rows = json
        .get("component_templates")
        .and_then(Json::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(Json::as_str)?;
                    let body = entry.get("component_template")?;
                    Some(json!({
                        "name": name,
                        "version": body.get("version").and_then(Json::as_i64),
                        "template_keys": template_keys(body.get("template")),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    summarise_templates(rows, cap)
}

pub fn parse_legacy_templates(json: &Json, cap: usize) -> TemplateSummary {
    let rows = json
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(name, body)| {
                    json!({
                        "name": name,
                        "index_patterns": body.get("index_patterns").cloned(),
                        "order": body.get("order").and_then(Json::as_i64),
                        "version": body.get("version").and_then(Json::as_i64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    summarise_templates(rows, cap)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskSummary {
    pub total: usize,
    pub by_action: Vec<(String, usize)>,
    pub running: Json,
    pub running_truncated: bool,
}

pub fn parse_tasks(json: &Json, cap: usize) -> TaskSummary {
    let mut total = 0usize;
    let mut by_action: Vec<(String, usize)> = Vec::new();
    let mut rows: Vec<(i64, Json)> = Vec::new();
    if let Some(tasks) = json.get("tasks").and_then(Json::as_array) {
        for task in tasks {
            let Some(action) = task.get("action").and_then(Json::as_str) else {
                continue;
            };
            total += 1;
            match by_action.iter_mut().find(|(a, _)| a == action) {
                Some((_, n)) => *n += 1,
                None => by_action.push((action.to_string(), 1)),
            }
            let nanos = task
                .get("running_time_in_nanos")
                .and_then(Json::as_i64)
                .unwrap_or(0);
            rows.push((nanos, task_row_json(task, nanos)));
        }
    }
    by_action.sort_by(|a, b| a.0.cmp(&b.0));
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    let running_truncated = rows.len() > cap;
    rows.truncate(cap);
    TaskSummary {
        total,
        by_action,
        running: Json::Array(rows.into_iter().map(|(_, row)| row).collect()),
        running_truncated,
    }
}

fn task_row_json(task: &Json, nanos: i64) -> Json {
    json!({
        "id": task.get("id").and_then(Json::as_i64),
        "node": task.get("node").and_then(Json::as_str),
        "action": task.get("action").and_then(Json::as_str),
        "running_time_ms": nanos / 1_000_000,
        "cancellable": task.get("cancellable").and_then(Json::as_bool),
        "cancelled": task.get("cancelled").and_then(Json::as_bool),
        "description": task.get("description").and_then(Json::as_str),
        "parent_task_id": task.get("parent_task_id").and_then(Json::as_str),
    })
}

pub fn parse_cat_nodes(json: &Json) -> Json {
    let Some(rows) = json.as_array() else {
        return Json::Array(Vec::new());
    };
    let mut out: Vec<Json> = rows
        .iter()
        .filter_map(|row| {
            let name = row.get("name").and_then(Json::as_str)?;
            Some(json!({
                "name": name,
                "ip": row.get("ip").and_then(Json::as_str),
                "roles": row.get("node.role").and_then(Json::as_str),
                "master": row.get("master").and_then(Json::as_str) == Some("*"),
                "heap_percent": cat_num(row.get("heap.percent")),
                "ram_percent": cat_num(row.get("ram.percent")),
                "cpu_percent": cat_num(row.get("cpu")),
                "load_1m": cat_num(row.get("load_1m")),
                "disk_used_percent": cat_num(row.get("disk.used_percent")),
                "disk_avail_bytes": cat_num(row.get("disk.avail")),
                "uptime_ms": cat_num(row.get("uptime")),
                "version": row.get("version").and_then(Json::as_str),
            }))
        })
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json::Array(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShardSummary {
    pub total: usize,
    pub by_state: Vec<(String, usize)>,
    pub problems: Json,
    pub problems_truncated: bool,
}

pub fn parse_cat_shards(json: &Json, problem_cap: usize) -> ShardSummary {
    let mut total = 0usize;
    let mut by_state: Vec<(String, usize)> = Vec::new();
    let mut problems: Vec<Json> = Vec::new();
    let mut truncated = false;
    if let Some(rows) = json.as_array() {
        for row in rows {
            let Some(state) = row.get("state").and_then(Json::as_str) else {
                continue;
            };
            total += 1;
            match by_state.iter_mut().find(|(s, _)| s == state) {
                Some((_, n)) => *n += 1,
                None => by_state.push((state.to_string(), 1)),
            }
            if state != "STARTED" {
                if problems.len() < problem_cap {
                    problems.push(shard_row_json(row));
                } else {
                    truncated = true;
                }
            }
        }
    }
    by_state.sort_by(|a, b| a.0.cmp(&b.0));
    ShardSummary {
        total,
        by_state,
        problems: Json::Array(problems),
        problems_truncated: truncated,
    }
}

pub fn shard_rows_json(json: &Json) -> Json {
    let Some(rows) = json.as_array() else {
        return Json::Array(Vec::new());
    };
    Json::Array(
        rows.iter()
            .filter(|row| row.get("state").and_then(Json::as_str).is_some())
            .map(shard_row_json)
            .collect(),
    )
}

fn shard_row_json(row: &Json) -> Json {
    json!({
        "index": row.get("index").and_then(Json::as_str),
        "shard": cat_num(row.get("shard")),
        "kind": match row.get("prirep").and_then(Json::as_str) {
            Some("p") => Some("primary"),
            Some("r") => Some("replica"),
            other => other,
        },
        "state": row.get("state").and_then(Json::as_str),
        "docs": cat_num(row.get("docs")),
        "store_bytes": cat_num(row.get("store")),
        "node": row.get("node").and_then(Json::as_str),
        "unassigned_reason": row.get("unassigned.reason").and_then(Json::as_str),
    })
}

pub fn merge_mappings(json: &Json) -> FieldTypes {
    let mut merged = FieldTypes::new();
    let Some(map) = json.as_object() else {
        return merged;
    };
    for index in map.values() {
        let Some(props) = index.get("mappings").and_then(|m| m.get("properties")) else {
            continue;
        };
        let types = FieldTypes::from_properties(props);
        for (path, _, native) in types.paths() {
            merged.insert(path, native);
        }
    }
    merged
}

fn record_source(
    root: &mut Vec<(Arc<str>, FieldTrie)>,
    source: &OrderedJson,
    prefix: &str,
    types: &FieldTypes,
) {
    let Some(fields) = source.as_object() else {
        return;
    };
    for (k, v) in fields {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        let idx = match root
            .iter()
            .position(|(name, _)| name.as_ref() == k.as_str())
        {
            Some(i) => i,
            None => {
                root.push((Arc::from(k.as_str()), FieldTrie::default()));
                root.len() - 1
            }
        };
        let logical = json_to_value(v, &path, types)
            .logical_type()
            .unwrap_or(LogicalType::Unknown);
        root[idx].1.record(logical);
        if v.is_object() {
            record_source(&mut root[idx].1.children, v, &path, types);
        }
    }
}

fn paginate_by_name(mut nodes: Vec<ObjectNode>, opts: &ListOpts) -> Page<ObjectNode> {
    nodes.sort_by_key(|a| a.path.to_string());
    nodes.dedup_by(|a, b| a.path == b.path);
    if let Some(after) = opts.resume.as_ref().and_then(decode_name_token) {
        nodes.retain(|n| n.path.to_string() > after);
    }
    let limit = opts.limit.max(1) as usize;
    let truncated = nodes.len() > limit;
    nodes.truncate(limit);
    let next = if truncated {
        nodes
            .last()
            .map(|n| ResumeToken(n.path.to_string().into_bytes().into()))
    } else {
        None
    };
    Page { items: nodes, next }
}

fn decode_name_token(token: &ResumeToken) -> Option<String> {
    String::from_utf8(token.0.to_vec()).ok()
}

pub fn encode_index_expression(expr: &str) -> Result<String, DbError> {
    if expr.is_empty() {
        return Err(DbError::Unsupported {
            feature: "an empty index expression".into(),
        });
    }
    let mut out = String::with_capacity(expr.len());
    for ch in expr.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '+' | '*' | ',' => out.push(ch),
            other => {
                let mut buf = [0u8; 4];
                for b in other.encode_utf8(&mut buf).as_bytes() {
                    out.push('%');
                    out.push_str(&format!("{b:02X}"));
                }
            }
        }
    }
    Ok(out)
}

fn not_found_is_empty_array(err: DbError) -> Result<Json, DbError> {
    match &err {
        DbError::Query { code, .. } if code.as_deref() == Some("index_not_found_exception") => {
            Ok(Json::Array(Vec::new()))
        }
        _ => Err(err),
    }
}

fn not_found_is_empty_object(err: DbError) -> Result<Json, DbError> {
    match &err {
        DbError::Query { code, .. }
            if code.as_deref() == Some("index_not_found_exception")
                || code.as_deref() == Some("aliases_not_found_exception") =>
        {
            Ok(Json::Object(serde_json::Map::new()))
        }
        _ => Err(err),
    }
}

pub fn describe_arrays(types: &FieldTypes) -> (Json, Json) {
    let mut fields: Vec<Json> = types
        .paths()
        .map(|(path, _, native)| json!({ "name": path, "type": native.as_ref() }))
        .collect();
    fields.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let mut indexes: Vec<Json> = vec![json!({
        "name": "_id",
        "kind": "primary",
        "fields": ["_id"],
        "unique": true,
        "primary": true,
        "definition": "document id (Elasticsearch has no user-defined primary key)"
    })];
    let mut searchable: Vec<Json> = types
        .paths()
        // Container types are not themselves searchable.
        .filter(|(_, _, native)| !matches!(native.as_ref(), "object" | "nested"))
        .map(|(path, _, native)| {
            json!({
                "name": path,
                "kind": "inverted",
                "fields": [path],
                "unique": false,
                "primary": false,
                "definition": format!("mapped as {native}")
            })
        })
        .collect();
    searchable.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    indexes.extend(searchable);

    (Json::Array(fields), Json::Array(indexes))
}

fn stat_number(stats: &Json, path: &[&str]) -> Option<i64> {
    let mut cur = stats;
    for seg in path {
        cur = cur.get(seg)?;
    }
    cur.as_i64()
}

#[async_trait]
impl Catalog for EsCatalog {
    fn levels(&self) -> Vec<LevelDef> {
        vec![
            LevelDef {
                name: Arc::from("index"),
                kind: ObjectKind::Collection,
                // One `_cat/indices` call, narrowable by prefix server-side.
                enumeration: Enumeration::Cheap,
            },
            LevelDef {
                name: Arc::from("field"),
                kind: ObjectKind::Field,
                enumeration: Enumeration::Cheap,
            },
        ]
    }

    async fn children(
        &self,
        parent: &ObjectPath,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        match parent.parts() {
            [] => self.top_level(&opts).await,
            [index] => self.list_fields(index, &opts).await,
            _ => Err(DbError::Unsupported {
                feature: "catalog path deeper than index/field".into(),
            }),
        }
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        match path.parts() {
            // The cluster itself: health, nodes, shards.
            [] => self.describe_root().await,
            [index] => {
                let types = self.mapping(index).await?;
                let stats = self.index_stats(index).await.unwrap_or(Json::Null);
                let shards = self.cat_shards(Some(index.as_ref())).await;
                let (fields, indexes) = describe_arrays(&types);

                let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
                let mut push =
                    |k: &str, v: String| extra.push((Arc::from(k), Arc::from(v.as_str())));
                push("field_count", types.len().to_string());
                if let Some(n) = stat_number(&stats, &["_all", "primaries", "docs", "count"]) {
                    push("document_count", n.to_string());
                }
                if let Some(n) = stat_number(&stats, &["_all", "primaries", "docs", "deleted"]) {
                    push("documents_deleted", n.to_string());
                }
                if let Some(n) =
                    stat_number(&stats, &["_all", "primaries", "store", "size_in_bytes"])
                {
                    push("store_size_bytes", n.to_string());
                }
                if let Some(n) = stat_number(&stats, &["_all", "total", "store", "size_in_bytes"]) {
                    push("store_size_bytes_with_replicas", n.to_string());
                }
                match &shards {
                    Ok(json) => {
                        let rows = shard_rows_json(json);
                        push(
                            "shard_count",
                            rows.as_array().map_or(0, Vec::len).to_string(),
                        );
                        push("shards", rows.to_string());
                    }
                    Err(e) => push("shards_error", e.to_string()),
                }
                // The honesty label that goes with `SCHEMA_DECLARED = false`.
                push(
                    "schema_source",
                    "_mapping (declared, but dynamic: documents may carry unmapped fields)".into(),
                );
                push("fields", fields.to_string());
                push("indexes", indexes.to_string());

                Ok(ObjectDetail {
                    node: ObjectNode {
                        path: path.clone(),
                        kind: ObjectKind::Collection,
                        has_children: true,
                        comment: None,
                    },
                    schema: None,
                    extra,
                })
            }
            [index, field] => {
                let types = self.mapping(index).await?;
                let native = types.native(field);
                let mut extra: Vec<(Arc<str>, Arc<str>)> = vec![(
                    Arc::from("mapped"),
                    Arc::from(if native.is_some() { "true" } else { "false" }),
                )];
                if let Some(native) = native {
                    extra.push((Arc::from("type"), native));
                } else {
                    extra.push((
                        Arc::from("note"),
                        Arc::from(
                            "not present in this index's mapping; it may still exist in \
                             documents under a dynamic mapping",
                        ),
                    ));
                }
                Ok(ObjectDetail {
                    node: ObjectNode {
                        path: path.clone(),
                        kind: ObjectKind::Field,
                        has_children: false,
                        comment: None,
                    },
                    schema: None,
                    extra,
                })
            }
            _ => Err(DbError::Unsupported {
                feature: "describe() needs the root, an [index], or an [index, field] path".into(),
            }),
        }
    }

    async fn infer_shape(
        &self,
        path: &ObjectPath,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        let [index] = path.parts() else {
            return Err(DbError::Unsupported {
                feature: "infer_shape() needs an [index] path".into(),
            });
        };
        let size = if sample_size == 0 {
            DEFAULT_SAMPLE_SIZE
        } else {
            sample_size
        };
        self.sample(index, size).await
    }

    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        let prefix = prefix_at_caret(&ctx.text, ctx.offset as usize);
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        match ctx.scope.as_ref().map(ObjectPath::parts) {
            // Inside an index: complete field names from that index's mapping.
            Some([index]) => {
                let types = self.mapping(index).await?;
                let mut out: Vec<Completion> = types
                    .paths()
                    .filter(|(path, _, _)| path.starts_with(&prefix))
                    .map(|(path, _, native)| Completion {
                        label: Arc::from(path),
                        kind: ObjectKind::Field,
                        detail: Some(native.clone()),
                    })
                    .collect();
                out.sort_by(|a, b| a.label.cmp(&b.label));
                out.truncate(COMPLETE_LIMIT);
                Ok(out)
            }
            // Otherwise: a bounded, server-side index-name prefix query.
            _ => {
                let indices = self
                    .list_indices(&ListOpts {
                        prefix: Some(Arc::from(prefix.as_str())),
                        limit: COMPLETE_LIMIT as u32,
                        resume: None,
                    })
                    .await?;
                Ok(indices
                    .into_iter()
                    .take(COMPLETE_LIMIT)
                    .map(|idx| Completion {
                        label: Arc::from(idx.name.as_str()),
                        kind: ObjectKind::Collection,
                        detail: idx.docs.map(|d| Arc::from(format!("{d} docs").as_str())),
                    })
                    .collect())
            }
        }
    }
}

fn prefix_at_caret(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut start = end;
    while start > 0
        && matches!(
            bytes[start - 1],
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'*'
        )
    {
        start -= 1;
    }
    std::str::from_utf8(&bytes[start..end])
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordered(text: &str) -> OrderedJson {
        OrderedJson::parse(text).expect(text)
    }

    #[test]
    fn cat_indices_rows_are_parsed_and_sorted() {
        let json = json!([
            { "index": "logs-2026.08", "health": "green", "status": "open",
              "docs.count": "100000", "store.size": "12.3mb" },
            { "index": "events", "health": "yellow", "status": "open",
              "docs.count": "7", "store.size": "5.1kb" },
            { "not-an-index": true }
        ]);
        let parsed = parse_cat_indices(&json);
        assert_eq!(parsed.len(), 2, "malformed rows are skipped, not fatal");
        assert_eq!(parsed[0].name, "events");
        assert_eq!(parsed[1].name, "logs-2026.08");
        assert_eq!(parsed[1].docs.as_deref(), Some("100000"));
        assert_eq!(parsed[0].health.as_deref(), Some("yellow"));
    }

    #[test]
    fn aliases_are_collected_across_every_backing_index() {
        let json = json!({
            "logs-000001": { "aliases": { "logs": {}, "logs-write": {} } },
            "logs-000002": { "aliases": { "logs": {} } }
        });
        assert_eq!(parse_aliases(&json), vec!["logs", "logs-write"]);
        assert!(parse_aliases(&json!({})).is_empty());
    }

    #[test]
    fn mappings_from_several_concrete_indices_merge_into_one_field_set() {
        let json = json!({
            "logs-000001": { "mappings": { "properties": {
                "ts": { "type": "date" }, "msg": { "type": "text" } } } },
            "logs-000002": { "mappings": { "properties": {
                "ts": { "type": "date" }, "level": { "type": "keyword" } } } }
        });
        let merged = merge_mappings(&json);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged.native("level").as_deref(), Some("keyword"));
        assert_eq!(merged.native("msg").as_deref(), Some("text"));
        assert!(merge_mappings(&json!(null)).is_empty());
    }

    #[test]
    fn index_expressions_cannot_escape_their_path_segment() {
        assert_eq!(
            encode_index_expression("logs-2026.08").unwrap(),
            "logs-2026.08"
        );
        assert_eq!(encode_index_expression("logs*").unwrap(), "logs*");
        assert_eq!(encode_index_expression("a,b").unwrap(), "a,b");
        assert_eq!(
            encode_index_expression("../_cluster/settings").unwrap(),
            "..%2F_cluster%2Fsettings",
            "a traversal attempt must stay inside one path segment"
        );
        assert_eq!(
            encode_index_expression("x/_delete_by_query").unwrap(),
            "x%2F_delete_by_query"
        );
        assert_eq!(encode_index_expression("a b").unwrap(), "a%20b");
        assert_eq!(encode_index_expression("a?q=1").unwrap(), "a%3Fq%3D1");
        assert!(encode_index_expression("").is_err());
    }

    fn node(name: &str) -> ObjectNode {
        ObjectNode {
            path: ObjectPath::new(vec![Arc::from(name)]),
            kind: ObjectKind::Collection,
            has_children: true,
            comment: None,
        }
    }

    #[test]
    fn listings_page_by_name_so_a_new_index_cannot_skip_an_entry() {
        let nodes: Vec<ObjectNode> = ["e", "a", "c", "b", "d"].iter().map(|n| node(n)).collect();
        let opts = ListOpts {
            prefix: None,
            limit: 2,
            resume: None,
        };
        let page = paginate_by_name(nodes.clone(), &opts);
        assert_eq!(
            page.items
                .iter()
                .map(|n| n.path.to_string())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let token = page.next.expect("more pages");
        assert_eq!(decode_name_token(&token).as_deref(), Some("b"));

        let page2 = paginate_by_name(
            nodes.clone(),
            &ListOpts {
                resume: Some(token),
                ..opts.clone()
            },
        );
        assert_eq!(
            page2
                .items
                .iter()
                .map(|n| n.path.to_string())
                .collect::<Vec<_>>(),
            vec!["c", "d"]
        );

        // The final page reports no continuation.
        let last = paginate_by_name(nodes, &ListOpts { limit: 100, ..opts });
        assert_eq!(last.items.len(), 5);
        assert!(last.next.is_none());
    }

    #[test]
    fn listings_deduplicate_an_alias_that_shares_a_name_with_nothing_else() {
        let nodes = vec![node("a"), node("a"), node("b")];
        let page = paginate_by_name(
            nodes,
            &ListOpts {
                prefix: None,
                limit: 10,
                resume: None,
            },
        );
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn describe_arrays_report_fields_and_a_sparse_but_true_indexes_array() {
        let types = FieldTypes::from_properties(&json!({
            "ts": { "type": "date" },
            "addr": { "properties": { "city": { "type": "keyword" } } }
        }));
        let (fields, indexes) = describe_arrays(&types);
        let names: Vec<&str> = fields
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["addr", "addr.city", "ts"]);

        let idx = indexes.as_array().unwrap();
        assert_eq!(idx[0]["name"], json!("_id"));
        assert_eq!(idx[0]["primary"], json!(true));
        assert_eq!(idx[0]["unique"], json!(true));
        // The `object` container is not itself searchable and is excluded.
        let idx_names: Vec<&str> = idx.iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(idx_names, vec!["_id", "addr.city", "ts"]);
        assert_eq!(idx[1]["kind"], json!("inverted"));
        assert!(idx[1]["definition"].as_str().unwrap().contains("keyword"));
    }

    #[test]
    fn field_trie_inference_keeps_heterogeneous_types_and_true_presence() {
        let types = FieldTypes::from_properties(&json!({ "age": { "type": "long" } }));
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        record_source(&mut root, &ordered(r#"{"name":"a","age":30}"#), "", &types);
        record_source(&mut root, &ordered(r#"{"name":"b"}"#), "", &types);
        record_source(
            &mut root,
            &ordered(r#"{"name":"c","age":"thirty"}"#),
            "",
            &types,
        );

        let name = &root.iter().find(|(n, _)| n.as_ref() == "name").unwrap().1;
        let age = &root.iter().find(|(n, _)| n.as_ref() == "age").unwrap().1;
        assert_eq!(name.present, 3);
        assert_eq!(age.present, 2, "absent in doc 2 — absence is not a type");
        assert!((age.presence_ratio(3) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(
            age.types,
            vec![(LogicalType::I64, 1), (LogicalType::Str, 1)],
            "a heterogeneous field stays visible, not coerced to a majority type"
        );
    }

    #[test]
    fn field_trie_inference_recurses_one_level_into_objects() {
        let types = FieldTypes::default();
        let mut root: Vec<(Arc<str>, FieldTrie)> = Vec::new();
        record_source(
            &mut root,
            &ordered(r#"{"address":{"city":"sg","zip":"000000"}}"#),
            "",
            &types,
        );
        let addr = &root
            .iter()
            .find(|(n, _)| n.as_ref() == "address")
            .unwrap()
            .1;
        assert_eq!(addr.types, vec![(LogicalType::Document, 1)]);
        assert_eq!(addr.children.len(), 2);
        assert_eq!(&*addr.children[0].0, "city");
    }

    #[test]
    fn a_missing_prefix_match_is_an_empty_listing_not_an_error() {
        let err = DbError::Query {
            code: Some("index_not_found_exception".into()),
            message: "no such index".into(),
            position: None,
        };
        assert_eq!(not_found_is_empty_array(err).unwrap(), json!([]));

        let other = DbError::Query {
            code: Some("security_exception".into()),
            message: "denied".into(),
            position: None,
        };
        assert!(not_found_is_empty_array(other).is_err());
    }

    #[test]
    fn cat_calls_always_request_machine_readable_units() {
        for columns in [CAT_NODES_COLUMNS, CAT_SHARDS_COLUMNS] {
            let params = cat_unit_params(columns);
            let get = |k: &str| {
                params
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(get("format"), Some("json"));
            assert_eq!(get("bytes"), Some("b"));
            assert_eq!(get("time"), Some("ms"));
            assert_eq!(
                get("h"),
                Some(columns),
                "only the columns actually surfaced are requested"
            );
            assert!(!columns.contains(' '));
        }
    }

    #[test]
    fn cat_shards_path_is_cluster_wide_or_one_encoded_expression() {
        assert_eq!(cat_shards_path(None).unwrap(), "/_cat/shards");
        assert_eq!(
            cat_shards_path(Some("logs-*")).unwrap(),
            "/_cat/shards/logs-*"
        );
        assert_eq!(
            cat_shards_path(Some("../_cluster/settings")).unwrap(),
            "/_cat/shards/..%2F_cluster%2Fsettings",
            "an index name must not be able to retarget the request"
        );
        assert!(cat_shards_path(Some("")).is_err());
    }

    #[test]
    fn cluster_health_keeps_the_servers_own_field_names() {
        let health = json!({
            "cluster_name": "prod-search",
            "status": "yellow",
            "timed_out": false,
            "number_of_nodes": 3,
            "number_of_data_nodes": 2,
            "active_primary_shards": 10,
            "active_shards": 19,
            "relocating_shards": 0,
            "initializing_shards": 1,
            "unassigned_shards": 2,
            "delayed_unassigned_shards": 0,
            "number_of_pending_tasks": 0,
            "number_of_in_flight_fetch": 0,
            "task_max_waiting_in_queue_millis": 0,
            "active_shards_percent_as_number": 86.4
        });
        let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        push_cluster_health_extra(&mut extra, &Ok(health));
        let get = |k: &str| {
            extra
                .iter()
                .find(|(key, _)| &**key == k)
                .map(|(_, v)| v.to_string())
        };
        assert_eq!(get("cluster_name").as_deref(), Some("prod-search"));
        assert_eq!(get("status").as_deref(), Some("yellow"));
        assert_eq!(get("number_of_nodes").as_deref(), Some("3"));
        assert_eq!(get("unassigned_shards").as_deref(), Some("2"));
        assert_eq!(
            get("task_max_waiting_in_queue_millis").as_deref(),
            Some("0"),
            "the unit stays in the name the server chose"
        );
        assert_eq!(
            get("active_shards_percent_as_number").as_deref(),
            Some("86.4")
        );
        assert_eq!(get("timed_out"), None, "unlisted fields are not invented");
    }

    #[test]
    fn cat_nodes_rows_become_unit_labelled_json() {
        let json = json!([
            { "name": "node-2", "ip": "10.0.0.2", "node.role": "cdfhilmrstw", "master": "-",
              "heap.percent": "43", "ram.percent": "91", "cpu": "7", "load_1m": "0.52",
              "disk.used_percent": "61.5", "disk.avail": "52613349376",
              "uptime": "864000000", "version": "8.15.0" },
            { "name": "node-1", "master": "*" },
            { "not-a-node": true }
        ]);
        let nodes = parse_cat_nodes(&json);
        let rows = nodes.as_array().unwrap();
        assert_eq!(rows.len(), 2, "a row without a name is skipped, not fatal");
        assert_eq!(rows[0]["name"], json!("node-1"), "sorted by name");
        assert_eq!(rows[0]["master"], json!(true), "\"*\" means elected master");
        assert_eq!(rows[1]["master"], json!(false));
        assert_eq!(rows[1]["disk_avail_bytes"], json!(52_613_349_376_i64));
        assert_eq!(rows[1]["uptime_ms"], json!(864_000_000_i64));
        assert_eq!(rows[1]["load_1m"], json!(0.52));
        assert_eq!(rows[1]["heap_percent"], json!(43));
        assert_eq!(
            rows[0]["ip"],
            Json::Null,
            "absent columns are null, never invented"
        );
        assert!(parse_cat_nodes(&json!({})).as_array().unwrap().is_empty());
    }

    #[test]
    fn cat_shards_summarise_by_state_and_cap_the_problem_list() {
        let json = json!([
            { "index": "logs", "shard": "0", "prirep": "p", "state": "STARTED",
              "docs": "100", "store": "12345", "node": "node-1" },
            { "index": "logs", "shard": "0", "prirep": "r", "state": "UNASSIGNED",
              "unassigned.reason": "NODE_LEFT" },
            { "index": "logs", "shard": "1", "prirep": "p", "state": "RELOCATING",
              "node": "node-2" },
            { "malformed": true }
        ]);
        let summary = parse_cat_shards(&json, 1);
        assert_eq!(
            summary.total, 3,
            "a row without a state is skipped, not fatal"
        );
        assert_eq!(
            summary.by_state,
            vec![
                ("RELOCATING".to_string(), 1),
                ("STARTED".to_string(), 1),
                ("UNASSIGNED".to_string(), 1)
            ]
        );
        let problems = summary.problems.as_array().unwrap();
        assert_eq!(problems.len(), 1, "the problem list is capped");
        assert!(summary.problems_truncated);
        assert_eq!(problems[0]["state"], json!("UNASSIGNED"));
        assert_eq!(problems[0]["kind"], json!("replica"));
        assert_eq!(problems[0]["unassigned_reason"], json!("NODE_LEFT"));

        let uncapped = parse_cat_shards(&json, 50);
        assert!(!uncapped.problems_truncated);
        assert_eq!(uncapped.problems.as_array().unwrap().len(), 2);

        let empty = parse_cat_shards(&json!(null), 50);
        assert_eq!(empty.total, 0);
        assert!(empty.by_state.is_empty());
    }

    #[test]
    fn one_indexs_shards_are_reported_row_by_row_with_byte_stores() {
        let json = json!([
            { "index": "logs", "shard": "0", "prirep": "p", "state": "STARTED",
              "docs": "100", "store": "12345", "node": "node-1" },
            { "index": "logs", "shard": "0", "prirep": "r", "state": "UNASSIGNED",
              "unassigned.reason": "INDEX_CREATED" }
        ]);
        let rows_json = shard_rows_json(&json);
        let rows = rows_json.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["kind"], json!("primary"));
        assert_eq!(
            rows[0]["store_bytes"],
            json!(12345),
            "bytes=b, so a byte count"
        );
        assert_eq!(rows[0]["shard"], json!(0));
        assert_eq!(rows[1]["node"], Json::Null);
        assert_eq!(rows[1]["unassigned_reason"], json!("INDEX_CREATED"));
        assert!(shard_rows_json(&json!(null)).as_array().unwrap().is_empty());
    }

    #[test]
    fn health_sources_degrade_independently() {
        let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        push_cluster_health_extra(
            &mut extra,
            &Ok(json!({ "status": "green", "number_of_nodes": 1 })),
        );
        push_nodes_extra(
            &mut extra,
            &Err(DbError::Auth(
                "action [cluster:monitor/nodes] is unauthorized".into(),
            )),
        );
        push_shards_extra(
            &mut extra,
            &Ok(json!([{ "index": "a", "shard": "0", "prirep": "p", "state": "STARTED" }])),
        );

        let get = |k: &str| {
            extra
                .iter()
                .find(|(key, _)| &**key == k)
                .map(|(_, v)| v.to_string())
        };
        // The healthy sources are populated…
        assert_eq!(get("status").as_deref(), Some("green"));
        assert_eq!(get("shard_count").as_deref(), Some("1"));
        // …the failed one is marked, not silently absent…
        assert!(get("nodes_error").unwrap().contains("unauthorized"));
        // …and contributes no fabricated data.
        assert_eq!(get("nodes"), None);
    }

    #[test]
    fn even_every_source_failing_yields_marked_rows_not_an_error() {
        let err = || DbError::Auth("denied".into());
        let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        push_cluster_health_extra(&mut extra, &Err(err()));
        push_nodes_extra(&mut extra, &Err(err()));
        push_shards_extra(&mut extra, &Err(err()));
        push_tasks_extra(&mut extra, &Err(err()));
        let keys: Vec<&str> = extra.iter().map(|(k, _)| &**k).collect();
        assert_eq!(
            keys,
            vec![
                "cluster_health_error",
                "nodes_error",
                "shards_error",
                "tasks_error"
            ]
        );
    }

    #[test]
    fn shard_states_extra_reports_counts_and_problems() {
        let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        push_shards_extra(
            &mut extra,
            &Ok(json!([
                { "index": "a", "shard": "0", "prirep": "p", "state": "STARTED" },
                { "index": "a", "shard": "0", "prirep": "r", "state": "UNASSIGNED",
                  "unassigned.reason": "NODE_LEFT" }
            ])),
        );
        let get = |k: &str| {
            extra
                .iter()
                .find(|(key, _)| &**key == k)
                .map(|(_, v)| v.to_string())
        };
        assert_eq!(get("shard_count").as_deref(), Some("2"));
        let states: Json = serde_json::from_str(&get("shard_states").unwrap()).unwrap();
        assert_eq!(states, json!({ "STARTED": 1, "UNASSIGNED": 1 }));
        let problems: Json = serde_json::from_str(&get("problem_shards").unwrap()).unwrap();
        assert_eq!(problems.as_array().unwrap().len(), 1);
        assert_eq!(
            get("problem_shards_truncated"),
            None,
            "no truncation note under the cap"
        );
    }

    #[test]
    fn tasks_summarise_by_action_and_cap_the_longest_running_first() {
        let json = json!({ "tasks": [
            { "node": "nodeA", "id": 1, "action": "indices:data/read/search",
              "running_time_in_nanos": 3_000_000_000_i64,
              "cancellable": true, "cancelled": false, "description": "indices[logs]" },
            { "node": "nodeA", "id": 2, "action": "indices:data/write/reindex",
              "running_time_in_nanos": 900_000_000_000_i64,
              "cancellable": true, "cancelled": false,
              "description": "reindex from [old] to [new]" },
            { "node": "nodeB", "id": 3, "action": "indices:data/read/search",
              "running_time_in_nanos": 1_000_000_i64, "cancellable": true },
            { "node": "nodeB", "id": 4, "action": "cluster:monitor/tasks/lists",
              "running_time_in_nanos": 500_000_i64, "cancellable": false },
            { "malformed": true }
        ]});

        let summary = parse_tasks(&json, 2);
        assert_eq!(
            summary.total, 4,
            "a task without an action is skipped, not fatal — and the listing \
             task itself is counted, because the server counts it"
        );
        assert_eq!(
            summary.by_action,
            vec![
                ("cluster:monitor/tasks/lists".to_string(), 1),
                ("indices:data/read/search".to_string(), 2),
                ("indices:data/write/reindex".to_string(), 1),
            ],
            "actions keep the cluster's own spelling"
        );
        let running = summary.running.as_array().unwrap();
        assert_eq!(running.len(), 2, "the listing is capped");
        assert!(summary.running_truncated);
        // Longest-running first: the reindex is the one worth seeing.
        assert_eq!(running[0]["action"], json!("indices:data/write/reindex"));
        assert_eq!(running[0]["running_time_ms"], json!(900_000));
        assert_eq!(running[0]["node"], json!("nodeA"));
        assert_eq!(running[1]["running_time_ms"], json!(3_000));
        // A fact, reported; nothing here offers to act on it.
        assert_eq!(running[0]["cancellable"], json!(true));
        assert_eq!(running[1]["description"], json!("indices[logs]"));

        let uncapped = parse_tasks(&json, 25);
        assert!(!uncapped.running_truncated);
        assert_eq!(uncapped.running.as_array().unwrap().len(), 4);

        let empty = parse_tasks(&json!({ "tasks": [] }), 25);
        assert_eq!(empty.total, 0);
        assert!(empty.by_action.is_empty());
        assert_eq!(parse_tasks(&json!(null), 25).total, 0);
    }

    #[test]
    fn task_extra_reports_counts_and_the_running_list() {
        let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        push_tasks_extra(
            &mut extra,
            &Ok(json!({ "tasks": [
                { "node": "n", "id": 1, "action": "indices:data/write/reindex",
                  "running_time_in_nanos": 2_000_000_000_i64 },
                { "node": "n", "id": 2, "action": "cluster:monitor/tasks/lists",
                  "running_time_in_nanos": 100_000_i64 }
            ]})),
        );
        let get = |k: &str| {
            extra
                .iter()
                .find(|(key, _)| &**key == k)
                .map(|(_, v)| v.to_string())
        };
        assert_eq!(get("task_count").as_deref(), Some("2"));
        let actions: Json = serde_json::from_str(&get("task_actions").unwrap()).unwrap();
        assert_eq!(
            actions,
            json!({ "cluster:monitor/tasks/lists": 1, "indices:data/write/reindex": 1 })
        );
        let running: Json = serde_json::from_str(&get("running_tasks").unwrap()).unwrap();
        assert_eq!(running.as_array().unwrap().len(), 2);
        assert_eq!(running[0]["running_time_ms"], json!(2_000));
        assert_eq!(
            get("running_tasks_truncated"),
            None,
            "no truncation note under the cap"
        );
    }

    #[test]
    fn index_templates_are_listed_by_name_with_what_they_define() {
        let json = json!({ "index_templates": [
            { "name": "logs", "index_template": {
                "index_patterns": ["logs-*"], "priority": 200,
                "composed_of": ["log-mappings"], "version": 3,
                "template": { "mappings": { "properties": {} }, "settings": {} } } },
            { "name": "events", "index_template": {
                "index_patterns": ["events-*"], "priority": 100,
                "composed_of": [], "data_stream": {},
                "template": { "aliases": {} } } },
            { "name": "bare", "index_template": { "index_patterns": ["b-*"] } },
            { "index_template": { "index_patterns": ["nameless-*"] } }
        ]});

        let summary = parse_index_templates(&json, 50);
        assert_eq!(
            summary.total, 3,
            "an entry without a name is skipped, not fatal"
        );
        assert!(!summary.truncated);
        let rows = summary.listing.as_array().unwrap();
        assert_eq!(rows[0]["name"], json!("bare"), "sorted by name");
        assert_eq!(rows[1]["name"], json!("events"));
        assert_eq!(rows[2]["name"], json!("logs"));
        assert_eq!(rows[2]["index_patterns"], json!(["logs-*"]));
        assert_eq!(rows[2]["priority"], json!(200));
        assert_eq!(rows[2]["composed_of"], json!(["log-mappings"]));
        assert_eq!(rows[2]["template_keys"], json!(["mappings", "settings"]));
        assert_eq!(rows[1]["data_stream"], json!(true));
        assert_eq!(rows[2]["data_stream"], json!(false));
        // A template that sets nothing is still a template.
        assert_eq!(rows[0]["template_keys"], json!([]));
        assert_eq!(rows[0]["priority"], json!(null));

        let capped = parse_index_templates(&json, 2);
        assert!(capped.truncated);
        assert_eq!(capped.listing.as_array().unwrap().len(), 2);
        assert_eq!(capped.total, 3, "the count stays the server's total");

        assert_eq!(parse_index_templates(&json!(null), 50).total, 0);
    }

    #[test]
    fn component_and_legacy_templates_keep_their_own_spellings() {
        let components = parse_component_templates(
            &json!({ "component_templates": [
                { "name": "log-mappings", "component_template": {
                    "version": 2, "template": { "mappings": { "properties": {} } } } },
                { "name": "shard-count", "component_template": {
                    "template": { "settings": {} } } }
            ]}),
            50,
        );
        assert_eq!(components.total, 2);
        let rows = components.listing.as_array().unwrap();
        assert_eq!(rows[0]["name"], json!("log-mappings"));
        assert_eq!(rows[0]["version"], json!(2));
        assert_eq!(rows[0]["template_keys"], json!(["mappings"]));
        assert_eq!(rows[1]["template_keys"], json!(["settings"]));

        let legacy = parse_legacy_templates(
            &json!({
                "old-logs": { "index_patterns": ["logs-*"], "order": 10, "version": 1 },
                "old-events": { "index_patterns": ["events-*"] }
            }),
            50,
        );
        assert_eq!(legacy.total, 2);
        let rows = legacy.listing.as_array().unwrap();
        assert_eq!(rows[0]["name"], json!("old-events"));
        assert_eq!(rows[0]["order"], json!(null));
        assert_eq!(rows[1]["name"], json!("old-logs"));
        assert_eq!(rows[1]["order"], json!(10));
        assert_eq!(rows[1]["index_patterns"], json!(["logs-*"]));

        assert_eq!(parse_legacy_templates(&json!([]), 50).total, 0);
    }

    #[test]
    fn a_failing_template_endpoint_marks_only_its_own_source() {
        let mut extra: Vec<(Arc<str>, Arc<str>)> = Vec::new();
        push_template_extra(
            &mut extra,
            "index_templates",
            &Ok(json!({ "index_templates": [
                { "name": "logs", "index_template": { "index_patterns": ["logs-*"] } }
            ]})),
            parse_index_templates,
        );
        push_template_extra(
            &mut extra,
            "component_templates",
            &Err(DbError::Unsupported {
                feature: "forbidden".into(),
            }),
            parse_component_templates,
        );
        let get = |k: &str| {
            extra
                .iter()
                .find(|(key, _)| &**key == k)
                .map(|(_, v)| v.to_string())
        };
        assert_eq!(get("index_templates_count").as_deref(), Some("1"));
        let listing: Json = serde_json::from_str(&get("index_templates").unwrap()).unwrap();
        assert_eq!(listing[0]["name"], json!("logs"));
        assert_eq!(get("index_templates_truncated"), None);
        // The forbidden source is named, and it does not take the other down.
        assert!(get("component_templates_error")
            .expect("the failed source is marked")
            .contains("forbidden"));
        assert_eq!(get("component_templates"), None);
    }

    #[test]
    fn prefix_at_caret_understands_index_and_field_characters() {
        assert_eq!(prefix_at_caret("GET /logs-2026", 14), "logs-2026");
        let text = "{\"query\":{\"term\":{\"addr.ci";
        assert_eq!(prefix_at_caret(text, text.len()), "addr.ci");
        assert_eq!(prefix_at_caret("", 0), "");
        assert_eq!(prefix_at_caret("x ", 2), "");
    }

    #[test]
    fn levels_are_two_deep_and_both_bounded() {
        let http = Arc::new(
            EsHttp::new(
                "http://127.0.0.1:9200".into(),
                crate::http::Auth::None,
                std::time::Duration::from_secs(1),
                false,
            )
            .unwrap(),
        );
        let catalog = EsCatalog::new(http, Arc::new(Mutex::new(HashMap::new())));
        let levels = catalog.levels();
        assert_eq!(levels.len(), 2);
        assert_eq!(&*levels[0].name, "index");
        assert_eq!(&*levels[1].name, "field");
        assert!(levels.iter().all(|l| l.enumeration == Enumeration::Cheap));
    }
}
