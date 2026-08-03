-- fixtures/postgres/seed.sql — the §6 seeded Postgres datasets.
--
-- Runs automatically on first container boot via docker-entrypoint-initdb.d
-- (see docker-compose.yml), or manually:
--   psql "$DBX_FIXTURES_URL" -f fixtures/postgres/seed.sql
--
-- Deterministic: setseed(0.42) is called once per session-scoped block that
-- uses random(), matching every seeded table to the same PRNG stream run to
-- run. generate_series-derived columns need no seed — they're pure
-- functions of the row number.
--
-- Expected seed timings and the "bake this into an image" note: see
-- fixtures/README.md. Short version: bench_narrow's 10M-row INSERT is most
-- of the wall-clock cost; do not run this fixture set on every CI job.

SET client_min_messages = warning;

-- ===========================================================================
-- bench_wide — 1,000,000 rows x 24 mixed-type columns (the headline fixture,
-- ~1.2 GB on the wire per design §6). Exercises every Value variant dbx-api
-- needs to round-trip: ints of three widths, numeric, float, bool, text,
-- timestamptz/timestamp/date/time/interval, array, jsonb, uuid, inet, bytea.
-- ===========================================================================

SELECT setseed(0.42);

DROP TABLE IF EXISTS bench_wide;
CREATE TABLE bench_wide (
  id            bigint PRIMARY KEY,
  small_int     smallint,
  big_int       bigint,
  amount        numeric(12,2),
  ratio         double precision,
  is_active     boolean,
  status        text,
  country       text,
  category      text,
  description   text,
  created_at    timestamptz,
  updated_at    timestamp,
  event_date    date,
  event_time    time,
  duration      interval,
  tags          text[],
  metadata      jsonb,
  external_id   uuid,
  ip_addr       inet,
  score         real,
  weight        numeric(8,4),
  notes         text,
  code          char(8),
  raw_bytes     bytea
);

INSERT INTO bench_wide
SELECT
  g                                                                  AS id,
  (g % 32767)::smallint                                              AS small_int,
  (g * 1000)::bigint                                                 AS big_int,
  round((random() * 100000)::numeric, 2)                             AS amount,
  random()                                                           AS ratio,
  (g % 2 = 0)                                                        AS is_active,
  (ARRAY['pending','active','archived','deleted'])[1 + (g % 4)]      AS status,
  (ARRAY['US','SG','GB','DE','JP','AU','IN','BR'])[1 + (g % 8)]      AS country,
  (ARRAY['a','b','c','d','e'])[1 + (g % 5)]                          AS category,
  'row ' || g || ' description text with some words to pad length'  AS description,
  timestamptz '2020-01-01' + (g || ' seconds')::interval             AS created_at,
  timestamp '2020-01-01' + (g || ' seconds')::interval               AS updated_at,
  date '2020-01-01' + (g % 3650)                                     AS event_date,
  time '00:00:00' + (g % 86400 || ' seconds')::interval              AS event_time,
  (g || ' seconds')::interval                                        AS duration,
  ARRAY['t' || (g % 10), 't' || (g % 7)]                             AS tags,
  jsonb_build_object('id', g, 'active', (g % 2 = 0), 'score', random()) AS metadata,
  md5(g::text)::uuid                                                 AS external_id,
  ('10.' || (g % 256) || '.' || ((g / 256) % 256) || '.' || ((g / 65536) % 256))::inet AS ip_addr,
  (random() * 1000)::real                                            AS score,
  round((random() * 10)::numeric, 4)                                 AS weight,
  repeat('x', 32)                                                    AS notes,
  lpad(g::text, 8, '0')                                              AS code,
  decode(md5(g::text), 'hex')                                        AS raw_bytes
FROM generate_series(1, 1000000) AS g;

ANALYZE bench_wide;

-- ===========================================================================
-- bench_narrow — 10,000,000 rows x 3 columns. Cheap per-row, expensive in
-- aggregate: this is the fixture that dominates seed wall-clock time and the
-- one that exercises sustained streaming throughput (§5.1 "never buffer").
-- ===========================================================================

DROP TABLE IF EXISTS bench_narrow;
CREATE TABLE bench_narrow (
  id     bigint PRIMARY KEY,
  value  double precision,
  label  text
);

INSERT INTO bench_narrow
SELECT
  g,
  random() * 1000,
  'label-' || (g % 1000)
FROM generate_series(1, 10000000) AS g;

ANALYZE bench_narrow;

-- ===========================================================================
-- bench_lowcard — 1,000,000 rows x 6 text columns, each with <=50 distinct
-- values (well under the design's <10% cardinality dictionary-encoding
-- threshold at 1M rows: 50/1,000,000 = 0.005%). Proves dictionary encoding
-- actually fires, not just that low-cardinality data exists.
-- ===========================================================================

DROP TABLE IF EXISTS bench_lowcard;
CREATE TABLE bench_lowcard (
  id     bigint PRIMARY KEY,
  col_a  text,  -- 10 distinct values
  col_b  text,  -- 20 distinct values
  col_c  text,  -- 30 distinct values
  col_d  text,  -- 40 distinct values
  col_e  text,  -- 50 distinct values
  col_f  text   -- 15 distinct values
);

INSERT INTO bench_lowcard
SELECT
  g,
  'a-val-' || (g % 10),
  'b-val-' || (g % 20),
  'c-val-' || (g % 30),
  'd-val-' || (g % 40),
  'e-val-' || (g % 50),
  'f-val-' || (g % 15)
FROM generate_series(1, 1000000) AS g;

ANALYZE bench_lowcard;

-- ===========================================================================
-- bench_catalog — 200 schemas x 500 tables = 100,000 relations. Proves
-- catalog introspection stays O(1) query, not O(schema size) (design §5.2:
-- "full-catalog introspection on connect" is banned outright), and backs
-- the M1 exit criterion "100k-relation catalog connect <400ms".
--
-- The loop: COMMIT after every schema (500 CREATE TABLEs) rather than once
-- at the end. A single 100k-table transaction needs >100k locks held
-- simultaneously (Postgres never releases a DDL lock before commit) and
-- risks "out of shared memory, increase max_locks_per_transaction" — see
-- docker-compose.yml's max_locks_per_transaction=4096 (x 100 connections =
-- 409,600 slots) for the belt-and-suspenders fix. Committing every ~500
-- tables keeps the in-flight lock count trivial regardless of that setting,
-- and IF NOT EXISTS on both CREATE statements makes a partial/retried run
-- idempotent.
-- ===========================================================================

DO $$
DECLARE
  s int;
  t int;
BEGIN
  FOR s IN 1..200 LOOP
    EXECUTE format('CREATE SCHEMA IF NOT EXISTS bench_catalog_s%s', s);
    FOR t IN 1..500 LOOP
      EXECUTE format(
        'CREATE TABLE IF NOT EXISTS bench_catalog_s%1$s.t%2$s (id bigint PRIMARY KEY, val text)',
        s, t
      );
    END LOOP;
    COMMIT;
  END LOOP;
END $$;

-- ===========================================================================
-- bench_slow — a set-returning function + view for P17 (cancel) testing.
-- Each row costs delay_ms of pg_sleep, so a client can start streaming,
-- cancel mid-stream, and assert the connection is usable again within the
-- P17 budget (<=100ms target / 400ms fail) rather than one single
-- pg_sleep(N) that can only prove "cancel works" at one fixed offset.
-- ===========================================================================

CREATE OR REPLACE FUNCTION bench_slow_rows(n int DEFAULT 1000000, delay_ms int DEFAULT 5)
RETURNS TABLE(i int) AS $$
BEGIN
  FOR i IN 1..n LOOP
    PERFORM pg_sleep(delay_ms / 1000.0);
    RETURN NEXT;
  END LOOP;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE VIEW bench_slow AS
SELECT i FROM bench_slow_rows();

-- ===========================================================================
-- bench_hostile — one row, one 10 MB text value. Proves graceful truncation
-- in the grid/preview path, not an OOM (design §5.2 banned-anti-patterns:
-- "Loading the full result before showing row 1"). repeat('x', N) is exactly
-- N bytes since 'x' is single-byte ASCII.
-- ===========================================================================

DROP TABLE IF EXISTS bench_hostile;
CREATE TABLE bench_hostile (
  id    bigint PRIMARY KEY,
  huge  text
);

INSERT INTO bench_hostile VALUES (1, repeat('x', 10 * 1024 * 1024));

-- ===========================================================================
-- Done. Sanity totals for anyone eyeballing the seed log.
-- ===========================================================================

SELECT 'bench_wide'    AS fixture, count(*) AS rows FROM bench_wide
UNION ALL
SELECT 'bench_narrow',              count(*) FROM bench_narrow
UNION ALL
SELECT 'bench_lowcard',             count(*) FROM bench_lowcard
UNION ALL
SELECT 'bench_hostile',             count(*) FROM bench_hostile
UNION ALL
SELECT 'bench_catalog (relations)', count(*) FROM information_schema.tables
  WHERE table_schema LIKE 'bench_catalog_s%';
