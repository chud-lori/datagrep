#ifndef DATAGREP_H
#define DATAGREP_H
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

typedef struct DatagrepCore  DatagrepCore;    // opaque
typedef struct DatagrepQuery DatagrepQuery;   // opaque
typedef struct DatagrepRows  DatagrepRows;    // opaque, one materialised window
typedef struct DatagrepExport DatagrepExport; // opaque, one running export

// ---- lifecycle -------------------------------------------------------
// Creates the engine + its own tokio runtime thread. Never blocks.
DatagrepCore* datagrep_core_new(const char* profiles_db_path, char** err_out);
void     datagrep_core_free(DatagrepCore*);
void     datagrep_string_free(char*);            // frees any char* this API returned

// ---- profiles --------------------------------------------------------
// Returns JSON:
// [{"name":..,"driver":..,"read_only":bool,
//   "safety":"silent"|"warn_all"|"warn_writes"|"auth_all"|"auth_writes",
//   "confirm_writes":bool,          // legacy view of "safety": writes need something
//   "color":str|null,"has_secret":bool}, ...]
char* datagrep_profiles_list_json(DatagrepCore*, char** err_out);
// Adds with default settings (writeable, no confirmation, no limits). Use
// datagrep_profiles_add_json to set the safety settings at creation time.
bool  datagrep_profiles_add(DatagrepCore*, const char* name, const char* url, char** err_out);
// datagrep_profiles_add with initial settings. options_json is NULL, "", or any
// subset of:
// {"read_only":bool,"safety":str,"confirm_writes":bool,
//  "auto_limit":i64|null,"idle_timeout_s":i64|null,"color":str|null,
//  "ssh":SSH|null}
// SSH = {"host":str,"port":u16 (22),"user":str,
//        "auth":"agent"|"key"|"password" (agent),"key_path":str|null,
//        "secret":str}
// "secret" (SSH password or key passphrase) is stored only in the keychain;
// absent or "" keeps the saved one while "auth" is unchanged.
// With "ssh" the engine dials a 127.0.0.1 forward to the URL's host:port.
// "safety" is the query-safety ladder for THIS connection, one of silent /
// warn_all / warn_writes / auth_all / auth_writes; "confirm_writes" is the
// boolean it replaced, still accepted (true = warn_writes) so a frontend can
// migrate, and still reported so an unmigrated one keeps working. When both
// are given, "safety" wins. It is orthogonal to "read_only": read-only REFUSES
// writes, the ladder GATES them.
bool  datagrep_profiles_add_json(DatagrepCore*, const char* name, const char* url,
                                 const char* options_json, char** err_out);
// Edit an existing profile, keyed by its current name. patch_json is any
// subset of:
// {"name":str,"url":str,"read_only":bool,"safety":str,
//  "confirm_writes":bool,"auto_limit":i64|null,"idle_timeout_s":i64|null,
//  "color":str|null,"ssh":SSH|null}
// Absent key = leave alone; JSON null = clear (auto_limit/idle_timeout_s/
// color/ssh only; a cleared tunnel's secret leaves the keychain too). Unknown keys are errors, not ignored. Renaming keeps the
// profile id and therefore its keychain secret. A new "url" is re-parsed and
// any inline password is re-split into the keychain exactly as _add does; a
// URL without a password keeps the stored secret (unless the engine changed).
// The edit applies to the NEXT query — the stale pool is closed here.
bool  datagrep_profiles_update(DatagrepCore*, const char* name,
                               const char* patch_json, char** err_out);
// Full detail for one profile — what an edit dialog populates from. JSON:
// {"name":str,"driver":str,"read_only":bool,"safety":str,
//  "confirm_writes":bool,"auto_limit":i64|null,"idle_timeout_s":i64|null,
//  "color":str|null,"folder_id":str|null,"has_secret":bool,
//  "secret":"••••"|null,"config":{key:str|num|bool,...},
//  "ssh":null|{"host":str,"port":u16,"user":str,"auth":str,
//              "key_path":str|null,"has_secret":bool},
//  "last_used_at":i64|null}
// The secret VALUE never crosses this ABI: "secret" is the mask string when
// one is stored in the keychain, null otherwise, and "config" is the
// persisted secretless connection config (secret-schema keys re-masked).
char* datagrep_profiles_get_json(DatagrepCore*, const char* name, char** err_out);
bool  datagrep_profiles_remove(DatagrepCore*, const char* name, char** err_out);

// Read-only truth for one profile: say WHICH protection is in force, never
// imply server enforcement that isn't there. Returns JSON:
// {"profile":str,"driver":str,"env":"dev"|"staging"|"prod",
//  "safety":"silent"|"warn_all"|"warn_writes"|"auth_all"|"auth_writes",
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

// Dial once and report what answered, saving nothing. A non-empty `name` tests
// that saved profile with its keychain secret; otherwise `url` tests an
// unsaved URL. BLOCKING for up to the engine's connect timeout. Returns JSON:
// {"ok":true,"driver":str,"product":str,"version":str,
//  "details":[[str,str],..],"elapsed_ms":u64}
// or NULL with *err_out set to the driver's own failure message.
char* datagrep_connection_test_json(DatagrepCore*, const char* name, const char* url,
                                    char** err_out);
// datagrep_connection_test_json for an unsaved dialog; empty `url` = saved config.
// options_json is NULL, "" or {"ssh":SSH|null}: absent = saved tunnel, null = none.
// An empty SSH "secret" is borrowed from `name`'s saved tunnel when "auth" matches.
char* datagrep_connection_test_with_json(DatagrepCore*, const char* name, const char* url,
                                         const char* options_json, char** err_out);

// ---- SSH host keys -----------------------------------------------------
// Fail-closed: only keys pinned here or in ~/.ssh/known_hosts (never written) connect;
// an unknown key fails until the user reviews it, a changed key always fails.
// Key exchange only, nothing is authenticated or sent. BLOCKING. JSON:
// {"host":str,"port":u16,"algorithm":str,"fingerprint":"SHA256:...",
//  "status":"trusted"|"unknown"|"changed","expected":str|null,
//  "known_hosts":str}   // the file datagrep pins keys in
char* datagrep_ssh_host_key_json(DatagrepCore*, const char* host, uint16_t port,
                                 char** err_out);
// Pins the key the last _host_key_json call saw, only if `fingerprint` is the one shown.
// Refuses a "changed" host; its old line must be removed from "known_hosts" by hand.
bool  datagrep_ssh_trust_host_key(DatagrepCore*, const char* host, uint16_t port,
                                  const char* fingerprint, char** err_out);

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

// The statement that reads one catalog object, written in that engine's own
// language and carrying its own comment marker on the @limit line. Pure: no
// core, no connection, no I/O — driver_id and the path are all it consults.
//
// `database` is the database this connection is open on, or NULL when it is
// not known. It is what lets the refusal be specific rather than a guess: the
// catalog can list databases a statement cannot reach (a Mongo collection
// outside the connection's database, a Postgres table in another database), and
// those return NULL with *err_out naming the database and why it is unreachable.
// A Redis key is refused too — the value's shape decides its command.
//
// Returns an OWNED char* the caller MUST datagrep_string_free().
char* datagrep_browse_statement(const char* driver_id, const char* path_json,
                                const char* database, char** err_out);

// ---- editor ----------------------------------------------------------
// Completion candidates for the caret at `caret`, a UTF-8 BYTE offset into
// `text` (the whole editor buffer). Returns JSON:
// {"prefix":str,"items":[{"label":str,"insert":str,"kind":str,"detail":str|null}],
//  "error":str|null}
// Accepting an item replaces the `prefix` text just before the caret with
// "insert". "kind" is keyword/schema/table/view/column for SQL engines and the
// catalog kind (collection/field/key) otherwise. SQL names come from the
// catalog only — the text is parsed here, never sent to the server — and are
// cached per connection: the table list on first use, a table's columns the
// first time a statement names it. A DDL statement run through
// datagrep_query_run drops the cache, as do datagrep_complete_forget and a
// five-minute age limit. "error" is set when the catalog could not be read;
// keywords are still returned.
// SYNCHRONOUS: the first call per connection lists its tables, so call it off
// the UI thread. Returns an OWNED char* the caller MUST datagrep_string_free().
char* datagrep_complete_json(DatagrepCore*, const char* profile, const char* text,
                             size_t caret, char** err_out);
// Drops the connection's cached names, e.g. when the user refreshes its schema.
void  datagrep_complete_forget(DatagrepCore*, const char* profile);
// The statement(s) re-flowed one clause per line with keywords upper-cased.
// Only whitespace between tokens and keyword case change; strings, quoted
// names and comments are kept byte for byte. NULL with *err_out set for an
// engine whose language is not SQL. Pure: no connection is used.
// Returns an OWNED char* the caller MUST datagrep_string_free().
char* datagrep_sql_format(const char* driver_id, const char* sql, char** err_out);

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
//  "safety": null                         // nothing was refused
//           | {"profile":str,"level":str,"requires":"warn"|"authenticate",
//              "challenge":str,
//              "statements":[{"text":str,"class":str,"requires":str}]},
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
// A statement the SAFETY LADDER refuses surfaces the same way, plus a non-null
// "safety" carrying the challenge to clear: warn or authenticate, then run the
// same statement again. Nothing was sent.
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

// ---- safe mode: the query-safety ladder, per connection ----------------
// Five rungs, set per profile as "safety": silent (send everything) /
// warn_all (warn before every query) / warn_writes (warn except reads) /
// auth_all (authenticate before every query) / auth_writes (authenticate
// except reads). "Reads" is datagrep-lang's Read classification — SELECT,
// EXPLAIN, SHOW and their per-engine equivalents; anything it cannot classify
// counts as a write.
//
// The engine decides, the frontend performs the ceremony, the engine judges
// the result. There is no "the user said yes" flag: the ONLY way past a rung
// is an engine-minted challenge, cleared by evidence the engine checks, which
// yields a grant bound to that exact statement, single-use and expiring
// (2 min). A frontend that does not ask gets a refusal, never a query — the
// gate sits on the one path every request takes, below this ABI, so a new
// entry point inherits it rather than having to remember it.
//
// What this statement would require, without running it. Returns JSON:
// {"profile":str,"level":str,"requires":"none"|"warn"|"authenticate",
//  "challenge":str|null,                // null iff requires == "none"
//  "statements":[{"text":str,           // one entry per statement in `sql`
//                 "class":"read"|"write"|"ddl"|"tcl"|"admin"|"unknown",
//                 "requires":str}]}
// Clearing the returned challenge clears every statement listed, and nothing
// else. Caller frees.
char* datagrep_safety_evaluate_json(DatagrepCore*, const char* profile, const char* sql,
                                    char** err_out);

// The challenges this connection has open, newest last — the same objects, for
// a caller that hit a refusal from a SYNCHRONOUS entry point (datagrep_mutate,
// datagrep_reread_documents) where the challenge is only named in *err_out.
// Caller frees.
char* datagrep_safety_pending_json(DatagrepCore*, const char* profile, char** err_out);

// Report what the user actually did. attestation_json is one of:
//   {"kind":"acknowledged"}                      // a warning was shown and dismissed
//   {"kind":"typed_phrase","typed":str}          // the user typed something
//   {"kind":"system_auth","method":str}          // Touch ID, polkit, ...
// An "acknowledged" NEVER clears an "authenticate" rung — the engine refuses
// it. A typed phrase must equal the CONNECTION NAME, which the engine holds
// and never sends in a challenge, so the string has to come from the user.
// System auth is the recommended path where the platform offers it; the typed
// phrase is the fallback where it does not.
//
// true = cleared; the same statement now runs, once. false with *err_out set =
// unknown/expired/already-used challenge, or evidence too weak for the rung.
bool datagrep_safety_satisfy(DatagrepCore*, const char* profile, const char* challenge,
                             const char* attestation_json, char** err_out);

// ---- export: one statement's full result to a file ---------------------
// Formats this engine can export, as a JSON array drawn from
// ["csv","json","markdown","sql"]. "sql" (INSERT statements) is offered only
// where an INSERT can recreate the rows. Caller frees.
char* datagrep_export_formats_json(const char* driver_id);

// Non-blocking, like datagrep_query_run: re-runs `sql` on `profile` and streams
// every row, never just the loaded window, into `path`. It takes the same gated
// path as a run, so the safety ladder and read-only apply; a refusal surfaces
// in the status exactly as it does for a query. Exactly one statement, and one
// that only reads — export never re-runs a write. `table` names the INSERT
// target for "sql" (dots qualify it) and is ignored otherwise; NULL is allowed.
// The file appears only on success: rows go to `path`.part, renamed when done.
DatagrepExport* datagrep_export_start(DatagrepCore*, const char* profile, const char* sql,
                                      const char* format, const char* table,
                                      const char* path, char** err_out);
// {"state":"running"|"done"|"cancelled"|"failed","rows_written":u64,
//  "error":str|null,"safety":null|{...same object as datagrep_query_status_json}}
char* datagrep_export_status_json(DatagrepExport*, char** err_out);
// Returns instantly; the server is asked to stop and the partial file removed.
void  datagrep_export_cancel(DatagrepExport*);
// Cancels a running export.
void  datagrep_export_free(DatagrepExport*);

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
