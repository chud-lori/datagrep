/*
 * smoke.c — the C-side proof that `include/datagrep.h` is real.
 *
 * This is deliberately written the way the Swift app will drive the ABI:
 * every string that comes back is freed with `datagrep_string_free`, every cell is
 * read as a (pointer, length) pair without assuming NUL termination, and the
 * only thing ever waited on is a status poll between calls.
 *
 * What it proves, in order:
 *   1. a core over a temp profiles db starts and never blocks
 *   2. a SQLite profile can be added and listed
 *   3. a 10 000-row table can be created through the query path
 *   4. the lazy catalog lists one level per call
 *   5. rows [0, 50) materialise
 *   6. rows [9000, 9050) materialise — WITHOUT the 8 950 rows in between
 *      having to be touched by the caller
 *   7. a SQL NULL reports kind 1, an empty string reports kind 0
 *   8. a long query cancels instantly and reports the server's answer
 *
 * Build/run: ./tests/run_smoke.sh
 */

#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "datagrep.h"

static int failures = 0;

static void ok(bool cond, const char *what) {
    printf("%s %s\n", cond ? "  ok  " : "  FAIL", what);
    if (!cond) failures++;
}

/* Abort loudly if an err_out came back non-NULL. */
static void no_err(char *err, const char *what) {
    if (err) {
        printf("  FAIL %s: %s\n", what, err);
        failures++;
        datagrep_string_free(err);
    }
}

static void nap_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

/* Copy a (not NUL-terminated) cell into a caller buffer. */
static const char *cell(DatagrepRows *rows, uint64_t r, uint32_t c, char *buf, size_t n) {
    size_t len = 0;
    const char *p = datagrep_rows_cell(rows, r, c, &len);
    if (!p) { buf[0] = '\0'; return buf; }
    if (len >= n) len = n - 1;
    memcpy(buf, p, len);
    buf[len] = '\0';
    return buf;
}

/* Progress callback — fired from a tokio worker thread, per the header. */
static void on_progress(void *ctx) {
    atomic_fetch_add((_Atomic int *)ctx, 1);
}

/* Poll status until it contains `needle`, or the deadline passes. */
static char *await_state(DatagrepQuery *q, const char *needle, int timeout_ms) {
    for (int waited = 0; waited < timeout_ms; waited += 10) {
        char *err = NULL;
        char *status = datagrep_query_status_json(q, &err);
        no_err(err, "datagrep_query_status_json");
        if (status && strstr(status, needle)) return status;
        if (status && strstr(status, "\"state\":\"failed\"")) return status;
        datagrep_string_free(status);
        nap_ms(10);
    }
    char *err = NULL;
    char *status = datagrep_query_status_json(q, &err);
    no_err(err, "datagrep_query_status_json");
    return status;
}

/* Run a statement (or script) to completion and report its final status. */
static void run_sync(DatagrepCore *core, const char *profile, const char *sql, const char *label) {
    char *err = NULL;
    DatagrepQuery *q = datagrep_query_run(core, profile, sql, &err);
    no_err(err, label);
    ok(q != NULL, label);
    if (!q) return;
    char *status = await_state(q, "\"state\":\"done\"", 60000);
    printf("       %s -> %s\n", label, status ? status : "(no status)");
    ok(status && strstr(status, "\"state\":\"done\"") != NULL, "  reached state=done");
    datagrep_string_free(status);
    datagrep_query_free(q);
}

int main(void) {
    char profiles_db[512], data_db[512];
    const char *tmp = getenv("DATAGREP_SMOKE_DIR");
    if (!tmp) tmp = "/tmp";
    snprintf(profiles_db, sizeof profiles_db, "%s/profiles.db", tmp);
    snprintf(data_db, sizeof data_db, "%s/smoke.db", tmp);
    remove(profiles_db);
    remove(data_db);

    char *err = NULL;

    /* ---- 1. lifecycle ------------------------------------------------ */
    printf("== 1. core ==\n");
    DatagrepCore *core = datagrep_core_new(profiles_db, &err);
    no_err(err, "datagrep_core_new");
    ok(core != NULL, "datagrep_core_new returned a handle");
    if (!core) return 1;

    /* ---- 2. profiles ------------------------------------------------- */
    printf("== 2. profiles ==\n");
    char url[600];
    snprintf(url, sizeof url, "sqlite://%s", data_db);
    ok(datagrep_profiles_add(core, "smoke", url, &err), "datagrep_profiles_add");
    no_err(err, "datagrep_profiles_add");

    char *list = datagrep_profiles_list_json(core, &err);
    no_err(err, "datagrep_profiles_list_json");
    printf("       %s\n", list ? list : "(null)");
    ok(list && strstr(list, "\"name\":\"smoke\"") != NULL, "the profile is listed");
    ok(list && strstr(list, "\"driver\":\"sqlite\"") != NULL, "with its driver");
    ok(list && strstr(list, "\"has_secret\":false") != NULL, "and no secret");
    datagrep_string_free(list);

    /* An unknown URL scheme is a clean error, not a crash. */
    ok(!datagrep_profiles_add(core, "bogus", "mongodb://h/db", &err), "an unknown URL is refused");
    ok(err != NULL, "  with a message");
    if (err) { printf("       %s\n", err); datagrep_string_free(err); err = NULL; }

    /* ---- 3. build a 10k-row table ------------------------------------ */
    printf("== 3. 10k rows ==\n");
    run_sync(core, "smoke",
             "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, note TEXT);"
             "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<10000) "
             "INSERT INTO t(id,name,note) SELECT x, 'name-'||x, "
             "CASE WHEN x%3=0 THEN NULL WHEN x%7=0 THEN '' ELSE 'note-'||x END FROM c;"
             "SELECT count(*) AS n FROM t",
             "create + insert 10000 rows");

    /* ---- 4. lazy catalog, one level per call ------------------------- */
    printf("== 4. catalog ==\n");
    char *roots = datagrep_catalog_children_json(core, "smoke", "[]", &err);
    no_err(err, "datagrep_catalog_children_json([])");
    printf("       roots:  %s\n", roots ? roots : "(null)");
    ok(roots && strstr(roots, "\"enumeration\":") != NULL, "roots carry an enumeration cost");
    datagrep_string_free(roots);

    char *tables = datagrep_catalog_children_json(core, "smoke", "[\"main\"]", &err);
    no_err(err, "datagrep_catalog_children_json([\"main\"])");
    printf("       main:   %s\n", tables ? tables : "(null)");
    ok(tables && strstr(tables, "\"name\":\"t\"") != NULL, "the table is listed");
    datagrep_string_free(tables);

    char *detail = datagrep_catalog_describe_json(core, "smoke", "[\"main\",\"t\"]", &err);
    no_err(err, "datagrep_catalog_describe_json");
    printf("       detail: %s\n", detail ? detail : "(null)");
    ok(detail && strstr(detail, "\"name\":\"note\"") != NULL, "describe returns columns");
    datagrep_string_free(detail);

    /* ---- 5..7. the hot path ------------------------------------------ */
    printf("== 5. windows ==\n");
    DatagrepQuery *q = datagrep_query_run(core, "smoke", "SELECT id, name, note FROM t ORDER BY id", &err);
    no_err(err, "datagrep_query_run");
    ok(q != NULL, "datagrep_query_run returned a handle immediately");
    if (!q) { datagrep_core_free(core); return 1; }

    _Atomic int ticks = 0;
    datagrep_query_on_progress(q, on_progress, (void *)&ticks);

    char *status = await_state(q, "\"state\":\"done\"", 60000);
    printf("       status: %s\n", status ? status : "(null)");
    ok(status && strstr(status, "\"rows_loaded\":10000") != NULL, "10000 rows loaded");
    ok(status && strstr(status, "\"total_known\":true") != NULL, "total_known once terminal");
    ok(status && strstr(status, "\"columns\":[{\"name\":\"id\"") != NULL, "columns reported");
    datagrep_string_free(status);
    printf("       progress callbacks fired: %d\n", atomic_load(&ticks));
    ok(atomic_load(&ticks) > 0, "the progress callback fired from a background thread");

    char buf[256];

    /* rows [0, 50) */
    DatagrepRows *head = datagrep_query_rows(q, 0, 50, &err);
    no_err(err, "datagrep_query_rows(0,50)");
    ok(head != NULL, "window [0,50) materialised");
    ok(datagrep_rows_count(head) == 50, "  50 rows available");
    ok(datagrep_rows_columns(head) == 3, "  3 columns");
    ok(!datagrep_rows_pending(head), "  not pending");
    printf("       [0]  id=%s", cell(head, 0, 0, buf, sizeof buf));
    printf(" name=%s", cell(head, 0, 1, buf, sizeof buf));
    printf(" note=%s\n", cell(head, 0, 2, buf, sizeof buf));
    ok(strcmp(cell(head, 0, 0, buf, sizeof buf), "1") == 0, "  row 0 id == 1");
    ok(strcmp(cell(head, 0, 1, buf, sizeof buf), "name-1") == 0, "  row 0 name == name-1");

    /* ---- 7. NULL vs empty string vs value ---------------------------- */
    printf("== 6. NULL / empty / value ==\n");
    /* id 3 is row index 2 and has note = NULL (x%3==0). */
    printf("       row 2 (id=3) note kind = %u\n", datagrep_rows_cell_kind(head, 2, 2));
    ok(datagrep_rows_cell_kind(head, 2, 2) == 1, "a SQL NULL reports kind 1");
    /* id 7 is row index 6 and has note = '' (x%7==0, x%3!=0). */
    printf("       row 6 (id=7) note kind = %u, text = \"%s\"\n",
           datagrep_rows_cell_kind(head, 6, 2), cell(head, 6, 2, buf, sizeof buf));
    ok(datagrep_rows_cell_kind(head, 6, 2) == 0, "an empty string reports kind 0, not NULL");
    ok(strlen(cell(head, 6, 2, buf, sizeof buf)) == 0, "  and its text is empty");
    /* id 1 is row index 0 and has a real note. */
    ok(datagrep_rows_cell_kind(head, 0, 2) == 0, "a real value reports kind 0");
    /* Arrow has no ABSENT: a tabular result never reports kind 2 in range. */
    ok(datagrep_rows_cell_kind(head, 999, 0) == 2, "out-of-range reports kind 2 (absent)");

    char *cell_json = datagrep_rows_cell_detail_json(head, 0, 1);
    printf("       detail(0,1) = %s\n", cell_json ? cell_json : "(null)");
    ok(cell_json && strcmp(cell_json, "\"name-1\"") == 0, "cell detail JSON");
    datagrep_string_free(cell_json);

    char *null_json = datagrep_rows_cell_detail_json(head, 2, 2);
    printf("       detail(2,2) = %s\n", null_json ? null_json : "(null)");
    ok(null_json && strcmp(null_json, "null") == 0, "a NULL cell's detail JSON is null");
    datagrep_string_free(null_json);

    datagrep_rows_free(head);

    /* ---- 6. a far window, with nothing in between materialised ------- */
    printf("== 7. far window ==\n");
    DatagrepRows *tail = datagrep_query_rows(q, 9000, 50, &err);
    no_err(err, "datagrep_query_rows(9000,50)");
    ok(tail != NULL, "window [9000,9050) materialised");
    ok(datagrep_rows_count(tail) == 50, "  50 rows available");
    ok(datagrep_rows_columns(tail) == 3, "  3 columns");
    printf("       [9000] id=%s", cell(tail, 0, 0, buf, sizeof buf));
    printf(" name=%s\n", cell(tail, 0, 1, buf, sizeof buf));
    ok(strcmp(cell(tail, 0, 0, buf, sizeof buf), "9001") == 0, "  first row id == 9001");
    ok(strcmp(cell(tail, 49, 0, buf, sizeof buf), "9050") == 0, "  last row id == 9050");
    datagrep_rows_free(tail);
    datagrep_query_free(q);

    /* ---- 8. cancel --------------------------------------------------- */
    printf("== 8. cancel ==\n");
    DatagrepQuery *slow = datagrep_query_run(
        core, "smoke",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<50000000) "
        "SELECT x, 'pad-'||x FROM c",
        &err);
    no_err(err, "datagrep_query_run(slow)");
    ok(slow != NULL, "the long query started");

    /* Let it actually produce rows first, so the cancel is meaningful. */
    char *streaming = await_state(slow, "\"rows_loaded\":", 5000);
    datagrep_string_free(streaming);
    nap_ms(250);
    char *before = datagrep_query_status_json(slow, &err);
    no_err(err, "status before cancel");
    printf("       before: %s\n", before ? before : "(null)");
    /* SQLite can reach the soft row cap in well under a second, and a capped
     * feeder has already closed its cursor (datagrep-core feeder.rs: the cursor is
     * released on every exit path). Cancelling then genuinely cancels nothing,
     * and reporting "cancelled" would be a lie — so what we assert below
     * depends on whether the query was still live when the button was hit. */
    int was_live = before && (strstr(before, "\"state\":\"streaming\"") != NULL ||
                              strstr(before, "\"state\":\"parked\"") != NULL);
    datagrep_string_free(before);

    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    char *outcome = NULL;
    datagrep_query_cancel(slow, &outcome);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    double ms = (t1.tv_sec - t0.tv_sec) * 1000.0 + (t1.tv_nsec - t0.tv_nsec) / 1e6;
    printf("       cancel returned in %.3f ms\n", ms);
    printf("       outcome: %s\n", outcome ? outcome : "(null)");
    ok(ms < 50.0, "the stop button returned instantly");
    ok(outcome != NULL, "an outcome JSON came back");
    ok(outcome && strstr(outcome, "\"local_stopped\":true") != NULL, "  the local half stopped");
    datagrep_string_free(outcome);

    char *after = await_state(slow, was_live ? "\"state\":\"cancelled\"" : "\"state\":\"", 5000);
    printf("       after:  %s\n", after ? after : "(null)");
    if (was_live) {
        ok(after && strstr(after, "\"state\":\"cancelled\"") != NULL,
           "a live query reports cancelled");
    } else {
        /* Already finished before the button was pressed: the terminal phase
         * must be preserved, not overwritten with a cancellation that did not
         * happen. */
        ok(after && (strstr(after, "\"state\":\"capped\"") != NULL ||
                     strstr(after, "\"state\":\"done\"") != NULL),
           "an already-finished query keeps its honest terminal state");
    }
    datagrep_string_free(after);

    /* The server half arrives later; calling cancel again reads it. */
    nap_ms(300);
    char *resolved = NULL;
    datagrep_query_cancel(slow, &resolved);
    printf("       server half: %s\n", resolved ? resolved : "(null)");
    ok(resolved && strstr(resolved, "\"outcome\":") != NULL, "the server's answer is reported");
    datagrep_string_free(resolved);

    /* Rows that did arrive are still readable — a cancel is not a wipe. */
    DatagrepRows *kept = datagrep_query_rows(slow, 0, 5, &err);
    no_err(err, "datagrep_query_rows after cancel");
    printf("       rows kept after cancel: %llu\n", (unsigned long long)datagrep_rows_count(kept));
    ok(datagrep_rows_count(kept) > 0, "the rows that arrived before the stop are kept");
    datagrep_rows_free(kept);
    datagrep_query_free(slow);

    /* ---- teardown ---------------------------------------------------- */
    printf("== 9. teardown ==\n");
    ok(datagrep_profiles_remove(core, "smoke", &err), "datagrep_profiles_remove");
    no_err(err, "datagrep_profiles_remove");
    datagrep_core_free(core);
    ok(true, "datagrep_core_free");

    /* NULL-safety sweep: every entry point tolerates a NULL handle. */
    printf("== 10. NULL safety ==\n");
    datagrep_core_free(NULL);
    datagrep_query_free(NULL);
    datagrep_rows_free(NULL);
    datagrep_string_free(NULL);
    datagrep_query_cancel(NULL, NULL);
    datagrep_query_on_progress(NULL, NULL, NULL);
    ok(datagrep_rows_count(NULL) == 0, "datagrep_rows_count(NULL) == 0");
    ok(datagrep_rows_columns(NULL) == 0, "datagrep_rows_columns(NULL) == 0");
    ok(datagrep_rows_cell(NULL, 0, 0, NULL) == NULL, "datagrep_rows_cell(NULL) == NULL");
    ok(datagrep_rows_cell_detail_json(NULL, 0, 0) == NULL, "datagrep_rows_cell_detail_json(NULL) == NULL");
    err = NULL;
    ok(datagrep_query_rows(NULL, 0, 1, &err) == NULL, "datagrep_query_rows(NULL) == NULL");
    ok(err != NULL, "  and sets err_out");
    datagrep_string_free(err);

    printf("\n%s (%d failure%s)\n", failures ? "SMOKE TEST FAILED" : "SMOKE TEST PASSED",
           failures, failures == 1 ? "" : "s");
    return failures ? 1 : 0;
}
