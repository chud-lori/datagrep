/* dbx C ABI — FROZEN. Do not edit without a matching change in crates/dbx-ffi.
 *
 * Ownership rules (mirrored by the Swift wrappers in Sources/DbxKit):
 *   - every `char*` returned by value or via an out-param must be released
 *     with dbx_string_free()
 *   - every DbxCore, DbxQuery and DbxRows pointer must be released with its
 *     matching _free()
 *   - dbx_rows_cell() returns a BORROWED, NOT nul-terminated pointer valid
 *     only until the owning DbxRows is freed
 */
#ifndef DBX_FFI_H
#define DBX_FFI_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct DbxCore  DbxCore;
typedef struct DbxQuery DbxQuery;
typedef struct DbxRows  DbxRows;

DbxCore *dbx_core_new(const char *profiles_db_path, char **err_out);
void     dbx_core_free(DbxCore *);
void     dbx_string_free(char *);

/* [{"name","driver","env","has_secret"}] */
char *dbx_profiles_list_json(DbxCore *, char **err_out);
bool  dbx_profiles_add(DbxCore *, const char *name, const char *url, char **err_out);
bool  dbx_profiles_remove(DbxCore *, const char *name, char **err_out);

/* path_json = JSON array of segments, [] for roots
 * [{"name","kind","has_children","enumeration":"cheap"|"scan_only"|"paged"|"on_demand"}] */
char *dbx_catalog_children_json(DbxCore *, const char *profile, const char *path_json,
                                char **err_out);
char *dbx_catalog_describe_json(DbxCore *, const char *profile, const char *path_json,
                                char **err_out);

DbxQuery *dbx_query_run(DbxCore *, const char *profile, const char *sql, char **err_out);
void      dbx_query_free(DbxQuery *);
void      dbx_query_cancel(DbxQuery *, char **outcome_json_out);
/* {"state":"streaming"|"parked"|"capped"|"done"|"cancelled"|"failed",
 *  "rows_loaded":u64,"elapsed_ms":u64,"error":str|null,
 *  "columns":[{"name","type"}],"total_known":bool} */
char *dbx_query_status_json(DbxQuery *, char **err_out);

typedef void (*DbxProgressFn)(void *ctx);
/* NOTE: cb is invoked on a BACKGROUND THREAD. */
void dbx_query_on_progress(DbxQuery *, DbxProgressFn cb, void *ctx);

DbxRows *dbx_query_rows(DbxQuery *, uint64_t offset, uint64_t len, char **err_out);
void     dbx_rows_free(DbxRows *);
uint64_t dbx_rows_count(DbxRows *);
uint32_t dbx_rows_columns(DbxRows *);
bool     dbx_rows_pending(DbxRows *);
/* borrowed, NOT nul-terminated */
const char *dbx_rows_cell(DbxRows *, uint64_t row, uint32_t col, size_t *len_out);
/* 0 value  1 NULL  2 ABSENT  3 nested */
uint8_t dbx_rows_cell_kind(DbxRows *, uint64_t row, uint32_t col);
char   *dbx_rows_cell_detail_json(DbxRows *, uint64_t row, uint32_t col);

#ifdef __cplusplus
}
#endif
#endif /* DBX_FFI_H */
