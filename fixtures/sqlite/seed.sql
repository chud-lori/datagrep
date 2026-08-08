-- fixtures/sqlite/seed.sql — the seeded SQLite benchmark dataset.
--
-- Usage:
--   sqlite3 fixtures/sqlite/bench.db < fixtures/sqlite/seed.sql
--
-- bench_sqlite is the fastest CI signal we have: no server,
-- no container, seeds in low single-digit seconds even at 2M rows (measured
-- locally: 200k rows via this exact WITH RECURSIVE shape seeds in ~0.1s, so
-- 2M is expected well under 5s on CI-class hardware). Prefer this fixture
-- over spinning up fixtures/postgres/ for anything that doesn't specifically
-- need Postgres wire-protocol or catalog behavior.

PRAGMA journal_mode = WAL;
PRAGMA synchronous = OFF;

DROP TABLE IF EXISTS bench_sqlite;
CREATE TABLE bench_sqlite (
  id     INTEGER PRIMARY KEY,
  value  REAL,
  label  TEXT
);

-- WITH RECURSIVE seq(n) generates 1..2,000,000 with no server-side
-- generate_series equivalent needed. SQLite has no recursion-depth limit
-- for WITH RECURSIVE beyond available memory, so this needs no batching.
INSERT INTO bench_sqlite (id, value, label)
WITH RECURSIVE seq(n) AS (
  SELECT 1
  UNION ALL
  SELECT n + 1 FROM seq WHERE n < 2000000
)
SELECT
  n,
  (n * 1.0) / 7.0,
  'row-' || (n % 1000)
FROM seq;

SELECT 'bench_sqlite' AS fixture, count(*) AS rows FROM bench_sqlite;
