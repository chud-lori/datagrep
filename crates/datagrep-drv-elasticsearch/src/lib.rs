//! `datagrep-drv-elasticsearch` — the Elasticsearch / OpenSearch driver behind
//! `datagrep-api`'s `Driver` seam (design §3.1, §3.2, §3.3, §5.1).
//!
//! # Why this driver is hand-rolled on `reqwest`
//!
//! The official `elasticsearch` crate has, in roughly six years, published
//! only alpha releases (`9.1.0-alpha.1` at the time of writing), and — the
//! disqualifying part — **it has no streaming whatsoever**: every response
//! body is read into memory in full before the caller sees a byte, and its
//! streaming-helper issue has been open since 2020. Design §3.2's entire
//! memory contract rests on the driver *not* doing that: `next_batch` is
//! pull-only so that when nobody pulls, the socket stops being read, the TCP
//! window closes, and the server stops producing. A buffering client cannot
//! participate in that at all.
//!
//! The `opensearch` crate is not a substitute. OpenSearch forked at
//! Elasticsearch 7.10.2 and its index format, security API and query syntax
//! have diverged since; using it would mean guessing which dialect a cluster
//! speaks. Instead this driver detects the product from `GET /`
//! (`version.distribution`), records it in `ServerInfo`, and **degrades
//! features rather than guessing** — see [`http::choose_page_mode`] and
//! [`driver::es_capabilities`].
//!
//! So the handful of endpoints actually needed (`_search`, `_pit`,
//! `_search/scroll`, `_async_search`, `_count`, `_tasks`, `_validate/query`,
//! `_cat/indices`, `_mapping`, `_alias`, `_data_stream`, `_stats`) are spoken
//! directly over HTTP with `serde_json`, and the pagination is ours.
//!
//! # Design decisions worth stating up front
//!
//! - **PIT + `search_after` is the default; `scroll` is the honest fallback.**
//!   Elasticsearch 7.12+ gets a point-in-time and the `_shard_doc` tiebreaker.
//!   Everything else — OpenSearch (whose PIT is a different endpoint shape
//!   with no `_shard_doc`) and pre-7.12 Elasticsearch — gets `_scroll`. Which
//!   one ran is reported in `ServerInfo` *and* as a `Notice` on the first
//!   batch, so it is never a mystery. See [`http::PageMode`].
//! - **The server-side context is released on every exit path.** Natural
//!   exhaustion, `Cursor::close`, any error including a cancel, and a `Drop`
//!   backstop all funnel through one idempotent `release_context` — see
//!   [`cursor`]'s module doc.
//! - **`Absent` vs `Null` is preserved by construction.** [`value`] only ever
//!   walks the JSON keys a document actually has, so a field missing from a
//!   hit is never synthesized as anything; `Document::get_path` returns `None`
//!   and the core renders `Value::Absent`. A JSON `null` that really is in
//!   `_source` becomes `Value::Null`.
//! - **Precision is not lost to `f64`.** A field the mapping declares
//!   `scaled_float` becomes `Value::Decimal`; so does a `long`/`unsigned_long`
//!   that arrived in a form `serde_json` could only parse as a float. See
//!   [`value`]'s module doc for the full rule and why a `double` correctly
//!   stays an `f64`.
//! - **Injection is blocked structurally, in this engine's own dialect.**
//!   Elasticsearch leaf queries accept either a bare value or an options
//!   object in the same position, so a caller-supplied document in the value
//!   position would be parsed as query *options* (`boost`, and for `match`
//!   even `query` itself). Every comparison therefore compiles to the explicit
//!   `{"term": {"f": {"value": …}}}` form, and `terms` always emits an array
//!   (an object there is a *terms lookup* against another index). Native-request
//!   parameters are substituted into the **parsed** JSON tree, never spliced
//!   into text. See [`filter`] and [`console`].
//! - **Cancellation is design §3.3's row, literally.** A search is submitted
//!   as `_async_search?wait_for_completion_timeout=0`, every request carries an
//!   `X-Opaque-Id`, and a cancel resolves that tag through `GET /_tasks` and
//!   issues `POST /_tasks/<id>/_cancel` (plus `DELETE /_async_search/<id>`).
//!   `CancelOutcome::ServerCancelled` is returned **only** when the server
//!   acknowledged cancelling something specific; otherwise the answer is
//!   `ClientAbandoned`, which is the truth.
//! - **`hits.total` is a lower bound and the driver says so.**
//!   `EXACT_COUNT_CHEAP` is off, `track_total_hits` is never added to a query
//!   the caller did not ask for, and a capped total raises a `Notice` naming
//!   the bound. `Op::Count { exact: true }` runs `_count` and labels the
//!   result "exact".
//!
//! # Known `datagrep-api` gaps found while implementing this driver
//!
//! 1. **`Shape::Documents { root_hint }` cannot say "these envelope fields are
//!    also columns".** Hits are `{_index, _id, _score, _source}`; pointing
//!    `root_hint` at `_source` is right for the grid, but it leaves no way to
//!    offer `_id` as a pinned column alongside the document's own fields.
//!    `SchemaDelta::AddColumn` names are therefore relative to the hinted
//!    root, and the pseudo-fields are reachable only through the detail pane.
//! 2. **`Op::Count` has no way to request "count exactly, but stop after N".**
//!    Elasticsearch's `track_total_hits` takes a *number* — "count up to
//!    100 000, then say ≥" — which is the genuinely useful middle setting and
//!    the boolean `exact` flag cannot express it. This driver therefore never
//!    sets `track_total_hits`, per the ticket.
//! 3. **`Notice` is only reachable through a `Batch`.** Compile-time facts a
//!    caller should see immediately (a filter path whose array index had to be
//!    dropped; `is null` compiling to "not exists" because Elasticsearch does
//!    not index nulls) can only be delivered once the first batch arrives, and
//!    an `Op::Scan` that matches nothing therefore delivers none of them.
//! 4. **`ExecOpts` has no per-request "this is a browse, prefer a fresh view"
//!    flag,** so the keep-alive of the point-in-time is a driver constant
//!    rather than something the caller can tune per query.
//! 5. **No `LanguageId` implementation exists for `EsDsl`.** `datagrep-lang`
//!    ships SQL, MongoShell and RedisCli; there is no Kibana-console language,
//!    so this crate accepts console text and bare JSON search bodies itself
//!    ([`console`]) rather than reimplementing a `Language`. Statement
//!    splitting, classification and highlighting for `EsDsl` are consequently
//!    unavailable to the editor.
//!
//! # Deliberately not implemented
//!
//! - **Writes.** `Caps::EDITABLE_RESULTS` and `Caps::DDL` are off and
//!   `Op::Mutate`/`Op::Ddl` are refused. Hits carry a real `_index`/`_id`
//!   identity so this is a scope decision rather than an engine limitation,
//!   but a half-built write path with no transaction to roll back into is
//!   worse than an honest capability flag.
//! - **Date and geo-point mapping.** An Elasticsearch `date` arrives as either
//!   epoch millis or one of many configurable formats; parsing those without a
//!   date library would mean guessing at a `Value::Timestamp`, and design risk
//!   #4 is explicit that a wrong instant is worse than a crash. Dates stay
//!   `Value::I64`/`Value::Str` exactly as the server sent them, and
//!   `native_type` on the schema delta says `date` so the UI can format them.
//!   `geo_point`/`geo_shape` likewise stay structural rather than becoming
//!   `Value::Geo`.
//! - **`GET /<index>/_explain/<id>`** (why one specific document scored what
//!   it did). `Op::Explain` carries a `Request`, not a document id, so there
//!   is nothing to explain against; `_validate/query?explain` and
//!   `profile: true` are used instead.
//! - **Cross-cluster search, sniffing, and multiple seed nodes.** One base URL
//!   per connection.
//!
//! # Deviations
//!
//! - **`children()` pages client-side, by name.** `_cat/indices` has no
//!   server-side paging, so a listing is fetched once and paged here on a
//!   keyset over the name (not an offset), which is why a concurrently created
//!   index cannot make a page skip an entry.
//! - **The driver-injected `sort` array is dropped from each emitted hit.** It
//!   is an artifact of *our* pagination, not of the user's document; it is
//!   preserved losslessly in the `ResumeToken` instead.
//! - **`serde_json`'s `preserve_order` feature is not enabled** (it is a
//!   workspace-wide feature and would change every crate's JSON behaviour), so
//!   object key order within `_source` is `serde_json`'s map order rather than
//!   the wire order. The `Document` type still preserves whatever order it is
//!   given, and the hit envelope's order is preserved exactly.

#![warn(rust_2018_idioms)]

pub mod canceller;
pub mod catalog;
pub mod connection;
pub mod console;
pub mod cursor;
pub mod driver;
pub mod error;
pub mod filter;
pub mod http;
pub mod json;
pub mod resume;
pub mod value;

pub use driver::{ElasticsearchDriver, DRIVER_ID};
