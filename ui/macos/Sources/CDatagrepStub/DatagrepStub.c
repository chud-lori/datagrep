/* DatagrepStub.c — a synthetic implementation of the frozen datagrep C ABI.
 *
 * Purpose: let the macOS UI be built, run and MEASURED before crates/datagrep-ffi
 * lands. It generates a fake 1,000,000-row x 24-column result set ON DEMAND —
 * it never materialises the whole table, exactly like the real ResultStore
 * window resolver, so the UI's virtualisation is genuinely exercised rather
 * than hidden behind a pre-built array.
 *
 * Build with:  swift build                  (default; DATAGREP_FFI unset or =stub)
 * Replace with the real static lib:  DATAGREP_FFI=real swift build
 */

#include "datagrep.h"

#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <time.h>

/* ------------------------------------------------------------------ utils */

static char *dup_cstr(const char *s) {
    size_t n = strlen(s) + 1;
    char *p = (char *)malloc(n);
    if (p) memcpy(p, s, n);
    return p;
}

static void set_err(char **err_out, const char *msg) {
    if (err_out) *err_out = dup_cstr(msg);
}

static uint64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000ull + (uint64_t)(ts.tv_nsec / 1000000ull);
}

/* A growable string buffer, so the JSON builders never guess a size. */
typedef struct {
    char  *buf;
    size_t len, cap;
} Sb;

static void sb_init(Sb *s) {
    s->cap = 256;
    s->len = 0;
    s->buf = (char *)malloc(s->cap);
    if (s->buf) s->buf[0] = '\0';
}
static void sb_put(Sb *s, const char *txt) {
    size_t n = strlen(txt);
    if (s->len + n + 1 > s->cap) {
        while (s->len + n + 1 > s->cap) s->cap *= 2;
        s->buf = (char *)realloc(s->buf, s->cap);
    }
    memcpy(s->buf + s->len, txt, n);
    s->len += n;
    s->buf[s->len] = '\0';
}
static void sb_putf(Sb *s, const char *fmt, ...) {
    char tmp[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(tmp, sizeof tmp, fmt, ap);
    va_end(ap);
    sb_put(s, tmp);
}

/* ------------------------------------------------------------- core object */

typedef struct {
    char *name;
    char *driver;
    char *env;
    int   has_secret;
} StubProfile;

struct DatagrepCore {
    pthread_mutex_t lock;
    StubProfile    *profiles;
    size_t          n;
    size_t          cap;
    char           *db_path;
};

static void core_push(DatagrepCore *c, const char *name, const char *driver, const char *env,
                      int has_secret) {
    if (c->n == c->cap) {
        c->cap = c->cap ? c->cap * 2 : 8;
        c->profiles = (StubProfile *)realloc(c->profiles, c->cap * sizeof(StubProfile));
    }
    StubProfile *p = &c->profiles[c->n++];
    p->name = dup_cstr(name);
    p->driver = dup_cstr(driver);
    p->env = dup_cstr(env);
    p->has_secret = has_secret;
}

DatagrepCore *datagrep_core_new(const char *profiles_db_path, char **err_out) {
    if (!profiles_db_path) {
        set_err(err_out, "profiles_db_path is null");
        return NULL;
    }
    DatagrepCore *c = (DatagrepCore *)calloc(1, sizeof(DatagrepCore));
    if (!c) {
        set_err(err_out, "out of memory");
        return NULL;
    }
    pthread_mutex_init(&c->lock, NULL);
    c->db_path = dup_cstr(profiles_db_path);
    /* Four synthetic profiles chosen to exercise every catalog Enumeration. */
    core_push(c, "local_pg", "postgres", "dev", 1);
    core_push(c, "app_sqlite", "sqlite", "dev", 0);
    core_push(c, "sessions_redis", "redis", "prod", 1);
    core_push(c, "events_mongo", "mongo", "staging", 1);
    return c;
}

void datagrep_core_free(DatagrepCore *c) {
    if (!c) return;
    for (size_t i = 0; i < c->n; i++) {
        free(c->profiles[i].name);
        free(c->profiles[i].driver);
        free(c->profiles[i].env);
    }
    free(c->profiles);
    free(c->db_path);
    pthread_mutex_destroy(&c->lock);
    free(c);
}

void datagrep_string_free(char *s) { free(s); }

char *datagrep_profiles_list_json(DatagrepCore *c, char **err_out) {
    if (!c) {
        set_err(err_out, "core is null");
        return NULL;
    }
    Sb s;
    sb_init(&s);
    sb_put(&s, "[");
    pthread_mutex_lock(&c->lock);
    for (size_t i = 0; i < c->n; i++) {
        sb_putf(&s, "%s{\"name\":\"%s\",\"driver\":\"%s\",\"env\":\"%s\",\"has_secret\":%s}",
                i ? "," : "", c->profiles[i].name, c->profiles[i].driver, c->profiles[i].env,
                c->profiles[i].has_secret ? "true" : "false");
    }
    pthread_mutex_unlock(&c->lock);
    sb_put(&s, "]");
    return s.buf;
}

/* Synthetic handshake result. The stub never opens a socket, so it answers
 * with a fixed product/version rather than null: the badge is exactly what
 * this build exists to let us look at without a database in the room. */
char *datagrep_connection_info_json(DatagrepCore *c, const char *name, char **err_out);

static const StubProfile *find_profile(DatagrepCore *c, const char *name) {
    for (size_t i = 0; i < c->n; i++)
        if (strcmp(c->profiles[i].name, name) == 0) return &c->profiles[i];
    return NULL;
}

char *datagrep_connection_info_json(DatagrepCore *c, const char *name, char **err_out) {
    if (!c || !name) {
        set_err(err_out, "null argument");
        return NULL;
    }
    pthread_mutex_lock(&c->lock);
    const StubProfile *p = find_profile(c, name);
    if (!p) {
        pthread_mutex_unlock(&c->lock);
        set_err(err_out, "no such profile");
        return NULL;
    }
    Sb s;
    sb_init(&s);
    sb_putf(&s,
            "{\"profile\":\"%s\",\"driver\":\"%s\",\"database\":\"stub\","
            "\"server\":{\"product\":\"%s\",\"version\":\"0.0.0-stub\"},"
            "\"read_only\":null}",
            p->name, p->driver, p->driver);
    pthread_mutex_unlock(&c->lock);
    return s.buf;
}

bool datagrep_profiles_add(DatagrepCore *c, const char *name, const char *url, char **err_out) {
    if (!c || !name || !url) {
        set_err(err_out, "null argument");
        return false;
    }
    pthread_mutex_lock(&c->lock);
    if (find_profile(c, name)) {
        pthread_mutex_unlock(&c->lock);
        set_err(err_out, "a profile with that name already exists");
        return false;
    }
    const char *driver = "postgres";
    if (strncmp(url, "sqlite:", 7) == 0) driver = "sqlite";
    else if (strncmp(url, "redis:", 6) == 0 || strncmp(url, "rediss:", 7) == 0) driver = "redis";
    else if (strncmp(url, "mongodb", 7) == 0) driver = "mongo";
    /* a password in the URL would be split out to the keychain by the real core */
    int has_secret = strchr(url, '@') != NULL;
    core_push(c, name, driver, "dev", has_secret);
    pthread_mutex_unlock(&c->lock);
    return true;
}

bool datagrep_profiles_remove(DatagrepCore *c, const char *name, char **err_out) {
    if (!c || !name) {
        set_err(err_out, "null argument");
        return false;
    }
    pthread_mutex_lock(&c->lock);
    for (size_t i = 0; i < c->n; i++) {
        if (strcmp(c->profiles[i].name, name) == 0) {
            free(c->profiles[i].name);
            free(c->profiles[i].driver);
            free(c->profiles[i].env);
            memmove(&c->profiles[i], &c->profiles[i + 1], (c->n - i - 1) * sizeof(StubProfile));
            c->n--;
            pthread_mutex_unlock(&c->lock);
            return true;
        }
    }
    pthread_mutex_unlock(&c->lock);
    set_err(err_out, "no such profile");
    return false;
}

/* ----------------------------------------------------------------- catalog */

/* Minimal JSON-array-of-strings parser; enough for path_json. */
#define MAX_SEG 8
typedef struct {
    char   seg[MAX_SEG][128];
    size_t n;
} Path;

static void parse_path(const char *json, Path *out) {
    out->n = 0;
    if (!json) return;
    const char *p = json;
    while (*p && *p != '[') p++;
    if (!*p) return;
    p++;
    while (*p) {
        while (*p == ' ' || *p == ',' || *p == '\t' || *p == '\n') p++;
        if (*p == ']' || *p == '\0') break;
        if (*p != '"') { p++; continue; }
        p++;
        size_t k = 0;
        while (*p && *p != '"' && k < sizeof(out->seg[0]) - 1) {
            if (*p == '\\' && p[1]) p++;
            out->seg[out->n][k++] = *p++;
        }
        out->seg[out->n][k] = '\0';
        if (*p == '"') p++;
        if (out->n + 1 < MAX_SEG) out->n++;
    }
}

static void node(Sb *s, int first, const char *name, const char *kind, int has_children,
                 const char *enumeration) {
    sb_putf(s, "%s{\"name\":\"%s\",\"kind\":\"%s\",\"has_children\":%s,\"enumeration\":\"%s\"}",
            first ? "" : ",", name, kind, has_children ? "true" : "false", enumeration);
}

static const char *TABLES[] = {"users",    "orders",  "order_items", "payments", "sessions",
                               "events",   "invoices", "addresses",  "products", "shipments",
                               "audit_log", "feature_flags"};
static const size_t N_TABLES = sizeof(TABLES) / sizeof(TABLES[0]);

static const char *COLS[24] = {
    "id",       "created_at", "updated_at", "name",     "email",      "status",
    "country",  "score",      "balance",    "attempts", "note",       "tags",
    "session_id","ip",        "user_agent", "referrer", "region",     "plan",
    "seats",    "trial",      "churn_risk", "last_seen","metadata",   "checksum"};
static const char *COL_TYPES[24] = {
    "bigint",  "timestamptz", "timestamptz", "text",   "text",        "text",
    "text",    "double",      "numeric",     "int",    "text",        "jsonb",
    "uuid",    "inet",        "text",        "text",   "text",        "text",
    "int",     "boolean",     "double",      "timestamptz", "jsonb",  "bytea"};

char *datagrep_catalog_children_json(DatagrepCore *c, const char *profile, const char *path_json,
                                char **err_out) {
    if (!c || !profile) {
        set_err(err_out, "null argument");
        return NULL;
    }
    pthread_mutex_lock(&c->lock);
    const StubProfile *p = find_profile(c, profile);
    char driver[32] = {0};
    if (p) snprintf(driver, sizeof driver, "%s", p->driver);
    pthread_mutex_unlock(&c->lock);
    if (!p) {
        set_err(err_out, "no such profile");
        return NULL;
    }

    Path path;
    parse_path(path_json, &path);

    Sb s;
    sb_init(&s);
    sb_put(&s, "[");

    if (strcmp(driver, "redis") == 0) {
        if (path.n == 0) {
            for (int i = 0; i < 4; i++) {
                char nm[16];
                snprintf(nm, sizeof nm, "db%d", i);
                /* ScanOnly { requires_prefix: true } — the UI must NOT auto-expand
                   this, or we have just fired KEYS * at a 40 GB Redis. */
                node(&s, i == 0, nm, "database", 1, "scan_only");
            }
        } else if (path.n == 1) {
            /* No prefix supplied: refuse to enumerate. Empty, not an error. */
        } else {
            const char *prefix = path.seg[path.n - 1];
            for (int i = 0; i < 25; i++) {
                char nm[192];
                snprintf(nm, sizeof nm, "%s%d", prefix, 1000 + i * 7);
                node(&s, i == 0, nm, i % 3 == 0 ? "hash" : "string", 0, "on_demand");
            }
        }
    } else if (strcmp(driver, "mongo") == 0) {
        if (path.n == 0) {
            node(&s, 1, "events", "database", 1, "cheap");
            node(&s, 0, "analytics", "database", 1, "cheap");
            node(&s, 0, "archive_2019", "database", 1, "paged");
        } else if (path.n == 1) {
            const char *cols[] = {"clickstream", "impressions", "sessions", "users_raw"};
            for (int i = 0; i < 4; i++) node(&s, i == 0, cols[i], "collection", 1, "cheap");
        } else if (path.n == 2) {
            for (int i = 0; i < 12; i++)
                node(&s, i == 0, COLS[i], "field", 0, "on_demand");
        }
    } else { /* postgres / sqlite */
        if (path.n == 0) {
            node(&s, 1, "public", "schema", 1, "cheap");
            node(&s, 0, "analytics", "schema", 1, "cheap");
            node(&s, 0, "pg_catalog", "schema", 1, "paged");
        } else if (path.n == 1) {
            for (size_t i = 0; i < N_TABLES; i++)
                node(&s, i == 0, TABLES[i], "table", 1, "cheap");
        } else if (path.n == 2) {
            for (int i = 0; i < 24; i++) node(&s, i == 0, COLS[i], "column", 0, "on_demand");
        }
    }

    sb_put(&s, "]");
    return s.buf;
}

char *datagrep_catalog_describe_json(DatagrepCore *c, const char *profile, const char *path_json,
                                char **err_out) {
    if (!c || !profile) {
        set_err(err_out, "null argument");
        return NULL;
    }
    Path path;
    parse_path(path_json, &path);
    if (path.n == 0) {
        set_err(err_out, "describe requires a path");
        return NULL;
    }
    Sb s;
    sb_init(&s);
    sb_putf(&s, "{\"name\":\"%s\",\"kind\":\"table\",\"columns\":[", path.seg[path.n - 1]);
    for (int i = 0; i < 24; i++)
        sb_putf(&s, "%s{\"name\":\"%s\",\"type\":\"%s\",\"nullable\":%s}", i ? "," : "", COLS[i],
                COL_TYPES[i], (i == 10 || i == 15) ? "true" : "false");
    sb_put(&s, "],\"primary_key\":[\"id\"],\"approx_rows\":1000000}");
    return s.buf;
}

/* ------------------------------------------------------------------- query */

enum { ST_STREAMING = 0, ST_PARKED, ST_CAPPED, ST_DONE, ST_CANCELLED, ST_FAILED };
static const char *STATE_NAMES[] = {"streaming", "parked", "capped",
                                    "done",      "cancelled", "failed"};

struct DatagrepQuery {
    pthread_mutex_t lock;
    pthread_t       thread;
    int             has_thread;
    int             cancel_flag;
    int             state;
    uint64_t        rows_loaded;
    uint64_t        total_rows;
    uint32_t        ncols;
    uint64_t        started_ms;
    uint64_t        finished_ms;
    char           *error;
    DatagrepProgressFn   cb;
    void           *cb_ctx;
};

static int sql_is_select(const char *sql) {
    while (*sql == ' ' || *sql == '\n' || *sql == '\t' || *sql == '\r') sql++;
    /* skip leading line comments (block directives live there) */
    while (sql[0] == '-' && sql[1] == '-') {
        while (*sql && *sql != '\n') sql++;
        while (*sql == ' ' || *sql == '\n' || *sql == '\t' || *sql == '\r') sql++;
    }
    return strncasecmp(sql, "select", 6) == 0 || strncasecmp(sql, "with", 4) == 0 ||
           strncasecmp(sql, "show", 4) == 0 || strncasecmp(sql, "explain", 7) == 0;
}

static uint64_t sql_row_target(const char *sql) {
    /* honour a trailing LIMIT n and the `-- @limit n` block directive so the
       UI's directive plumbing has something real to show */
    uint64_t best = 1000000;
    const char *p = sql;
    while ((p = strcasestr(p, "limit")) != NULL) {
        p += 5;
        while (*p == ' ' || *p == '\t') p++;
        uint64_t v = 0;
        int digits = 0;
        while (*p >= '0' && *p <= '9') {
            v = v * 10 + (uint64_t)(*p - '0');
            p++;
            digits++;
        }
        if (digits && v < best) best = v;
    }
    return best;
}

static void *stream_thread(void *arg) {
    DatagrepQuery *q = (DatagrepQuery *)arg;
    const int TICKS = 24;
    for (int t = 1; t <= TICKS; t++) {
        /* 40 ms per tick, in 5 ms slices so cancel is observed promptly */
        for (int k = 0; k < 8; k++) {
            struct timespec ts = {0, 5 * 1000 * 1000};
            nanosleep(&ts, NULL);
            pthread_mutex_lock(&q->lock);
            int cancelled = q->cancel_flag;
            pthread_mutex_unlock(&q->lock);
            if (cancelled) goto out;
        }
        pthread_mutex_lock(&q->lock);
        q->rows_loaded = q->total_rows * (uint64_t)t / (uint64_t)TICKS;
        q->finished_ms = now_ms();
        DatagrepProgressFn cb = q->cb;
        void *ctx = q->cb_ctx;
        pthread_mutex_unlock(&q->lock);
        if (cb) cb(ctx); /* BACKGROUND THREAD — the Swift side hops to main */
    }
    pthread_mutex_lock(&q->lock);
    if (!q->cancel_flag) {
        q->rows_loaded = q->total_rows;
        q->state = ST_DONE;
        q->finished_ms = now_ms();
    }
    pthread_mutex_unlock(&q->lock);
out : {
    pthread_mutex_lock(&q->lock);
    DatagrepProgressFn cb = q->cb;
    void *ctx = q->cb_ctx;
    pthread_mutex_unlock(&q->lock);
    if (cb) cb(ctx);
}
    return NULL;
}

DatagrepQuery *datagrep_query_run(DatagrepCore *c, const char *profile, const char *sql, char **err_out) {
    if (!c || !profile || !sql) {
        set_err(err_out, "null argument");
        return NULL;
    }
    pthread_mutex_lock(&c->lock);
    const StubProfile *p = find_profile(c, profile);
    pthread_mutex_unlock(&c->lock);
    if (!p) {
        set_err(err_out, "no such profile");
        return NULL;
    }
    if (strstr(sql, "@@fail") != NULL) {
        set_err(err_out, "syntax error at or near \"@@fail\" (stub)");
        return NULL;
    }

    DatagrepQuery *q = (DatagrepQuery *)calloc(1, sizeof(DatagrepQuery));
    pthread_mutex_init(&q->lock, NULL);
    q->started_ms = now_ms();
    q->finished_ms = q->started_ms;
    q->state = ST_STREAMING;

    if (!sql_is_select(sql)) {
        /* Mirrors datagrep-cli README gap #3: Shape::Ack never reaches the frontend,
           so a DDL/DML statement is indistinguishable from an empty SELECT. */
        q->total_rows = 0;
        q->ncols = 0;
        q->state = ST_DONE;
        return q;
    }

    q->total_rows = sql_row_target(sql);
    q->ncols = 24;
    if (pthread_create(&q->thread, NULL, stream_thread, q) == 0) q->has_thread = 1;
    else {
        q->rows_loaded = q->total_rows;
        q->state = ST_DONE;
    }
    return q;
}

void datagrep_query_free(DatagrepQuery *q) {
    if (!q) return;
    pthread_mutex_lock(&q->lock);
    q->cancel_flag = 1;
    q->cb = NULL; /* no callback may fire after free begins */
    pthread_mutex_unlock(&q->lock);
    if (q->has_thread) pthread_join(q->thread, NULL);
    free(q->error);
    pthread_mutex_destroy(&q->lock);
    free(q);
}

void datagrep_query_cancel(DatagrepQuery *q, char **outcome_json_out) {
    if (!q) return;
    pthread_mutex_lock(&q->lock);
    int was_running = (q->state == ST_STREAMING || q->state == ST_PARKED);
    q->cancel_flag = 1;
    if (was_running) q->state = ST_CANCELLED;
    q->finished_ms = now_ms();
    uint64_t rows = q->rows_loaded;
    pthread_mutex_unlock(&q->lock);
    /* Returns instantly: we do NOT join the feeder here, because the stop
       button must always return control immediately. */
    if (outcome_json_out) {
        Sb s;
        sb_init(&s);
        sb_putf(&s,
                "{\"kind\":\"ClientAbandon\",\"rows_kept\":%llu,\"message\":\"stopped receiving "
                "results; the server may still be executing this query\"}",
                (unsigned long long)rows);
        *outcome_json_out = s.buf;
    }
}

char *datagrep_query_status_json(DatagrepQuery *q, char **err_out) {
    if (!q) {
        set_err(err_out, "query is null");
        return NULL;
    }
    pthread_mutex_lock(&q->lock);
    int state = q->state;
    uint64_t rows = q->rows_loaded;
    uint64_t elapsed = q->finished_ms - q->started_ms;
    uint32_t ncols = q->ncols;
    char *err = q->error ? dup_cstr(q->error) : NULL;
    pthread_mutex_unlock(&q->lock);

    Sb s;
    sb_init(&s);
    sb_putf(&s, "{\"state\":\"%s\",\"rows_loaded\":%llu,\"elapsed_ms\":%llu,",
            STATE_NAMES[state], (unsigned long long)rows, (unsigned long long)elapsed);
    if (err) {
        sb_putf(&s, "\"error\":\"%s\",", err);
        free(err);
    } else {
        sb_put(&s, "\"error\":null,");
    }
    sb_put(&s, "\"columns\":[");
    for (uint32_t i = 0; i < ncols; i++)
        sb_putf(&s, "%s{\"name\":\"%s\",\"type\":\"%s\"}", i ? "," : "", COLS[i], COL_TYPES[i]);
    sb_putf(&s, "],\"total_known\":%s}", state == ST_DONE ? "true" : "false");
    return s.buf;
}

void datagrep_query_on_progress(DatagrepQuery *q, DatagrepProgressFn cb, void *ctx) {
    if (!q) return;
    pthread_mutex_lock(&q->lock);
    q->cb = cb;
    q->cb_ctx = ctx;
    pthread_mutex_unlock(&q->lock);
}

/* -------------------------------------------------------------------- rows */

struct DatagrepRows {
    uint64_t  count;
    uint32_t  cols;
    int       pending;
    uint64_t  base_row;
    char     *arena;      /* all cell bytes, packed, NOT nul-terminated */
    uint32_t *off;        /* count*cols */
    uint32_t *len;        /* count*cols */
    uint8_t  *kind;       /* count*cols */
};

static const char *STATUSES[] = {"active", "trial", "churned", "paused"};
static const char *COUNTRIES[] = {"SG", "ID", "MY", "TH", "VN", "PH", "JP", "AU"};
static const char *PLANS[] = {"free", "pro", "team", "enterprise"};
static const char *REGIONS[] = {"ap-southeast-1", "ap-southeast-3", "us-east-1", ""};

/* Deterministic per-(row,col) text. Returns bytes written into `out`. */
static int gen_cell(uint64_t r, uint32_t c, char *out, size_t cap, uint8_t *kind) {
    *kind = 0;
    switch (c) {
    case 0: return snprintf(out, cap, "%llu", (unsigned long long)(r + 1));
    case 1:
        return snprintf(out, cap, "2026-%02llu-%02llu 0%llu:%02llu:%02llu+08",
                        (unsigned long long)(r % 12 + 1), (unsigned long long)(r % 28 + 1),
                        (unsigned long long)(r % 9), (unsigned long long)(r % 60),
                        (unsigned long long)((r * 7) % 60));
    case 2:
        return snprintf(out, cap, "2026-%02llu-%02llu 1%llu:%02llu:00+08",
                        (unsigned long long)((r + 3) % 12 + 1),
                        (unsigned long long)((r + 5) % 28 + 1), (unsigned long long)(r % 4),
                        (unsigned long long)((r * 3) % 60));
    case 3: return snprintf(out, cap, "customer_%06llu", (unsigned long long)(r % 999983));
    case 4:
        return snprintf(out, cap, "user%llu@example.%s", (unsigned long long)r,
                        (r % 3) ? "com" : "co");
    case 5: return snprintf(out, cap, "%s", STATUSES[r % 4]);
    case 6: return snprintf(out, cap, "%s", COUNTRIES[r % 8]);
    case 7: return snprintf(out, cap, "%llu.%03llu", (unsigned long long)(r % 100),
                            (unsigned long long)(r % 1000));
    case 8: return snprintf(out, cap, "%llu.%02llu", (unsigned long long)((r * 13) % 250000),
                            (unsigned long long)(r % 100));
    case 9: return snprintf(out, cap, "%llu", (unsigned long long)(r % 17));
    case 10: /* note — genuinely NULL every 7th row */
        if (r % 7 == 0) { *kind = 1; return 0; }
        return snprintf(out, cap, "follow-up #%llu", (unsigned long long)(r % 421));
    case 11: /* tags — nested array */
        *kind = 3;
        return snprintf(out, cap, "[%llu items]", (unsigned long long)(r % 4 + 1));
    case 12:
        return snprintf(out, cap, "%08llx-1c4d-4a%02llx-9f31-%012llx",
                        (unsigned long long)(r * 2654435761ull & 0xffffffffull),
                        (unsigned long long)(r % 256), (unsigned long long)(r * 1000003ull));
    case 13:
        return snprintf(out, cap, "10.%llu.%llu.%llu", (unsigned long long)(r % 250),
                        (unsigned long long)((r / 250) % 250), (unsigned long long)(r % 199 + 1));
    case 14:
        return snprintf(out, cap, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) build/%llu",
                        (unsigned long long)(r % 9999));
    case 15: /* referrer — ABSENT every 5th row (field not present in the document) */
        if (r % 5 == 0) { *kind = 2; return 0; }
        return snprintf(out, cap, "https://ref.example/%llu", (unsigned long long)(r % 733));
    case 16: /* region — a genuinely EMPTY string every 11th row */
        return snprintf(out, cap, "%s", (r % 11 == 0) ? "" : REGIONS[r % 3]);
    case 17: return snprintf(out, cap, "%s", PLANS[r % 4]);
    case 18: return snprintf(out, cap, "%llu", (unsigned long long)(r % 250 + 1));
    case 19: return snprintf(out, cap, "%s", (r % 3 == 0) ? "true" : "false");
    case 20: return snprintf(out, cap, "0.%03llu", (unsigned long long)(r % 1000));
    case 21:
        return snprintf(out, cap, "2026-07-%02llu 09:%02llu:00+08",
                        (unsigned long long)(r % 30 + 1), (unsigned long long)(r % 60));
    case 22: /* metadata — nested document */
        *kind = 3;
        return snprintf(out, cap, "{%llu fields}", (unsigned long long)(r % 3 + 3));
    case 23:
        return snprintf(out, cap, "\\x%08llx", (unsigned long long)(r * 2246822519ull & 0xffffffff));
    default: return snprintf(out, cap, "-");
    }
}

DatagrepRows *datagrep_query_rows(DatagrepQuery *q, uint64_t offset, uint64_t len, char **err_out) {
    if (!q) {
        set_err(err_out, "query is null");
        return NULL;
    }
    pthread_mutex_lock(&q->lock);
    uint64_t loaded = q->rows_loaded;
    uint32_t ncols = q->ncols;
    pthread_mutex_unlock(&q->lock);

    uint64_t avail = (offset < loaded) ? (loaded - offset) : 0;
    uint64_t count = len < avail ? len : avail;

    DatagrepRows *rw = (DatagrepRows *)calloc(1, sizeof(DatagrepRows));
    rw->count = count;
    rw->cols = ncols;
    rw->base_row = offset;
    rw->pending = (count < len) ? 1 : 0;

    size_t cells = (size_t)count * (size_t)ncols;
    if (cells == 0) return rw;

    rw->off = (uint32_t *)malloc(cells * sizeof(uint32_t));
    rw->len = (uint32_t *)malloc(cells * sizeof(uint32_t));
    rw->kind = (uint8_t *)malloc(cells);
    size_t cap = cells * 24 + 64;
    rw->arena = (char *)malloc(cap);
    size_t used = 0;

    char tmp[256];
    for (uint64_t r = 0; r < count; r++) {
        for (uint32_t c = 0; c < ncols; c++) {
            uint8_t kind = 0;
            int n = gen_cell(offset + r, c, tmp, sizeof tmp, &kind);
            if (n < 0) n = 0;
            if ((size_t)n > sizeof tmp - 1) n = (int)sizeof tmp - 1;
            if (used + (size_t)n > cap) {
                cap = (used + (size_t)n) * 2;
                rw->arena = (char *)realloc(rw->arena, cap);
            }
            size_t idx = (size_t)r * ncols + c;
            rw->off[idx] = (uint32_t)used;
            rw->len[idx] = (uint32_t)n;
            rw->kind[idx] = kind;
            /* packed with NO separator and NO terminator — the Swift side must
               honour len_out, which is exactly what we want to prove */
            memcpy(rw->arena + used, tmp, (size_t)n);
            used += (size_t)n;
        }
    }
    return rw;
}

void datagrep_rows_free(DatagrepRows *r) {
    if (!r) return;
    free(r->arena);
    free(r->off);
    free(r->len);
    free(r->kind);
    free(r);
}

uint64_t datagrep_rows_count(DatagrepRows *r) { return r ? r->count : 0; }
uint32_t datagrep_rows_columns(DatagrepRows *r) { return r ? r->cols : 0; }
bool     datagrep_rows_pending(DatagrepRows *r) { return r ? (r->pending != 0) : false; }

const char *datagrep_rows_cell(DatagrepRows *r, uint64_t row, uint32_t col, size_t *len_out) {
    if (len_out) *len_out = 0;
    if (!r || row >= r->count || col >= r->cols) return NULL;
    size_t idx = (size_t)row * r->cols + col;
    if (len_out) *len_out = r->len[idx];
    return r->arena + r->off[idx];
}

uint8_t datagrep_rows_cell_kind(DatagrepRows *r, uint64_t row, uint32_t col) {
    if (!r || row >= r->count || col >= r->cols) return 2;
    return r->kind[(size_t)row * r->cols + col];
}

char *datagrep_rows_cell_detail_json(DatagrepRows *r, uint64_t row, uint32_t col) {
    if (!r || row >= r->count || col >= r->cols) return NULL;
    uint64_t abs = r->base_row + row;
    Sb s;
    sb_init(&s);
    if (col == 11) {
        sb_put(&s, "[");
        for (uint64_t i = 0; i <= abs % 4; i++)
            sb_putf(&s, "%s\"tag_%llu\"", i ? "," : "", (unsigned long long)((abs + i) % 97));
        sb_put(&s, "]");
    } else if (col == 22) {
        sb_putf(&s,
                "{\"source\":\"%s\",\"campaign_id\":%llu,\"ab_bucket\":\"%c\","
                "\"device\":{\"os\":\"macOS\",\"version\":\"15.%llu\"}",
                (abs % 2) ? "organic" : "paid", (unsigned long long)(abs % 5000),
                (char)('A' + (int)(abs % 4)), (unsigned long long)(abs % 7));
        if (abs % 3 >= 1) sb_putf(&s, ",\"experiment\":\"exp_%llu\"", (unsigned long long)(abs % 31));
        if (abs % 3 >= 2) sb_put(&s, ",\"notes\":null");
        sb_put(&s, "}");
    } else {
        size_t l = 0;
        const char *p = datagrep_rows_cell(r, row, col, &l);
        sb_put(&s, "\"");
        for (size_t i = 0; i < l; i++) {
            char ch[2] = {p[i], 0};
            if (p[i] == '"' || p[i] == '\\') sb_put(&s, "\\");
            sb_put(&s, ch);
        }
        sb_put(&s, "\"");
    }
    return s.buf;
}

/* ------------------------------------------------------------------ writes */

/* The stub generates a synthetic table, not documents: its rows have no
 * envelope, and there is no server behind them to write to. Both calls say so
 * the same way the real engine says "this result is not editable" — a NULL
 * envelope, and a refusal carrying a reason — rather than by being absent from
 * the link, which would be a build failure instead of a message. The UI gates
 * editing on the status JSON's "editable" block, which this stub never emits,
 * so neither is reached in a stub build. */

char *datagrep_rows_column_names_json(DatagrepRows *r) {
    if (!r) return NULL;
    Sb s;
    sb_init(&s);
    sb_put(&s, "[");
    for (uint32_t i = 0; i < r->cols; i++) sb_putf(&s, "%s\"%s\"", i ? "," : "", COLS[i]);
    sb_put(&s, "]");
    return s.buf;
}

char *datagrep_rows_envelope_json(DatagrepRows *r, uint64_t row) {
    (void)r;
    (void)row;
    return NULL;
}

char *datagrep_mutate(DatagrepCore *c, const char *profile, const char *mutation_json,
                      char **err_out) {
    (void)c;
    (void)profile;
    (void)mutation_json;
    set_err(err_out, "this build has no datagrep engine linked in, so nothing can be written");
    return NULL;
}

char *datagrep_reread_documents(DatagrepCore *c, const char *profile, const char *addresses_json,
                                char **err_out) {
    (void)c;
    (void)profile;
    (void)addresses_json;
    set_err(err_out,
            "this build has no datagrep engine linked in, so there is no server to read from");
    return NULL;
}
