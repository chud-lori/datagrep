#ifndef DATAGREP_H
#define DATAGREP_H
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

typedef struct DatagrepCore  DatagrepCore;    // opaque
typedef struct DatagrepQuery DatagrepQuery;   // opaque
typedef struct DatagrepRows  DatagrepRows;    // opaque, one materialised window

// ---- lifecycle -------------------------------------------------------
// Creates the engine + its own tokio runtime thread. Never blocks.
DatagrepCore* datagrep_core_new(const char* profiles_db_path, char** err_out);
void     datagrep_core_free(DatagrepCore*);
void     datagrep_string_free(char*);            // frees any char* this API returned

// ---- profiles --------------------------------------------------------
// Returns JSON:
// [{"name":..,"driver":..,"env":"dev"|"staging"|"prod","read_only":bool,
//   "has_secret":bool}, ...]
// env tints prod rows; read_only badges guarded rows — no per-row round trip.
char* datagrep_profiles_list_json(DatagrepCore*, char** err_out);
// Adds with default settings (env=dev, writeable, no limits). Use
// datagrep_profiles_add_json to set env / safety settings at creation time.
bool  datagrep_profiles_add(DatagrepCore*, const char* name, const char* url, char** err_out);
// datagrep_profiles_add with initial settings. options_json is NULL, "", or any
// subset of:
// {"env":"dev"|"staging"|"prod","read_only":bool,"confirm_writes":bool,
//  "auto_limit":i64|null,"idle_timeout_s":i64|null,"color":str|null}
// This is how a profile is born prod: env drives the prod guardrails (red
// chrome, confirm-on-write).
bool  datagrep_profiles_add_json(DatagrepCore*, const char* name, const char* url,
                                 const char* options_json, char** err_out);
// Edit an existing profile, keyed by its current name. patch_json is any
// subset of:
// {"name":str,"url":str,"env":"dev"|"staging"|"prod","read_only":bool,
//  "confirm_writes":bool,"auto_limit":i64|null,"idle_timeout_s":i64|null,
//  "color":str|null}
// Absent key = leave alone; JSON null = clear (auto_limit/idle_timeout_s/
// color only). Unknown keys are errors, not ignored. Renaming keeps the
// profile id and therefore its keychain secret. A new "url" is re-parsed and
// any inline password is re-split into the keychain exactly as _add does; a
// URL without a password keeps the stored secret (unless the engine changed).
// The edit applies to the NEXT query — the stale pool is closed here.
bool  datagrep_profiles_update(DatagrepCore*, const char* name,
                               const char* patch_json, char** err_out);
// Full detail for one profile — what an edit dialog populates from. JSON:
// {"name":str,"driver":str,"env":"dev"|"staging"|"prod","read_only":bool,
//  "confirm_writes":bool,"auto_limit":i64|null,"idle_timeout_s":i64|null,
//  "color":str|null,"folder_id":str|null,"has_secret":bool,
//  "secret":"••••"|null,"config":{key:str|num|bool,...},
//  "last_used_at":i64|null}
// The secret VALUE never crosses this ABI: "secret" is the mask string when
// one is stored in the keychain, null otherwise, and "config" is the
// persisted secretless connection config (secret-schema keys re-masked).
char* datagrep_profiles_get_json(DatagrepCore*, const char* name, char** err_out);
bool  datagrep_profiles_remove(DatagrepCore*, const char* name, char** err_out);

// Read-only truth for one profile: say WHICH protection is in force, never
// imply server enforcement that isn't there. Returns JSON:
// {"profile":str,"driver":str,"env":"dev"|"staging"|"prod",
//  "read_only": null                                    // profile is writeable
//             | {"enforcement":"server"|"client"|"none",
//                "server_confirmed":bool}}
// "server" - a live connection accepted a server-side read-only session (PG/
//   MySQL SET SESSION ... READ ONLY, SQLite PRAGMA query_only); only then is
//   server_confirmed true.
// "client" - only this process blocks writes: statements classified Write/
//   Ddl/Admin are refused before dispatch. Redis has no server-side mode, and
//   a profile that has never connected is also at most "client". A client-only
//   badge MUST say so - it is not the server protecting you.
// "none"   - no enforcement of any kind is available.
// The same object appears as "read_only" in datagrep_query_status_json.
// Also carries what a header badge needs to name where the user is:
//   "database": str|null   // what this profile points at; null on an engine
//                          // with no database concept (Redis, SQLite)
//   "server":   null                                  // never connected yet
//             | {"product":str,"version":str}         // reported at handshake
// "server" is NEVER guessed - an unconfirmed version is the number a user
// would quote when asking whether a feature exists on their server. This call
// warms it from the pool once; a profile that cannot be reached still returns
// its identity, with "server":null.
char* datagrep_connection_info_json(DatagrepCore*, const char* name, char** err_out);

// ---- catalog (lazy, ONE level per call) -------------------------------
// path_json is a JSON array of path segments, e.g. ["main"] or [] for roots.
// Returns JSON: [{"name":..,"kind":..,"has_children":bool,"enumeration":"cheap"|"scan_only"|"paged"|"on_demand"}, ...]
char* datagrep_catalog_children_json(DatagrepCore*, const char* profile, const char* path_json, char** err_out);
// Full detail for one object. Fetched lazily — columns, indexes and stats are
// read only when THIS call is made for THIS path, never on tree expansion.
// Returns JSON:
// {"path":[..],"name":..,"kind":..,"has_children":bool,"comment":string|null,
//  "columns":[{"name":..,"ordinal":int,"native_type":string|null,
//              "logical_type":string|null,"type":string,       // legacy alias
//              "nullable":bool,"default":string|null,"primary_key":bool,
//              "unique":bool,"indexed":bool,"auto_generated":bool,
//              "presence_ratio":double}]                       // sampled engines only
//            | null,                    // null = engine declares no schema
//  "indexes":[{"name":..,"columns":[{"name":string|null,"order":"asc"|"desc"|null}],
//              "unique":bool,"primary":bool,"type":"btree"|"gin"|"text"|..,
//              "partial":bool,"filter":string|null,"size_bytes":i64|null,
//              "definition":string|null,"sparse":bool,
//              "expire_after_seconds":i64|null}]
//            | null,                    // null = not reported; [] = none exist
//  "row_estimate":i64|null,             // estimate, never a COUNT(*)
//  "size_bytes":i64|null,
//  "inferred":bool,                     // true = columns come from sampling
//  "sampled_docs":u64|null,             // sample size behind `inferred`
//  "extra":[[k,v],..]}                  // engine-specific display pairs
char* datagrep_catalog_describe_json(DatagrepCore*, const char* profile, const char* path_json, char** err_out);

// ---- query -----------------------------------------------------------
// Non-blocking: returns immediately with a handle; rows stream in the background.
DatagrepQuery* datagrep_query_run(DatagrepCore*, const char* profile, const char* sql, char** err_out);
void      datagrep_query_free(DatagrepQuery*);

// Cancel. ALWAYS returns instantly. outcome_json describes whether the SERVER
// also stopped — caller must datagrep_string_free it if non-NULL.
void datagrep_query_cancel(DatagrepQuery*, char** outcome_json_out);

// Status snapshot as JSON:
// {"state":"streaming"|"parked"|"capped"|"done"|"cancelled"|"failed",
//  "rows_loaded":u64,"affected_rows":u64|null,"elapsed_ms":u64,
//  "error":string|null,
//  "read_only": null | {"enforcement":"server"|"client"|"none",
//                       "server_confirmed":bool},   // see datagrep_connection_info_json
//  "columns":[{"name":..,"type":..}],"total_known":bool,
//  "editable": null                       // this result cannot be edited
//            | {"identity":[str,..],      // fields naming ONE row, e.g.
//                                         // ["_index","_id","_routing"]
//               "guard":[str,..],         // fields a write must compare
//                                         // against, e.g. ["_seq_no",
//                                         // "_primary_term"] — send them as
//                                         // `expect`, loaded values and all
//               "root":str|null,          // the field the columns are
//                                         // projected from ("_source"); the
//                                         // rest of the row is the envelope
//               "atomic_batch":bool}}     // false = a failing batch can leave
//                                         // a prefix applied, and the commit
//                                         // confirmation must say so
// "editable" is non-null only when the connection reports EDITABLE_RESULTS AND
// this result declared a row identity: an aggregate has no identity even on a
// connection whose rows usually do, and a profile that has not connected yet
// reports null rather than a guess. It is what a grid must consult before it
// offers an edit — the mutation it would build is addressed by exactly these
// identity fields.
// A statement that a read-only profile refuses (Write/Ddl/Admin, classified
// client-side before dispatch) surfaces as state="failed" with an error
// naming the profile — it never reaches the server.
char* datagrep_query_status_json(DatagrepQuery*, char** err_out);

// Registers a callback fired when the query makes progress. Called from a
// background thread — the Swift side MUST hop to the main queue itself.
typedef void (*DatagrepProgressFn)(void* ctx);
void datagrep_query_on_progress(DatagrepQuery*, DatagrepProgressFn cb, void* ctx);

// ---- mutate: commit one guarded document edit ------------------------
// SYNCHRONOUS: blocks until the commit completes (unlike datagrep_query_run,
// which returns immediately and streams). A save is a discrete commit the UI
// waits on, not a stream it scrolls.
//
// mutation_json is a serde-encoded MutationBatch — the structured write op the
// driver compiles natively. Externally-tagged shape:
// {"mutations":[
//   {"Update":{"path":["events"],
//              "key":[[[{"Field":"_index"}],{"Str":"events"}],
//                     [[{"Field":"_id"}],{"Str":"abc"}]],
//              "sets":[[[{"Field":"status"}],{"Str":"done"}]],
//              "expect":[[[{"Field":"_seq_no"}],{"I64":41}],
//                        [[{"Field":"_primary_term"}],{"I64":3}]]}},
//   {"Insert":{"path":["events"],"doc":{"Document":[...]}}},
//   {"Delete":{"path":["events"],
//              "key":[[[{"Field":"_id"}],{"Str":"gone"}]]}}]}
// (`expect` is optional; an Elasticsearch update/delete without an
//  `_seq_no`/`_primary_term` guard is refused, never sent unguarded.)
//
// Runs through the same lease/pool path as a query, so a read-only profile
// refuses the write (surfaced as an error: NULL return, *err_out set) rather
// than committing it.
//
// Returns an OWNED char* — the batch report as JSON — that the caller MUST
// datagrep_string_free(). NULL on error (parse failure, read-only refusal, a
// whole-batch driver refusal) with *err_out set. Report schema:
// {
//   "rows": [ {"op":"update"|"insert"|"delete",
//              "_index":str,"_id":str,"_routing":str?,
//              "outcome":"applied"|"failed"|"not attempted",
//              "result":str?,"_seq_no":i64?,"_primary_term":i64?,
//              "conflict":true?,"error_code":str?,"error":str?,
//              "forced_refresh":true?} ],   // rows are clean flat JSON
//   "notices": [ {"severity":"info"|"warning","code":str|null,"message":str} ],
//   "summary": {"applied":u64,"failed":u64,"not_attempted":u64,"conflicts":u64}
// }
// A per-row version conflict (ES 409) is a row with outcome="failed" and
// conflict=true — a UI state, NOT an error — so the call still returns a report.
char* datagrep_mutate(DatagrepCore*, const char* profile, const char* mutation_json,
                      char** err_out);

// Read what the server holds NOW for documents already addressed — the read
// half of a version conflict. SYNCHRONOUS, like datagrep_mutate.
//
// This is what turns a 409 into a decision instead of a dead end: the caller
// puts the value it loaded, the value here, and the value the user typed side
// by side, then offers "rebase onto this version" or "discard mine". It never
// re-sends anything — retry_on_conflict is exactly the clobber the guard
// exists to prevent.
//
// addresses_json re-uses a mutation's own `key` (identity fields paired with
// this document's values), so nothing has to build a second address:
// {"documents":[{"key":[[[{"Field":"_index"}],{"Str":"events"}],
//                       [[{"Field":"_id"}],{"Str":"abc"}]]}]}
//
// Returns an OWNED char* the caller MUST datagrep_string_free():
// {"documents":[ {"found":true,
//                 "envelope":{...},   // outside the projected root: which
//                                     // document, and the FRESH guard values
//                                     // (_seq_no/_primary_term) a rebase
//                                     // re-guards against
//                 "fields":{...}},    // the document itself, at its root
//                {"found":false},                // gone from the server
//                {"found":false,"error":str} ]}  // this one could not be read
// One entry per address, IN THE ORDER SENT — matched by position, exactly like
// the mutation report.
//
// NULL with *err_out set when the batch as a whole could not run: an
// unparseable list, an unknown profile, a connection that could not be leased,
// or an engine that has not said which identity field names the object a
// document lives in (only Elasticsearch has, today).
char* datagrep_reread_documents(DatagrepCore*, const char* profile,
                                const char* addresses_json, char** err_out);

// ---- rows: the hot path ----------------------------------------------
// Materialises ONLY [offset, offset+len). Returns NULL on error.
DatagrepRows* datagrep_query_rows(DatagrepQuery*, uint64_t offset, uint64_t len, char** err_out);
void     datagrep_rows_free(DatagrepRows*);

uint64_t datagrep_rows_count(DatagrepRows*);        // rows actually available in this window
uint32_t datagrep_rows_columns(DatagrepRows*);
bool     datagrep_rows_pending(DatagrepRows*);      // true => not fetched yet, draw skeletons

// Cell text, borrowed — valid until datagrep_rows_free. NOT null-terminated:
// use the returned length. UTF-8.
//
// NEVER pass this to datagrep_string_free(): it points into the window's arena
// rather than being separately allocated, so freeing it corrupts the heap. The
// `const char*` return type is the signal — only an owned `char*` is freeable.
const char* datagrep_rows_cell(DatagrepRows*, uint64_t row, uint32_t col, size_t* len_out);

// 0 = value, 1 = SQL NULL, 2 = ABSENT (field not present in the document),
// 3 = nested (document/array; cell text is a summary like "{3 fields}")
uint8_t datagrep_rows_cell_kind(DatagrepRows*, uint64_t row, uint32_t col);

// Full raw value of one cell as JSON, for the detail pane. Caller frees.
char* datagrep_rows_cell_detail_json(DatagrepRows*, uint64_t row, uint32_t col);

// This window's own column names, as a JSON array. Caller frees.
//
// A document result has no global column list, so a window projects the union
// of the field names ITS rows carry, while the status JSON reports what the
// first chunk revealed. Those agree for a homogeneous result and may not for a
// heterogeneous one — so anything addressing a field by name (an edit naming
// the field it sets) must ask the window the value came from, not the header
// drawn above it.
char* datagrep_rows_column_names_json(DatagrepRows*);

// The row's fields OUTSIDE the projected root — its envelope — as one JSON
// object. Caller frees. NULL for a row outside the window, and for any result
// whose driver declared no root (there is then nothing outside the row).
//
// This is where the facts a guarded write needs live, because none of them
// belong in a column of the user's own document: for an Elasticsearch hit,
// `_index`/`_id`/`_routing` (which document) and `_seq_no`/`_primary_term`
// (which version of it was loaded — the compare-and-swap `datagrep_mutate`
// sends as `expect`). Read it for the row being edited, at the moment the edit
// is staged: it is the loaded version, not the current one, that a guard has
// to carry.
char* datagrep_rows_envelope_json(DatagrepRows*, uint64_t row);

#endif
