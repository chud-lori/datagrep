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
// This is how a profile is born prod (env drives the design 3.8 prod
// guardrails: red chrome, confirm-on-write).
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

// Read-only truth for one profile (design 3.8: say WHICH protection is in
// force, never imply server enforcement that isn't there). Returns JSON:
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
// also stopped (design §3.3) — caller must datagrep_string_free it if non-NULL.
void datagrep_query_cancel(DatagrepQuery*, char** outcome_json_out);

// Status snapshot as JSON:
// {"state":"streaming"|"parked"|"capped"|"done"|"cancelled"|"failed",
//  "rows_loaded":u64,"affected_rows":u64|null,"elapsed_ms":u64,
//  "error":string|null,
//  "read_only": null | {"enforcement":"server"|"client"|"none",
//                       "server_confirmed":bool},   // see datagrep_connection_info_json
//  "columns":[{"name":..,"type":..}],"total_known":bool}
// A statement that a read-only profile refuses (Write/Ddl/Admin, classified
// client-side before dispatch) surfaces as state="failed" with an error
// naming the profile — it never reaches the server.
char* datagrep_query_status_json(DatagrepQuery*, char** err_out);

// Registers a callback fired when the query makes progress. Called from a
// background thread — the Swift side MUST hop to the main queue itself.
typedef void (*DatagrepProgressFn)(void* ctx);
void datagrep_query_on_progress(DatagrepQuery*, DatagrepProgressFn cb, void* ctx);

// ---- rows: the hot path ----------------------------------------------
// Materialises ONLY [offset, offset+len). Returns NULL on error.
DatagrepRows* datagrep_query_rows(DatagrepQuery*, uint64_t offset, uint64_t len, char** err_out);
void     datagrep_rows_free(DatagrepRows*);

uint64_t datagrep_rows_count(DatagrepRows*);        // rows actually available in this window
uint32_t datagrep_rows_columns(DatagrepRows*);
bool     datagrep_rows_pending(DatagrepRows*);      // true => not fetched yet, draw skeletons

// Cell text, borrowed — valid until datagrep_rows_free. NOT null-terminated:
// use the returned length. UTF-8.
const char* datagrep_rows_cell(DatagrepRows*, uint64_t row, uint32_t col, size_t* len_out);

// 0 = value, 1 = SQL NULL, 2 = ABSENT (field not present in the document),
// 3 = nested (document/array; cell text is a summary like "{3 fields}")
uint8_t datagrep_rows_cell_kind(DatagrepRows*, uint64_t row, uint32_t col);

// Full raw value of one cell as JSON, for the detail pane. Caller frees.
char* datagrep_rows_cell_detail_json(DatagrepRows*, uint64_t row, uint32_t col);

#endif
