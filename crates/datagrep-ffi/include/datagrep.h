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
// Returns JSON: [{"name":..,"driver":..,"env":..,"has_secret":bool}, ...]
char* datagrep_profiles_list_json(DatagrepCore*, char** err_out);
bool  datagrep_profiles_add(DatagrepCore*, const char* name, const char* url, char** err_out);
bool  datagrep_profiles_remove(DatagrepCore*, const char* name, char** err_out);

// ---- catalog (lazy, ONE level per call) -------------------------------
// path_json is a JSON array of path segments, e.g. ["main"] or [] for roots.
// Returns JSON: [{"name":..,"kind":..,"has_children":bool,"enumeration":"cheap"|"scan_only"|"paged"|"on_demand"}, ...]
char* datagrep_catalog_children_json(DatagrepCore*, const char* profile, const char* path_json, char** err_out);
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
//  "rows_loaded":u64,"elapsed_ms":u64,"error":string|null,
//  "columns":[{"name":..,"type":..}],"total_known":bool}
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
