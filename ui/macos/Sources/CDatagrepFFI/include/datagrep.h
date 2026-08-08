/* datagrep C ABI — FROZEN. Do not edit without a matching change in crates/datagrep-ffi.
 *
 * Ownership rules (mirrored by the Swift wrappers in Sources/DatagrepKit):
 *   - every OWNED `char*` returned by value or via an out-param must be
 *     released with datagrep_string_free(). Owned means `char*`; a
 *     `const char*` is borrowed and must NOT be freed — datagrep_rows_cell
 *     returns one, pointing into the row window's arena, and passing it to
 *     datagrep_string_free() corrupts the heap.
 *   - every DatagrepCore, DatagrepQuery and DatagrepRows pointer must be released with its
 *     matching _free()
 *   - datagrep_rows_cell() returns a BORROWED, NOT nul-terminated pointer valid
 *     only until the owning DatagrepRows is freed
 */
#ifndef DATAGREP_H
#define DATAGREP_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct DatagrepCore  DatagrepCore;
typedef struct DatagrepQuery DatagrepQuery;
typedef struct DatagrepRows  DatagrepRows;

DatagrepCore *datagrep_core_new(const char *profiles_db_path, char **err_out);
void     datagrep_core_free(DatagrepCore *);
void     datagrep_string_free(char *);

/* [{"name","driver","env","has_secret"}] */
char *datagrep_profiles_list_json(DatagrepCore *, char **err_out);
bool  datagrep_profiles_add(DatagrepCore *, const char *name, const char *url, char **err_out);
bool  datagrep_profiles_remove(DatagrepCore *, const char *name, char **err_out);

/* path_json = JSON array of segments, [] for roots
 * [{"name","kind","has_children","enumeration":"cheap"|"scan_only"|"paged"|"on_demand"}] */
char *datagrep_catalog_children_json(DatagrepCore *, const char *profile, const char *path_json,
                                char **err_out);
char *datagrep_catalog_describe_json(DatagrepCore *, const char *profile, const char *path_json,
                                char **err_out);

DatagrepQuery *datagrep_query_run(DatagrepCore *, const char *profile, const char *sql, char **err_out);
void      datagrep_query_free(DatagrepQuery *);
void      datagrep_query_cancel(DatagrepQuery *, char **outcome_json_out);
/* {"state":"streaming"|"parked"|"capped"|"done"|"cancelled"|"failed",
 *  "rows_loaded":u64,"elapsed_ms":u64,"error":str|null,
 *  "columns":[{"name","type"}],"total_known":bool} */
char *datagrep_query_status_json(DatagrepQuery *, char **err_out);

typedef void (*DatagrepProgressFn)(void *ctx);
/* NOTE: cb is invoked on a BACKGROUND THREAD. */
void datagrep_query_on_progress(DatagrepQuery *, DatagrepProgressFn cb, void *ctx);

DatagrepRows *datagrep_query_rows(DatagrepQuery *, uint64_t offset, uint64_t len, char **err_out);
void     datagrep_rows_free(DatagrepRows *);
uint64_t datagrep_rows_count(DatagrepRows *);
uint32_t datagrep_rows_columns(DatagrepRows *);
bool     datagrep_rows_pending(DatagrepRows *);
/* borrowed, NOT nul-terminated */
const char *datagrep_rows_cell(DatagrepRows *, uint64_t row, uint32_t col, size_t *len_out);
/* 0 value  1 NULL  2 ABSENT  3 nested */
uint8_t datagrep_rows_cell_kind(DatagrepRows *, uint64_t row, uint32_t col);
char   *datagrep_rows_cell_detail_json(DatagrepRows *, uint64_t row, uint32_t col);

#ifdef __cplusplus
}
#endif
#endif /* DATAGREP_H */
