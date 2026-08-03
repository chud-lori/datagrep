#ifndef DBX_H
#define DBX_H
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

typedef struct DbxCore  DbxCore;    // opaque
typedef struct DbxQuery DbxQuery;   // opaque
typedef struct DbxRows  DbxRows;    // opaque, one materialised window

// ---- lifecycle -------------------------------------------------------
// Creates the engine + its own tokio runtime thread. Never blocks.
DbxCore* dbx_core_new(const char* profiles_db_path, char** err_out);
void     dbx_core_free(DbxCore*);
void     dbx_string_free(char*);            // frees any char* this API returned

// ---- profiles --------------------------------------------------------
// Returns JSON: [{"name":..,"driver":..,"env":..,"has_secret":bool}, ...]
char* dbx_profiles_list_json(DbxCore*, char** err_out);
bool  dbx_profiles_add(DbxCore*, const char* name, const char* url, char** err_out);
bool  dbx_profiles_remove(DbxCore*, const char* name, char** err_out);

// ---- catalog (lazy, ONE level per call) -------------------------------
// path_json is a JSON array of path segments, e.g. ["main"] or [] for roots.
// Returns JSON: [{"name":..,"kind":..,"has_children":bool,"enumeration":"cheap"|"scan_only"|"paged"|"on_demand"}, ...]
char* dbx_catalog_children_json(DbxCore*, const char* profile, const char* path_json, char** err_out);
char* dbx_catalog_describe_json(DbxCore*, const char* profile, const char* path_json, char** err_out);

// ---- query -----------------------------------------------------------
// Non-blocking: returns immediately with a handle; rows stream in the background.
DbxQuery* dbx_query_run(DbxCore*, const char* profile, const char* sql, char** err_out);
void      dbx_query_free(DbxQuery*);

// Cancel. ALWAYS returns instantly. outcome_json describes whether the SERVER
// also stopped (design §3.3) — caller must dbx_string_free it if non-NULL.
void dbx_query_cancel(DbxQuery*, char** outcome_json_out);

// Status snapshot as JSON:
// {"state":"streaming"|"parked"|"capped"|"done"|"cancelled"|"failed",
//  "rows_loaded":u64,"elapsed_ms":u64,"error":string|null,
//  "columns":[{"name":..,"type":..}],"total_known":bool}
char* dbx_query_status_json(DbxQuery*, char** err_out);

// Registers a callback fired when the query makes progress. Called from a
// background thread — the Swift side MUST hop to the main queue itself.
typedef void (*DbxProgressFn)(void* ctx);
void dbx_query_on_progress(DbxQuery*, DbxProgressFn cb, void* ctx);

// ---- rows: the hot path ----------------------------------------------
// Materialises ONLY [offset, offset+len). Returns NULL on error.
DbxRows* dbx_query_rows(DbxQuery*, uint64_t offset, uint64_t len, char** err_out);
void     dbx_rows_free(DbxRows*);

uint64_t dbx_rows_count(DbxRows*);        // rows actually available in this window
uint32_t dbx_rows_columns(DbxRows*);
bool     dbx_rows_pending(DbxRows*);      // true => not fetched yet, draw skeletons

// Cell text, borrowed — valid until dbx_rows_free. NOT null-terminated:
// use the returned length. UTF-8.
const char* dbx_rows_cell(DbxRows*, uint64_t row, uint32_t col, size_t* len_out);

// 0 = value, 1 = SQL NULL, 2 = ABSENT (field not present in the document),
// 3 = nested (document/array; cell text is a summary like "{3 fields}")
uint8_t dbx_rows_cell_kind(DbxRows*, uint64_t row, uint32_t col);

// Full raw value of one cell as JSON, for the detail pane. Caller frees.
char* dbx_rows_cell_detail_json(DbxRows*, uint64_t row, uint32_t col);

#endif
