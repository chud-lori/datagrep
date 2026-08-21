# Elasticsearch: closing the read-only gap

**Status:** research + plan. No code written. Prepared 2026-08-08.
**Question this answers:** *"why elasticsearch querying only? u might want to check elasticvue for reference"*

Everything about datagrep below was verified by reading the code at
`crates/datagrep-drv-elasticsearch/`, `crates/datagrep-api/`, `crates/datagrep-lang/`
and `crates/datagrep-core/`. Everything about Elasticvue was verified against its
own source on GitHub (`cars10/elasticvue`, 2 714 stars, last pushed 2026-06-07),
not its marketing page. Anything I could not verify is labelled **unverified**.

Jump to: [0. Answer](#0-the-short-answer) · [1. Current state](#1-current-state-verified)
· [2. What good ES clients do](#2-what-a-good-es-client-actually-does)
· [3. Writes without a transaction](#3-what-es-writes-should-look-like-in-datagrep)
· [4. Ranked plan](#4-ranked-plan) · [5. datagrep-api changes](#5-what-datagrep-api-needs)
· [6. EsDsl](#6-the-esdsl-language-gap) · [7. Copy / don't copy](#7-copy-outright--deliberately-dont)
· [8. Out of scope](#8-out-of-scope-stated-plainly)

---

## 0. The short answer

The honest answer to "why querying only" is **not** "Elasticsearch can't be
written to safely" — it can, and better than Postgres can, because
`if_seq_no`/`if_primary_term` give a real per-document compare-and-swap that a
generated SQL `UPDATE … WHERE id = ?` does not have. The real reasons are three,
and only one of them is about Elasticsearch:

1. **`datagrep-core` has no write-staging at all.** `CoreApi` exposes
   `run_query`, `run_export`, `get_rows`, `cancel`, `close_query`,
   `list_catalog` — nothing about pending edits. `Caps::EDITABLE_RESULTS` is
   read in exactly one place in the whole workspace: `datagrep-cli`'s `doctor`
   command (`crates/datagrep-cli/src/cmd/doctor.rs:73`). The
   "edit → review pending diff → commit" model is a design-document model with
   **no implementation anywhere**, for any driver. So there is no existing edit
   flow to wire Elasticsearch into. This is the single biggest caveat in this
   plan and it is not an ES problem.
2. **The api can't express an ES write's precondition.** `Mutation::Update`
   carries `key: Vec<(FieldPath, Value)>`, which expresses `_index` + `_id`
   (+ `_routing`) perfectly well — but `_seq_no`/`_primary_term` are *not*
   identity, they are a precondition that changes on every write. There is
   nowhere to put them.
3. **`LanguageId::EsDsl` resolves to `FallbackLanguage`**
   (`crates/datagrep-lang/src/lib.rs:178`), so the editor cannot split, classify
   or highlight anything an ES user types.

**Recommendation:** ship the language module and single-document guarded
update/delete. Do not chase Elasticvue's breadth — index admin, snapshots,
mapping edits and reindex are cluster administration, and Elasticvue itself does
the two most important ones (document editing, deep paging) in ways we should
deliberately not copy. Elasticvue's own 223-respondent survey ranks the REST
console #1 by a 1.6× margin and document CRUD #2, with ILM/ingest/shard
relocation at 1–3 votes each — independent confirmation of exactly this
ordering (§2.4).

**Two things to weigh before committing.** (1) There is a real counter-argument
from Elasticvue's maintainer that manual document editing is a rarer need than
people claim, and that most "editing" requests are really requests for a
formatted JSON *view* (§2.4). That is an argument for stopping P0-3 at
single-document update/delete, which is what this plan does. (2) **On ES ≥ 9.4,
optimistic concurrency is off by default for time-series indices** and lost
updates there are *silent* rather than a 409 (§3.2.3) — the write path must
detect index mode and disable editing, not discover this in production.

**Worth stating loudly, because it surprised me:** the driver is *not*
read-only today. `EsConnection::execute_native` refuses writes only when
`read_only_active(opts)` is true (`connection.rs:258–260`), and
`ExecOpts::read_only_assert` defaults to `false`. A user can type
`PUT /my-index/_doc/1` + body into the query editor right now and it executes.
What is missing is *generated* writes, a truthful capability flag, and
statement classification — not the ability to write.

---

## 1. Current state (verified)

`crates/datagrep-drv-elasticsearch/` — 8 694 LOC across 13 modules, 1 077 LOC of
integration tests. Read `src/lib.rs`'s crate doc first; it is accurate.

| Fact | Where |
|---|---|
| `EDITABLE_RESULTS`, `DDL`, `TRANSACTIONS` all off | `driver.rs:49–57`, asserted at `driver.rs:585–603` |
| `Op::Mutate` / `Op::Ddl` refused with a stated reason | `connection.rs:628–641` |
| Native console writes **do** execute unless read-only is on | `connection.rs:258–260`, `301–316` |
| Read-only is `Enforcement::Client`, allow-list shaped | `connection.rs:57–72`, `561–585` |
| Hits carry `_index`/`_id`/`_score`/`_source`; `root_hint` points at `_source` | `cursor.rs:159–165`, `693–706` |
| `_seq_no`/`_primary_term` are **not** requested — no `seq_no_primary_term` in any search body | grepped `cursor.rs`, `connection.rs` |
| 409s already surface with `version_conflict_engine_exception` as `DbError::Query::code` | `error.rs:70–110` |
| Dates stay `I64`/`Str` with `native_type: "date"` | `lib.rs:108–115` |
| `console::strip_comment_lines` drops only whole-line `#` comments | `console.rs:183–188` |
| Catalog pages client-side; one base URL; no sniffing | `lib.rs:120–121`, `lib.rs:123–128`, `catalog.rs` |

Precedents in sibling drivers, which matter for the design below:

- **MySQL** wraps a `MutationBatch` in an explicit transaction and rolls back on
  any mutation that does not affect exactly one row
  (`crates/datagrep-drv-mysql/src/connection.rs:213–275`).
- **Mongo's connection-level `execute_mutate` does not** — it loops, one
  `update_one` at a time, with no transaction
  (`crates/datagrep-drv-mongo/src/connection.rs:712–780`). A partial batch is
  therefore *already possible* in datagrep today. Elasticsearch would not be
  breaking new ground, only being louder about it.
- **Redis** uses `MULTI`/`EXEC` so the batch is atomic "where the engine allows"
  (`crates/datagrep-drv-redis/src/connection.rs:23–27`).

Two landmines in the existing value conversion that a write path would step on
immediately:

- `value_to_json(Value::Absent) → Json::Null` (`value.rs:266`). In an
  `_update` partial document a JSON null **sets the field to null**; it does not
  remove it. "Clear this cell" is therefore ambiguous and must not be resolved
  by accident.
- **`map_status_error` will mis-label timeouts on ES 9.** It maps `408|504 →
  Timeout` and `429 → ResourceExhausted` (`error.rs:98–109`). ES 9.0's breaking
  changes moved server-side timeouts from 5xx to **429**
  ([#116026](https://github.com/elastic/elasticsearch/pull/116026)), so on a 9.x
  cluster a timeout now surfaces as "resource exhausted". Small, real,
  independent of everything else in this plan.
- **Error bodies on ES ≥ 8.18 can contain the whole document.**
  `?include_source_on_error` was added for create/index/update/bulk and
  **defaults to `true`**
  ([#120725](https://github.com/elastic/elasticsearch/pull/120725)). `error.rs`
  goes to real trouble not to leak credentials into logs; once we generate
  writes, a parse-failure body may carry the user's document. Set
  `include_source_on_error=false` on generated writes, or bound what is logged.
- `field_path_to_es` drops `PathSeg::Index` (`filter.rs:68–84`). Combined with
  the fact that `{"doc": …}` **replaces arrays wholesale** rather than merging
  them, editing one element of an array field is inexpressible. It must be
  refused, not approximated.

---

## 2. What a good ES client actually does

### 2.1 Elasticvue — verified against its source, not its docs

Its README claims six things: *"Cluster overview"*, *"Index & alias
management"*, *"Shard management"*, *"Searching and editing documents"*,
*"Rest queries"*, *"Snapshot & repository management"*
([README](https://raw.githubusercontent.com/cars10/elasticvue/master/README.md)).
The precise surface is one file —
[`src/services/ElasticsearchAdapter.ts`](https://github.com/cars10/elasticvue/blob/master/src/services/ElasticsearchAdapter.ts):

| Area | Endpoints it calls |
|---|---|
| Cluster | `_cluster/health`, `_cluster/stats`, `_cluster/settings` (GET+PUT), `_cluster/reroute` |
| Nodes / shards | `_cat/nodes`, `_nodes`, `_cat/shards`, `_cat/recovery`, `_recovery` |
| Indices | `_cat/indices`, `PUT /<index>`, `DELETE`, `_refresh`, `_flush`, `_cache/clear`, `_forcemerge`, `_close`, `_open`, `_settings` (GET+PUT), `_stats`, `_clone`, `_reindex?wait_for_completion=false` |
| Aliases | `PUT /<index>/_alias/<alias>`, `DELETE /<index>/_alias/<alias>` |
| Templates | `GET _template`, `GET _index_template` (read only) |
| Documents | `PUT /<index>/<type>/<id>?refresh=true`, `GET`, `DELETE …?refresh=true`, `POST _bulk?refresh=true`, `_delete_by_query?refresh=true` |
| Search | `POST /<index>/_search` |
| Snapshots | `_snapshot` repo CRUD, snapshot create/delete/restore, `_slm/policy` CRUD + `_execute` |

Four things stand out, all verified:

1. **There is no `_mapping` write anywhere in the adapter.** Mappings are read
   via `GET /<index>`. Elasticvue does not offer mapping editing — the leading
   ES GUI reached the same conclusion §8 reaches.
2. **`if_seq_no` and `seq_no_primary_term` appear zero times in the entire
   repository** (`gh search code "if_seq_no repo:cars10/elasticvue"` → no
   results; same for `seq_no_primary_term`; control searches for `_doc` and
   `_bulk` return hits, so the search works). `_update` appears only as the
   Tauri auto-updater plugin. **Every document save is a blind full-document
   `PUT` overwrite with no concurrency guard** — last write wins, silently.
   The sharpest version of this: `EditDocument.ts` *reads* `_seq_no` and
   `_primary_term` off the document and renders them beside the editor as
   display-only metadata, then issues the `PUT` **without them**. The values are
   in hand and unused. The edit UI is a modal with a raw JSON editor of
   `_source` and one "Update" button, and `updateDocument()` carries **no
   confirmation step** — the click fires the write. No diff, no conflict
   handling.

   Confirmation coverage across the app is inconsistent in a way worth learning
   from: index delete, delete-by-query, close (single row), alias delete and
   snapshot *delete* all confirm; **forcemerge, reindex, clone, alias add,
   snapshot *restore* and document update do not**, and bulk close/open skip the
   confirmation that the single-row version has.
3. **Every write hardcodes `?refresh=true`** — `index()`, `delete()`,
   `docsBulkDelete()`, `deleteByQuery()`. That forces a segment refresh on the
   shard on every single save.
4. **Search paginates with `from`+`size`**: `DEFAULT_SEARCH_QUERY_OBJ =
   { query: { query_string: { query: '*' } }, size: 10, from: 0, sort: [] }`
   (`src/consts.ts:31`), and `max_result_window` appears nowhere in the repo —
   so deep paging simply 400s past 10 000.

Genuinely good ideas it has: named multi-cluster switching; a REST console with
**tabs, saved queries, history capped at 1 000 entries, curated examples, and
auto-parsing of pasted Kibana-console syntax into method/path/body**
(`RestQueryForm.ts`); and chunking multi-index operations at
`MAX_INDICES_PER_REQUEST = 16` so it never builds a 4 KB URL.

Three more absences worth recording, because they bound what "feature parity"
would even mean: **`QueryBuilder.vue` is a stub containing literally
`<h1 />`** — the visual query builder was never built; **index templates are
read-only** (no create/edit/delete); and **create-index takes name + shards +
replicas only**, no mappings or settings JSON. There is also **no read-only
mode, by stated policy** — the FAQ answers the request flatly: *"No. Users will
always be able to change things by using the REST API."* datagrep already does
better here (`Enforcement::Client` plus the allow-list classifier at
`connection.rs:561–585`), and should say so.

**Scale is its weak point, and we partly share the weakness.** Indices and
shards are fetched whole and filtered/paginated **client-side** — the structural
cause of the large-cluster sluggishness in its tracker (#354: 18 071 shards made
*Test connection* fail outright). Our own `_cat/indices` listing pages
client-side too (`lib.rs:123–128`, stated as a deviation). Theirs is worse
because it is also unbounded, but this is not a stone we can throw hard.

### 2.2 Kibana Dev Tools Console — the console reference

The [Console docs](https://www.elastic.co/docs/explore-analyze/query-filter/tools/console)
give the grammar a real lexer has to handle: multiple requests written
sequentially in one editor and *"executes the requests one by one"*; comments as
*"double forward slashes or pound signs"* for single-line and `/* … */` for
multi-line; `${variableName}` substitution with `"""${var}"""` triple quotes to
*"enforce string substitution"*; context-sensitive autocomplete; and a `kbn:`
prefix for Kibana APIs. Triple-quoted strings matter: a `"""…"""` block
containing a `}` breaks any naive brace counter.

### 2.3 Contrast

- **Dejavu** (`appbaseio/dejavu`, 8 468 stars): a spreadsheet-style data browser
  with in-place cell editing and CSV/JSON/NDJSON import, rather than an admin
  console. Its `pushed_at` of 2026-07-02 is dependabot noise; real feature work
  is bursty (3.10.0 in 2025-09, 3.8.3 in 2024-09). **Its full-document Update
  button has been broken on ES 8 since 2022**
  ([#450](https://github.com/appbaseio/dejavu/issues/450)): it `GET`s the
  document *including metadata fields* and `PUT`s it back verbatim, so ES 8
  rejects it with `Field [_ignored] is a metadata field and cannot be added
  inside a document`. A read-modify-write with no metadata stripping and no
  optimistic concurrency — the same class of mistake as Elasticvue's, unfixed
  for three years. Direct lesson for §3.3: **write back only `_source`, never
  the envelope.**
- **Cerebro** (`lmenezes/cerebro`, 5 613 stars): **last substantive commit
  2021-07-03** — the 2024 `pushed_at` is a dependabot branch. Five years
  dormant, 207 open issues, AngularJS 1.8 (itself EOL), and it now fails to
  start on modern JDKs
  ([#600](https://github.com/lmenezes/cerebro/issues/600)). "Is this project
  alive?" ([#591](https://github.com/lmenezes/cerebro/issues/591)) is open. ES 8
  support ([#567](https://github.com/lmenezes/cerebro/issues/567)) has been
  open and unassigned since 2022. **Not a live reference in 2026**; its detailed
  feature list is **unverified** and was not worth chasing.

### 2.4 What users actually want — and the counter-argument

**The best prioritisation evidence found anywhere in this research** is
Elasticvue's own 2021 feature survey,
[#55](https://github.com/cars10/elasticvue/issues/55), 223 respondents: the
**REST console is the #1 ask (23 votes), by a 1.6× margin over second place**;
**document CRUD is #2 (14 votes)**; ILM, ingest pipelines and shard relocation
drew **1–3 votes each**. That is independent confirmation of this plan's
ranking: console/language work first (P0-2), document editing second (P0-3),
cluster administration last or never (§8). Most respondents also run Kibana —
these tools are a complement to it, not a replacement.

**The honest counter-argument, from the maintainer himself**
([#225](https://github.com/cars10/elasticvue/issues/225)): *"why is editing such
a big usecase for you? I have never worked with an elasticsearch deployment
where i would manually edit documents"* — and per that thread most people asking
for "editing" actually wanted a **formatted JSON view**, not an editor. Worth
sitting with before spending 9–12 days. It does not change the ranking (the
console work is P0 on its own merits and needs no editing story), but it is a
real argument for scoping P0-3 to single-document update/delete and stopping
there, rather than pushing on to bulk and insert.

From `github.com/cars10/elasticvue/issues` sorted by comments:

- **[#270](https://github.com/cars10/elasticvue/issues/270)** — "Add Syntax
  Highlighting and Validation for Elasticsearch REST DSL Requests". *Even the
  leading ES GUI does not have what §6 proposes.* This is the clearest signal in
  the whole research that the language module is worth building.
- **[#257](https://github.com/cars10/elasticvue/issues/257)** — "Docker: make API
  calls on the server". The CORS tax: Elasticvue needs
  `http.cors.enabled: true` on the cluster unless you use the desktop app or a
  browser extension. **Structurally absent for datagrep** — a native app talking
  to ES over HTTP has no CORS. Worth saying out loud; it is a real advantage.
- **[#321](https://github.com/cars10/elasticvue/issues/321)**,
  **[#323](https://github.com/cars10/elasticvue/issues/323)**,
  **[#358](https://github.com/cars10/elasticvue/issues/358)** — shards/nodes/
  indices views degrade or go blank on closed indices and when an alias call
  fails. Admin screens are where the bugs are. #358's cause is visible in the
  source and is a **direct lesson for P1-4**: `ClusterIndices.ts` does
  `Promise.all([catIndices, indexGetAlias('*')])`, so one failing alias call
  takes out the entire index listing. Health/metadata enrichment must degrade
  per-source, never all-or-nothing — our catalog already has this instinct
  (`not_found_is_empty_array` at `catalog.rs:118–120`) and should keep it.
- **[#332](https://github.com/cars10/elasticvue/issues/332)** — a *default*
  changed in v1.11.0 to `"fields": ["*","*.*"]`, spiked node CPU and, in a
  user's words, *"seriously affect[ed] elasticsearch cluster stability"*;
  reverted in 1.11.1. A cautionary tale about what a browse tool's defaults cost
  on someone else's production cluster.
- **[#338](https://github.com/cars10/elasticvue/issues/338)** — navigating away
  aborts in-flight requests, cancelling long reindexes midway. Another argument
  for keeping reindex out (§8).
- **[#316](https://github.com/cars10/elasticvue/issues/316)** — "Prompt for
  username/password". Credential handling; datagrep already solves this via
  `datagrep-secrets` + the keychain.
- **[#307](https://github.com/cars10/elasticvue/issues/307)** — security user/role
  management. See §8.

**Reddit is genuinely unresearched, not empty** — r/elasticsearch was
unreachable on every attempted path (crawler blocked; `old.reddit.com` 403).
**Hacker News has nothing to cite**: exactly one Elasticvue submission
([39012130](https://news.ycombinator.com/item?id=39012130)), 1 point, 1 comment
— which recommends Cerebro instead. Treat the issue trackers and the survey as
the entire evidence base, and do not repeat the common claims that "Kibana is
too heavy to install just for a console" or that people object to Elastic's
licensing: **no primary source was found for either.**

---

## 3. What ES writes should look like in datagrep

Elasticsearch has no transactions, so "edit → review pending diff → commit →
roll back on failure" cannot be delivered as written. What follows is what
*can* be delivered honestly, and it is not a downgrade in every dimension.

### 3.1 The one thing ES gives us that SQL does not

`if_seq_no` / `if_primary_term` are a per-document compare-and-swap. Every
indexing operation returns `_seq_no` and `_primary_term`; setting
`seq_no_primary_term: true` on a search returns them per hit; passing them back
as `if_seq_no`/`if_primary_term` means the write *only happens if nobody else
touched the document since you loaded it*
([optimistic concurrency control](https://www.elastic.co/docs/reference/elasticsearch/rest-apis/optimistic-concurrency-control)).
A mismatch is an HTTP **409** with
`"type": "version_conflict_engine_exception"` and a reason of the form
`[<id>]: version conflict, required seqNo [N], primary term [T]. current
document has seqNo [M] and primary term [T]`.

A generated Postgres `UPDATE … WHERE id = $1` has no equivalent — it clobbers
whatever is there. So the honest framing is:

> **datagrep's Elasticsearch writes are safer per document than its SQL writes,
> and weaker across documents.** We trade cross-document atomicity for
> per-document non-clobbering. That trade should be stated in the UI, not
> hidden.

### 3.2 The commit model: guarded, serial, halt-and-report

1. **The scan must ask for the guard.** Add `seq_no_primary_term: true` to the
   search body so every hit carries `_seq_no`/`_primary_term`. Without it there
   is nothing to guard with.
2. **Every generated write is guarded, or refused.** If a document's
   `_seq_no`/`_primary_term` are absent (an aggregation result, a `fields`-only
   projection, a resumed scan whose token predates this change), **refuse the
   write** rather than sending it unguarded. This is the same rule as
   `id_filter`'s "an empty identity is refused, never guessed at".
3. **On ES ≥ 9.4, a returned `_seq_no` may be a lie — and this is the single
   sharpest hazard in the whole plan.** Elasticsearch 9.4 disables sequence
   numbers by default for **time-series-mode indices** (they cost up to 30 % of
   storage for OTLP metrics). The
   [9.x breaking changes](https://www.elastic.co/docs/release-notes/elasticsearch/breaking-changes)
   are explicit: *"Index, update and delete operations using `if_seq_no` error
   out, and search calls with `seq_no_primary_term` set return **sentinel
   values** for sequence numbers"*, and by-query operations *"proceed without
   conflict detection, so concurrent modifications may **silently overwrite**
   the affected docs without triggering version conflict errors"*
   ([#145737](https://github.com/elastic/elasticsearch/pull/145737)). The escape
   hatch is `index.disable_sequence_numbers: false` in the index template.
   > **Rule: detect the index mode before offering an edit.** A TSDB index on
   > 9.4+ must have editing disabled with a truthful reason, not attempted and
   > 400'd — and *never* attempted unguarded, because there the failure is
   > silent. This is a capability question, so it belongs in the per-index
   > catalog detail, not in a driver `if`.
4. **`refresh=wait_for`, not `refresh=true`** — with two gotchas that must be
   handled, not discovered. `wait_for` blocks until the next *scheduled* refresh
   makes the change visible; `true` forces an immediate segment refresh on the
   shard, which the docs say *"should **ONLY** be done after careful thought"*
   because the cost is paid three times (indexing a tiny segment, searching it,
   merging it). Elasticvue does the latter on every write. The gotchas
   ([refresh docs](https://www.elastic.co/guide/en/elasticsearch/reference/current/docs-refresh.html)):
   - if `index.refresh_interval` is `-1`, **`wait_for` waits indefinitely** —
     so the write must carry our own deadline, which `ExecOpts::timeout`
     already gives us;
   - if more than `index.max_refresh_listeners` (default 1000) requests are
     already waiting on a shard, `wait_for` **silently degrades to `true`** and
     the response carries `"forced_refresh": true`. Surface that in a `Notice`
     rather than swallowing it.

   Emit a `Notice` saying which refresh mode actually applied, so "I saved it
   but I don't see it" never becomes a mystery.
4. **The batch is serial and not atomic, and the UI says so *before* the
   click.** The commit confirmation must read, literally: *"4 documents will be
   updated one at a time. Elasticsearch has no transaction — if #3 fails, #1 and
   #2 stay written."* datagrep already has this habit: `CancelOutcome`,
   `Enforcement` and `CancelKind` all exist to name the strength of a guarantee
   rather than imply one.
5. **On failure: halt and report.** Not roll back (impossible), not continue
   (turns one conflict into an unbounded unreviewed partial write). Stop at the
   first failure and return a report naming *applied*, *failed*, and *not
   attempted*. The not-attempted mutations stay pending in the grid so they can
   be retried after a refresh. Halting bounds the damage to a prefix a human can
   read.
6. **A 409 is a UI state, not an error toast.** On
   `version_conflict_engine_exception`: re-`GET` the document and show three
   columns — the value you loaded, the value on the server now, and the value
   you typed. That is exactly the three-way pattern Postico uses for pending DDL
   (`notes/ux-reference-study.md` §2), and it is the right shape here. Offer
   *rebase* (re-apply my field edits onto the current `_seq_no`) or *discard
   mine*. **Never `retry_on_conflict`** — silently retrying is precisely the
   clobber the guard exists to prevent.

### 3.3 Per-operation rules

| Operation | Compiles to | Rule |
|---|---|---|
| Update a field | `POST /<index>/_update/<id>?if_seq_no=N&if_primary_term=T&refresh=wait_for` with `{"doc": {…}}` | Partial merge. Requires `_source` enabled (*"The `_source` field must be enabled to use `update`"*). |
| Delete | `DELETE /<index>/_doc/<id>?if_seq_no=N&if_primary_term=T` | Same guard. |
| Insert with a user-supplied id | `PUT /<index>/_doc/<id>?op_type=create` | 409s instead of silently overwriting. **Never a bare `PUT`** — that is Elasticvue's blind overwrite. |
| Insert with no id | `POST /<index>/_doc` | Server generates the id. |
| Set a field to null | `{"doc": {"f": null}}` | `Value::Null` in `sets`. |
| Remove a field | scripted `ctx._source.remove('f')` | `Value::Absent` in `sets`. **P1** — until then, refuse `Absent` in `sets` rather than let `value_to_json` turn it into a null (`value.rs:266`). **Constraint:** the docs are explicit that *"If both `doc` and `script` are specified, then `doc` is ignored"* — so a mutation that both sets and removes fields must compile to **one script**, not `doc` + `script`. That is what makes P1-3 a day rather than an hour. |
| Edit an array element | — | **Refuse.** `{"doc": …}` replaces arrays wholesale and `field_path_to_es` already drops `PathSeg::Index`. Reuse the existing refusal message from `filter.rs:95–100`. |

**Routing is part of identity.** An index using custom routing needs `routing=`
on every write, or the write lands on the wrong shard (or 400s with
`routing_missing_exception`). `_routing` must ride along in `Mutation::key`
whenever the hit carries it — which means the scan has to stop discarding it.

**Dates round-trip by accident, and that is fine.** Reads keep a `date` as the
`Str`/`I64` the server sent (`lib.rs:108–115`), so writing it back sends the
same token the mapping's own format already accepted. No new date library, no
guessed instant. Do not "fix" this while adding writes.

---

## 4. Ranked plan

Estimates are engineer-days for someone already inside this codebase, and
include unit tests at the density the crate already runs (120 unit tests for
8 694 LOC). They do **not** include UI work — see the caveat in §0.1.

### P0 — the answer to "why querying only", ≈ 9–12 days total

| # | Item | Days | Notes / risk |
|---|---|---|---|
| **P0-1** | Request `seq_no_primary_term: true` on scans; carry `_seq_no`, `_primary_term`, `_routing` through the hit envelope; detect time-series index mode | **0.5–1** | Blocks P0-3. Adds two keys to every emitted hit; unit tests asserting the exact envelope key list (`cursor.rs:1054`) will need updating. The index-mode check (§3.2.3) is the part that is not trivial — on ES 9.4+ a TSDB index returns *sentinel* seq-nos, so "we got a number back" is not proof the guard will work. |
| **P0-2** | `LanguageId::EsDsl` in `datagrep-lang`: split / classify / highlight / context_at; driver adopts it | **3–4** | Fully independent — ships alone and is worth shipping alone. See §6. Also fixes a silent bug: ES connections get no `@limit`/`@timeout`/`@readonly` block directives today. |
| **P0-3** | Guarded single-document `Op::Mutate` (update + delete), `refresh=wait_for`, halt-and-report, `EDITABLE_RESULTS` on | **3–4** driver + **1** live-ES integration tests | Depends on P0-1 and P0-4. The integration suite currently seeds fixtures with a raw `reqwest` client *precisely because* nothing in the write path is vouched for (`tests/README.md:125–131`); that can now go through the seam. |
| **P0-4** | `datagrep-api`: `Shape::Documents.identity`, `Mutation::{Update,Delete}.expect`, `Caps::ATOMIC_BATCH`, `DbError::Conflict` | **1.5–2** | Touches five other drivers. Mostly `expect: vec![]` plus an `Unsupported` guard where a driver can't honour a precondition. See §5. |

### P1 — ≈ 9–11 days

| # | Item | Days | Notes |
|---|---|---|---|
| P1-1 | Insert (`op_type=create` / `POST /_doc`) | 1 | Straightforward once P0-3 lands. |
| P1-2 | Compile a multi-document batch to one `_bulk` NDJSON; parse per-item `status`/`error`; report per item | 2–3 | N round trips → 1, and one refresh-listener slot per shard instead of N. Bulk carries `if_seq_no`/`if_primary_term` on the *action line*, so the guard survives (note `retry_on_conflict` is also an action-line field there, not a query param — and we do not want it, per §3.2.6). Bulk is **not** atomic: HTTP is **200 even when items fail**; the signal is top-level `errors: true` plus per-item `status`, which is exactly the halt-and-report shape. Risks: the body must be `application/x-ndjson`, must **not** be pretty-printed, and must end in `\n` — our `EsHttp` only sends parsed JSON today; the 100 MB `http.max_content_length` ceiling applies; and **ES 9.0 tightened bulk action parsing into a hard error** ([#115923](https://github.com/elastic/elasticsearch/pull/115923)), so previously-tolerated sloppiness now fails. `?filter_path=items.*.error` is a cheap way to keep the response small. |
| P1-3 | Field removal via scripted update (`ctx._source.remove(…)`) | 1 | Makes `Value::Absent` in `sets` mean "remove", closing the `value.rs:266` ambiguity properly. |
| P1-4 | Cluster / node / shard health surfaced through the **existing catalog**, not a new screen: `_cluster/health`, `_cat/nodes`, `_cat/shards` in `ObjectDetail::extra` and a root-level `describe` | 2 | Reuses machinery we already have; no new UI concept. Two constraints: each source must degrade independently (Elasticvue's #358 `Promise.all` failure), and Elastic is blunt that **cat APIs are *"only intended for human consumption… not intended for use by applications"*** — values are human-formatted (`3.5mb`) unless `bytes=`/`time=` are passed. We already depend on `_cat/indices?format=json` in the catalog, so this is a pre-existing bet, not a new one — but pass the unit params. |
| P1-5 | Index create/delete/close/open and alias add/remove as structured `DdlOp` | — | **Delete is structured; create, close/open and alias *add* stay native — and that is the whole honest answer.** `DdlOp` now carries `Drop { path, kind, if_exists }`, `Rename` and `CreateIndex`, designed against Postgres, MySQL, MariaDB, SQLite and MongoDB rather than this engine alone — five engines honour some of it, this one honours `Drop`. The `ObjectKind` is what makes `Drop` work here: an index and an alias share one namespace and the server refuses `DELETE /<alias>` (*"specify the corresponding concrete indices instead"*), so `Collection` becomes `DELETE /<index>` and `View` becomes a `POST /_aliases` remove — one action list, never visible half-removed. `if_exists` rides `ignore_unavailable` for an index; the alias endpoint has no such parameter (`must_exist: false` does **not** suppress the 404), so that one is tolerated by error type. Wildcards, comma lists, `_all` and date-math names are refused before anything is sent rather than trusting `action.destructive_requires_name`, which is a setting. What stays native and why: `Create` needs an authoring type vocabulary the api does not have; `close`/`open` is a lifecycle state no other engine has; and alias **add** carries a `filter` query document plus `routing`, which a structured form here could only drop. `Rename` and `CreateIndex` are refused by name (no rename without a reindex; no named secondary index). `Caps::DDL` is now on. |

### P2 — deferred, not refused

- Task-monitoring surface (`_tasks` polling) — we already have `_tasks` plumbing
  for cancellation, so a "long-running operations" view is not absurd. It is a
  product feature, not a driver change. Reindex (§8) only becomes defensible
  after it exists.
- Index templates — **read done**, in the root-level `describe` alongside cluster health. Not "one `GET`": there are **three** systems on the same cluster — composable `_index_template`, the `_component_template` pieces those are `composed_of`, and legacy `_template`, which still wins when no composable template matches. A stock 8.15 ships 45 / 44 / 5 of them before anyone authors one, so each is counted and its listing capped. The JSON APIs, not `_cat/templates`, which renders `index_patterns` as the string `"[logs-*]"` and does not know about component templates. **Authoring is done, and it stays native** — the same gate that P1-5 resolved resolves this one the same way: a template body *is* mappings and settings, so it is `Request::Native` (`PUT /_index_template/<name>`), not a structured verb, permanently rather than pending. The whole loop is covered against a real cluster: author a component and a composable template through the driver, create a matching index and confirm the cluster applied the composed mapping, read it back through this describe, delete both — and read-only refuses every one of those writes.

---

## 5. What `datagrep-api` needs

Read `crates/datagrep-api/src/request.rs` and `shape.rs`.

**1. `Shape::Documents` cannot declare identity.** Only
`Shape::Table(RowSchema)` carries `Identity` (`shape.rs:41`, `98–101`). A
document-shaped cursor therefore has no way to say "these paths identify a
hit", which would force the grid to know that ES identity is `_index` + `_id`
(+ `_routing`) — exactly the `if driver_id == …` the README bans.

> **Add `identity: Option<Vec<FieldPath>>` to `Shape::Documents`**, as paths
> relative to the *hit*, not to `root_hint`. Mongo needs the same thing for
> `_id`, so this is not an ES special case.

**2. `Mutation::key` expresses identity fine, but not the precondition.**
`_index` / `_id` / `_routing` are named `(FieldPath, Value)` pairs and fit
`key` exactly as designed — the doc's promise that a driver "never has to
reverse-engineer which columns the values belong to" holds. But
`_seq_no`/`_primary_term` are **not identity**: they change on every write, and
a "key" that changes is not a key. Putting them in `key` would also make the
exactly-one-row invariant ambiguous.

> **Add `expect: Vec<(FieldPath, Value)>` to `Mutation::Update` and
> `Mutation::Delete`** — "only apply if these fields still hold these values".
> This is genuinely portable and earns its keep beyond ES: Postgres/MySQL can
> compile it into extra `WHERE` conjuncts (optimistic locking on a `version` or
> `updated_at` column), Mongo into extra filter fields. Drivers that cannot
> honour it reject a non-empty `expect` with `DbError::Unsupported`, which is
> honest and cheap.
>
> Rejected alternative: smuggling the guard through `key` and special-casing it
> in the ES driver. It violates the no-branching rule and lies about what a key
> is.

**3. Nothing says whether a batch is atomic.** `MutationBatch`'s doc says
"applied together — atomically where the engine allows", which the caller
cannot inspect. `Caps::TRANSACTIONS` is about interactive `begin`, not batch
atomicity — Mongo's connection-level `execute_mutate` already applies a batch
non-atomically while other paths do not.

> **Add `Caps::ATOMIC_BATCH`** so the commit dialog can render §3.2's sentence
> without knowing the engine. Off for ES; on for PG/MySQL/SQLite and for Redis
> (`MULTI`/`EXEC`); Mongo needs a considered answer since it differs by path.

**4. `Shape::Ack { affected, message }` cannot report a partial batch.**
Halt-and-report needs per-mutation outcomes.

> **v1: emit one `Value::Document` per mutation through `Shape::Documents`**
> (index, id, outcome, error code, error reason) plus summary `Notice`s. No new
> shape, the grid already renders documents, the CLI already prints them.
> Introduce a real `Shape::MutationReport` only when a second driver needs it.

**5. `DbError` has no `Conflict`.** A 409 lands in
`DbError::Query { code: Some("version_conflict_engine_exception") }` — the ES
driver's status mapping has no branch for it
(`crates/datagrep-drv-elasticsearch/src/error.rs:98–109`), so the core would
have to string-match an engine-specific code to render §3.2's conflict UI — the
banned branch again.

> **Add `DbError::Conflict { code, message }`, recoverable.** Postgres
> serialization failures (`40001`) and Mongo `WriteConflict` map onto it too.

Detection must key off the structured `error.type`, **never the reason string**.
The reason is built by `VersionConflictEngineException` as
`"{documentDescription}: version conflict, required seqNo [N], primary term [T]."`
plus either `" current document has seqNo [M] and primary term [T]"` or
`" but no document was found"` — and `documentDescription` is **not always
`[<id>]`**: for time-series indices it includes the timestamp and dimensions.
Same `type`, different prose. (Helpfully, ES 9.0 also made error JSON uniform —
*"error fields will always have `type` and `reason`"*,
[#90529](https://github.com/elastic/elasticsearch/pull/90529).) Note the
second variant is a distinct UX case: the document was **deleted** underneath
you, so "rebase my edits" is not offered — only "re-insert" or "discard".

**6. The unavoidable caveat.** None of the above gives datagrep a place to
*stage* an edit. `CoreApi` has no pending-edit concept and `EDITABLE_RESULTS` is
consumed nowhere but `doctor`. So either:

- **(a)** the ES write path ships as driver-level `Op::Mutate` support whose
  only callers are the CLI and FFI — small, honest, and a real capability, but
  no grid editing; or
- **(b)** it becomes the forcing function for building result-grid editing
  across all six drivers — large, valuable, and **not an Elasticsearch ticket**.

This plan scopes (a). Say so in the issue.

---

## 6. The EsDsl language gap

`language_for(LanguageId::EsDsl)` returns `FallbackLanguage`
(`crates/datagrep-lang/src/lib.rs:178`): the whole buffer is one statement,
`classify` returns `Unknown`, `highlight` returns nothing. Four consequences,
one of which is a live bug:

1. One request per tab. A pasted Kibana script cannot be split.
2. `StatementClass::Unknown` means the client-side write guardrail cannot tell
   `GET /_search` from `DELETE /my-index`. The driver's `is_read_request`
   already does exactly this job (`connection.rs:561–571`, whole-segment
   matching against `READ_ONLY_POST_ENDPOINTS`) — but it lives below the editor
   and only runs when read-only mode is on.
3. No syntax highlighting.
4. **Live bug:** block directives (`@limit`, `@timeout`, `@connection`,
   `@readonly`) are parsed by `Language::split` from the comment lines above a
   statement. The fallback returns `Ok(Directives::default())`, so **ES
   connections silently get no block directives** while every other engine does.
   Nobody would notice until they relied on `-- @limit`.

**Second live bug, cheap to fix:** `console::strip_comment_lines`
(`console.rs:183–188`) drops only whole lines beginning with `#`. Kibana also
supports `//` and `/* … */`. A snippet pasted from Kibana using `//` comments
fails today with "request body is not valid JSON".

### What the module has to handle

**We do not have to guess at the grammar.** Kibana's Console parser is a
hand-written recursive descent parser and its **test file is the de-facto
spec** —
[`parser.ts`](https://github.com/elastic/kibana/blob/main/src/platform/packages/shared/kbn-monaco/src/languages/console/parser.ts)
and
[`parser.test.ts`](https://github.com/elastic/kibana/blob/main/src/platform/packages/shared/kbn-monaco/src/languages/console/parser.test.ts).
Port the rules, and every script Elastic's own docs ship works.

- **`split`** — a request begins at a line whose first token is
  `GET|POST|PUT|DELETE|HEAD|PATCH`; the body runs to the next request line or
  EOF. The rules that matter, all taken from Kibana's parser:
  - **The boundary is the next `METHOD` token, not a blank line.**
    `'GET _search\nPOST _test_index'` is two requests with no blank line
    between them. Method matching is case-insensitive and asserts the next
    character is whitespace, so `GETTER` is not a method.
  - A body is parsed **only** if the next non-space character is `{`.
  - **Several JSON objects in a row** in one body: the parser loops
    `while (ch === '{')` and collects them into an array. `_bulk` and
    `_msearch` are NDJSON, and `_bulk` is the single most-pasted ES request
    there is — a "one JSON object per request" parser gets it wrong.
  - `#`, `//` **and** `/* … */` comments, plus `#!` as a *distinct* token
    (deprecation warnings echoed in responses), not a plain comment.
  - **Triple-quoted strings** `"""…"""`, scanned to the closing delimiter
    rather than brace-counted — an embedded `}` defeats a naive counter.
  - **Error recovery**: on a parse failure, re-anchor on
    `/^\s*(POST|HEAD|GET|PUT|DELETE|PATCH)/im` so one broken body does not
    destroy the rest of the buffer. Worth copying — it is the difference
    between "one bad request" and "the tab is unusable".
  - Kibana also flags **duplicate JSON keys** (`Duplicate key "a"`), which
    `serde_json` silently accepts. Cheap, and a real ES footgun.
- **`classify`** — `Read` for GET/HEAD plus the POST read allow-list; `Write`
  for `_doc`/`_update`/`_bulk`/`_delete_by_query`/`_update_by_query`/`_reindex`;
  `Ddl` for index create/delete/close/open, `_mapping`, `_settings`, `_alias`,
  templates; `Admin` for `_cluster/settings`, `_snapshot`, `_slm`,
  `_nodes/reload_secure_settings`, `_tasks/*/_cancel`.
  **The allow-list should move down from the driver into `datagrep-lang` so
  there is one table.** `datagrep-drv-redis` already sets this precedent — it
  uses `datagrep_lang::redis` for splitting rather than reimplementing it
  (`crates/datagrep-drv-redis/src/connection.rs:4–9`).
- **`highlight`** — method keyword, path, query params, then a JSON lexer
  (string / number / `true|false|null` / punct) plus comments. ~150 LOC.
- **`context_at`** — inside a JSON string, inside a comment, or in an
  identifier. Mechanical once the lexer exists.

**Effort: 3–4 days, ~600–800 LOC plus ~40 unit tests**, benchmarked against
`redis.rs` (573 LOC, one file) and `sql/` (~1 350 LOC across four files). It is
the cheapest visible win in this whole plan, it fixes two real bugs, and after
it lands `console::parse` shrinks to "turn one span `datagrep-lang` already
found into a `ConsoleRequest`".

**Skip for v1: Kibana's `${variable}` substitution.** We already bind typed
`$1` parameters into the *parsed* JSON tree (`console.rs:193–223`), which is
strictly safer than Kibana's textual substitution — a parameter cannot introduce
a key, a clause, or a nesting level. Adding textual variables would be a step
backwards. Also skip `kbn:` (we do not talk to Kibana) and autocomplete (a
separate milestone, and the catalog's `complete` already exists).

---

## 7. Copy outright / deliberately don't

### Copy

- **Saved queries + history in the console.** Elasticvue has both, plus tabs.
  datagrep already stores query history in `datagrep-profiles`; ES just needs to
  participate. Nearly free — and it beats Kibana's own Console, whose scripts
  live only in `localStorage`
  ([#93909](https://github.com/elastic/kibana/issues/93909),
  [#39017](https://github.com/elastic/kibana/issues/39017), open since 2019,
  with users reporting *"I had a browser session crash and I lost all the
  queries"*).
- **Auto-parse pasted Kibana-console text** into method/path/body
  (`RestQueryForm.ts`). Once P0-2 lands this is nearly free for us too, and it
  is the single most common way an ES snippet arrives.
- **Error recovery in the parser** — Kibana re-anchors on the next METHOD after
  a parse failure. Copy it (§6).
- **A starter list of request examples** (`REST_QUERY_EXAMPLES`) — `_cat/indices`,
  `_cat/aliases`, `_cat/shards`, create index, `_bulk`. Trivial, and it genuinely
  helps people who do not remember endpoint names. This is what our
  `console.rs` "expected a Kibana-console request line" error message is
  gesturing at and failing to deliver.
- **Chunking multi-index operations** at a fixed cap
  (`MAX_INDICES_PER_REQUEST = 16`), if we ever do multi-index operations. Good
  instinct: never build a 4 KB URL.
- **Cluster/node health as a first-class read**, but through our catalog rather
  than a bespoke screen (P1-4).

### Deliberately don't

- **`?refresh=true` on every write.** Verified in `index()`, `delete()`,
  `docsBulkDelete()`, `deleteByQuery()`. Forces a shard refresh per save. Use
  `refresh=wait_for` (§3.2.3).
- **Blind full-document `PUT` with no concurrency guard, fired with no
  confirmation.** Verified: zero occurrences of
  `if_seq_no`/`seq_no_primary_term` in the whole repository, and
  `updateDocument()` has no confirm step. This is last-write-wins over a
  colleague's edit — the exact failure datagrep's identity story exists to
  prevent, and the reason §3 is written the way it is. Dejavu makes the same
  mistake one step worse by echoing metadata fields back
  ([#450](https://github.com/appbaseio/dejavu/issues/450), broken since 2022) —
  **write back `_source` only**.
- **Confirmation as an opt-in per call site.** Elasticvue's confirms are a
  `confirm` prop each caller may forget, which is why snapshot *restore* and
  bulk close/open have none while their single-row siblings do. Destructiveness
  should be a property of the operation, decided once.
- **`from`+`size` paging.** `DEFAULT_SEARCH_QUERY_OBJ` ships `from: 0` and the
  repo has no `max_result_window` handling; deep paging 400s. datagrep is
  keyset-only by decision (`RANDOM_ACCESS_PAGE` off, `driver.rs:36–38`).
- **A "delete all documents in this index" button.** Elasticvue's
  `deleteByQuery({index})` hardcodes `{query: {match_all: {}}}`. One click, no
  preview, no undo.
- **Threading a `_type` segment through document paths.** Elasticvue builds
  `${index}/${type}/${id}` and defaults `_type` to `_doc`
  (`src/models/SearchResults.ts`). Mapping types were removed in ES 8. We
  should not carry the concept at all.
- **Kibana's textual `${var}` substitution.** See §6. Its own history is a
  warning: values starting with a digit were emitted unquoted and silently
  parsed as numbers
  ([#145764](https://github.com/elastic/kibana/issues/145764)).
- **Expensive defaults.** Elasticvue shipped `"fields": ["*","*.*"]` as a
  default and destabilised users' clusters (#332). Our equivalent temptation is
  `track_total_hits` — already deliberately refused (`connection.rs:409–411`).
  Keep refusing.
- **A response pane that isn't valid JSON.** Kibana's Console rewrites strings
  into `"""…"""` on output, so you cannot pipe it into `jq`
  ([#15628](https://github.com/elastic/kibana/issues/15628) — "fixed" only by
  adding an opt-out). Our `DocsCursor` returns the reply as a real `Value`;
  keep it that way.

---

## 8. Out of scope, stated plainly

These are decisions, not a backlog.

- **Mapping edits. Out.** Elasticsearch only allows adding fields and changing a
  small set of parameters; anything else requires a reindex. A GUI that offers
  "edit mapping" and then fails on half the edits is worse than one that does
  not offer it. **Elasticvue reached the same conclusion** — no `_mapping` write
  exists in its adapter.
- **Snapshots, repositories and SLM. Out.** Creating a repository requires
  filesystem or object-store configuration on every node (`path.repo`), which a
  data browser cannot do; restoring a snapshot over a live index is a
  cluster-admin action with no undo. The console covers the occasional read.
- **Reindex. Out for now.** It is a long-running task. Elasticvue fires
  `_reindex?wait_for_completion=false` and then offers no task tracking — a
  fire-and-forget button over an operation that can run for hours. Doing it
  properly needs the task-monitoring surface in P2, and that is a product
  feature, not a driver change.
- **Delete-by-query and update-by-query as *generated* operations. Out.**
  Unbounded destructive statements with no preview. They remain available by
  typing them into the console, where the user has authored them explicitly —
  that is the correct amount of friction.
- **Cluster administration: `_cluster/settings` PUT, `_cluster/reroute`,
  `_nodes/reload_secure_settings`, force-merge, cache clear, flush. Out.**
  Elasticvue has all of them. They are not data browsing, and they are also
  where its bug reports cluster (#321, #323, #358).
- **Security / user / role management. Out.** (Elasticvue #307 asks for it.)
- **Cross-cluster search, node sniffing, multiple seed nodes. Out**, unchanged
  from the crate doc. One base URL per connection.
- **Guessing dates into `Value::Timestamp`. Out**, unchanged. A wrong instant is
  worse than a raw value, and the write path in §3.3 depends on the current
  behaviour to round-trip correctly.

---

## Sources

- [Elasticvue README](https://raw.githubusercontent.com/cars10/elasticvue/master/README.md) ·
  [`ElasticsearchAdapter.ts`](https://github.com/cars10/elasticvue/blob/master/src/services/ElasticsearchAdapter.ts) ·
  [`EditDocument.vue`](https://github.com/cars10/elasticvue/blob/master/src/components/search/EditDocument.vue) ·
  [`consts.ts`](https://github.com/cars10/elasticvue/blob/master/src/consts.ts) ·
  [issues](https://github.com/cars10/elasticvue/issues) ([#270](https://github.com/cars10/elasticvue/issues/270), [#257](https://github.com/cars10/elasticvue/issues/257), [#307](https://github.com/cars10/elasticvue/issues/307), [#316](https://github.com/cars10/elasticvue/issues/316), [#321](https://github.com/cars10/elasticvue/issues/321), [#323](https://github.com/cars10/elasticvue/issues/323), [#358](https://github.com/cars10/elasticvue/issues/358))
- Elasticvue [feature survey #55](https://github.com/cars10/elasticvue/issues/55) (223 respondents) ·
  [FAQ, "no read-only mode"](https://github.com/cars10/elasticvue/wiki/FAQ) ·
  [#225 maintainer on editing](https://github.com/cars10/elasticvue/issues/225) ·
  [#332](https://github.com/cars10/elasticvue/issues/332) · [#338](https://github.com/cars10/elasticvue/issues/338) · [#354](https://github.com/cars10/elasticvue/issues/354)
- [Optimistic concurrency control](https://www.elastic.co/docs/reference/elasticsearch/rest-apis/optimistic-concurrency-control) ·
  [Index API](https://www.elastic.co/guide/en/elasticsearch/reference/current/docs-index_.html) ·
  [Update API](https://www.elastic.co/guide/en/elasticsearch/reference/8.19/docs-update.html) ·
  [Bulk API](https://www.elastic.co/guide/en/elasticsearch/reference/8.19/docs-bulk.html) ·
  [refresh](https://www.elastic.co/guide/en/elasticsearch/reference/current/docs-refresh.html) ·
  [Reindex](https://www.elastic.co/guide/en/elasticsearch/reference/current/docs-reindex.html) ·
  [Aliases](https://www.elastic.co/guide/en/elasticsearch/reference/current/indices-aliases.html) ·
  [Put mapping](https://www.elastic.co/guide/en/elasticsearch/reference/current/indices-put-mapping.html) ·
  [Register a snapshot repository](https://www.elastic.co/guide/en/elasticsearch/reference/current/snapshots-register-repository.html) ·
  [cat APIs](https://www.elastic.co/guide/en/elasticsearch/reference/current/cat.html)
- **[ES 9.x breaking changes](https://www.elastic.co/docs/release-notes/elasticsearch/breaking-changes)** — TSDB sequence numbers ([#145737](https://github.com/elastic/elasticsearch/pull/145737)), strict bulk parsing ([#115923](https://github.com/elastic/elasticsearch/pull/115923)), uniform error JSON ([#90529](https://github.com/elastic/elasticsearch/pull/90529)), timeouts → 429 ([#116026](https://github.com/elastic/elasticsearch/pull/116026)), `include_source_on_error` ([#120725](https://github.com/elastic/elasticsearch/pull/120725))
- [Kibana Console (Dev Tools)](https://www.elastic.co/docs/explore-analyze/query-filter/tools/console) ·
  [`parser.ts`](https://github.com/elastic/kibana/blob/main/src/platform/packages/shared/kbn-monaco/src/languages/console/parser.ts) ·
  [`parser.test.ts`](https://github.com/elastic/kibana/blob/main/src/platform/packages/shared/kbn-monaco/src/languages/console/parser.test.ts) (the de-facto grammar spec)
- [`appbaseio/dejavu`](https://github.com/appbaseio/dejavu) ([#450](https://github.com/appbaseio/dejavu/issues/450)) ·
  [`lmenezes/cerebro`](https://github.com/lmenezes/cerebro) ([#591](https://github.com/lmenezes/cerebro/issues/591), [#600](https://github.com/lmenezes/cerebro/issues/600))
- [`version_conflict_engine_exception` explainer](https://www.baeldung.com/ops/elasticsearch-version_conflict_engine_exception)
