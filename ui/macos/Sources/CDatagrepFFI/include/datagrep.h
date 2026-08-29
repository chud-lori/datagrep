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

/* [{"name","driver","read_only","safety","confirm_writes","color","has_secret"}]
 * "safety" is this connection's rung on the query-safety ladder:
 * "silent"|"warn_all"|"warn_writes"|"auth_all"|"auth_writes". "confirm_writes"
 * is the boolean it replaced, still reported (true = the rung asks for
 * something before a write) so an unmigrated caller keeps working. */
char *datagrep_profiles_list_json(DatagrepCore *, char **err_out);
bool  datagrep_profiles_add(DatagrepCore *, const char *name, const char *url, char **err_out);
bool  datagrep_profiles_remove(DatagrepCore *, const char *name, char **err_out);

/* {"profile","driver","database":str|null,
 *  "server":null|{"product","version"},"safety":str,
 *  "read_only":null|{"enforcement":"server"|"client"|"none","server_confirmed":bool}}
 * "server" is what the engine reported at handshake, never a guess, and is
 * null until a connection of this profile has succeeded. */
char *datagrep_connection_info_json(DatagrepCore *, const char *name, char **err_out);

/* path_json = JSON array of segments, [] for roots
 * [{"name","kind","has_children","enumeration":"cheap"|"scan_only"|"paged"|"on_demand"}] */
char *datagrep_catalog_children_json(DatagrepCore *, const char *profile, const char *path_json,
                                char **err_out);
char *datagrep_catalog_describe_json(DatagrepCore *, const char *profile, const char *path_json,
                                char **err_out);

/* The statement that reads one catalog object, in that engine's own language.
 * Pure: driver_id and the path are all it consults. `database` is the database
 * the connection is open on, or NULL when unknown — an object the statement
 * cannot reach returns NULL with *err_out saying which and why. */
char *datagrep_browse_statement(const char *driver_id, const char *path_json,
                                const char *database, char **err_out);

DatagrepQuery *datagrep_query_run(DatagrepCore *, const char *profile, const char *sql, char **err_out);
void      datagrep_query_free(DatagrepQuery *);
void      datagrep_query_cancel(DatagrepQuery *, char **outcome_json_out);
/* {"state":"streaming"|"parked"|"capped"|"done"|"cancelled"|"failed",
 *  "rows_loaded":u64,"elapsed_ms":u64,"error":str|null,
 *  "columns":[{"name","type"}],"total_known":bool,
 *  "safety":null|{"profile","level","requires":"warn"|"authenticate",
 *                 "challenge","statements":[{"text","class","requires"}]},
 *  "editable":null|{"identity":[str,..],"guard":[str,..],"root":str|null,
 *                   "atomic_batch":bool}}
 * "editable" is non-null only when the connection reports EDITABLE_RESULTS and
 * this result declared a row identity; `atomic_batch` false means a failing
 * batch can leave a prefix applied.
 * A non-null "safety" on state="failed" means the ladder refused this
 * statement and NOTHING was sent: clear the challenge, then run it again. */
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
/* This window's own column names as a JSON array — a document window projects
 * its own, which a heterogeneous result can make differ from the status JSON's.
 * Address a field by these, never by the header above the column. */
char   *datagrep_rows_column_names_json(DatagrepRows *);
/* The row's fields outside the projected root — for an ES hit the
 * `_index`/`_id`/`_routing` identity and the `_seq_no`/`_primary_term` guard a
 * write compares against. NULL when the result has no root. */
char   *datagrep_rows_envelope_json(DatagrepRows *, uint64_t row);

/* Commit one guarded MutationBatch. SYNCHRONOUS — it blocks until the write
 * lands, unlike datagrep_query_run. `mutation_json` is serde-encoded:
 * FieldPath is [{"Field":"_id"}], Value is {"Str":"x"}/{"I64":42}. Returns the
 * batch report
 *   {"rows":[{"op","_index","_id","outcome","conflict"?,"_seq_no"?,…}],
 *    "notices":[{"severity","code","message"}],
 *    "summary":{"applied","failed","not_attempted","conflicts"}}
 * or NULL with *err_out set (parse failure, read-only refusal, whole-batch
 * refusal). A per-row version conflict is a row with conflict=true, NOT an
 * error: the call still returns a report. */
char   *datagrep_mutate(DatagrepCore *, const char *profile, const char *mutation_json,
                        char **err_out);

/* What the server holds NOW for documents already addressed — the read half of
 * a version conflict, so a 409 becomes loaded / server-now / typed with a
 * rebase or discard-mine, rather than a retry that clobbers. SYNCHRONOUS.
 * `addresses_json` re-uses a mutation's own key:
 *   {"documents":[{"key":[[[{"Field":"_id"}],{"Str":"abc"}]]}]}
 * Returns
 *   {"documents":[{"found":true,"envelope":{…fresh _seq_no/_primary_term…},
 *                  "fields":{…the document…}},
 *                 {"found":false},{"found":false,"error":str}]}
 * one entry per address, IN THE ORDER SENT, or NULL with *err_out set when the
 * batch as a whole could not run. */
char   *datagrep_reread_documents(DatagrepCore *, const char *profile,
                                  const char *addresses_json, char **err_out);

/* ---- safe mode: the query-safety ladder, per connection ----------------
 * Five rungs on the profile: silent / warn_all / warn_writes / auth_all /
 * auth_writes, where "writes" means everything datagrep-lang does not classify
 * Read. The engine decides and judges; the frontend only performs the ceremony.
 * There is no "the user agreed" flag — the only way past a rung is a challenge
 * the engine minted, cleared by evidence it checks, yielding a grant bound to
 * that one statement, single-use and expiring. Not asking is a refusal. */

/* What running `sql` would require, without running it:
 * {"profile","level","requires":"none"|"warn"|"authenticate",
 *  "challenge":str|null,
 *  "statements":[{"text","class","requires"}]}
 * Clearing the challenge clears exactly the statements listed. */
char *datagrep_safety_evaluate_json(DatagrepCore *, const char *profile, const char *sql,
                                    char **err_out);

/* The challenges this connection has open — for a refusal from a SYNCHRONOUS
 * call (datagrep_mutate, datagrep_reread_documents), where the challenge id is
 * only named in *err_out. Same objects as _evaluate_json. */
char *datagrep_safety_pending_json(DatagrepCore *, const char *profile, char **err_out);

/* Report what the user did:
 *   {"kind":"acknowledged"}               a warning was shown and dismissed
 *   {"kind":"typed_phrase","typed":str}   what the user typed
 *   {"kind":"system_auth","method":str}   Touch ID / LocalAuthentication
 * "acknowledged" NEVER clears an "authenticate" rung. A typed phrase must equal
 * the connection name, which the engine holds and never sends, so it has to
 * come from the user. Prefer LocalAuthentication where it is available and fall
 * back to the typed phrase. */
bool datagrep_safety_satisfy(DatagrepCore *, const char *profile, const char *challenge,
                             const char *attestation_json, char **err_out);

#ifdef __cplusplus
}
#endif
#endif /* DATAGREP_H */
