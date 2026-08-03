# Integration tests

These tests talk to a real Postgres server and are `#[ignore]`d by default so
`cargo test` stays hermetic. Start a throwaway server:

```sh
docker run --rm -d --name dbx-pg-test -p 5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:16
```

Then run:

```sh
DBX_TEST_PG=1 cargo test -p dbx-drv-postgres --test integration -- --ignored --test-threads=1
```

(`--test-threads=1` because several tests create/drop the same scratch
schemas/tables.)

Connection defaults to `localhost:5432`, user `postgres`, database
`postgres`, no password/TLS. Override with `DBX_TEST_PG_HOST`,
`DBX_TEST_PG_PORT`, `DBX_TEST_PG_USER`, `DBX_TEST_PG_PASSWORD`,
`DBX_TEST_PG_DB` if pointing at something else.

Tear down:

```sh
docker stop dbx-pg-test
```

## What each test proves

- `streams_100k_rows_first_batch_arrives_fast` — design §3.2's "chunk 1
  renders before chunk 2 is requested": the first 500-row batch of a
  100k-row `generate_series` must land in a small fraction of the time the
  whole stream takes, which is only possible if the driver is genuinely
  pulling from a portal rather than materializing the result first.
- `streaming_does_not_retain_the_whole_result_set` — a best-effort RSS check
  (via `/proc/self/status` or `ps`) that process memory doesn't grow by
  anything like the size of the result set while streaming it.
- `numeric_round_trips_as_decimal_string` — design risk #4: `0.1::numeric`
  must come back as the exact string `"0.1"`, not an f64-tainted
  approximation.
- `cancel_mid_sleep_leaves_connection_usable` — design §3.3: cancelling a
  `pg_sleep(5)` reports `CancelOutcome::Requested` (never a false
  `ServerCancelled` — Postgres's protocol gives no ack) and the connection
  is still usable for a follow-up query afterward.
- `catalog_children_on_seeded_schema` — design item 5: one query per catalog
  level (database → schema → table → column) against a schema created by the
  test, plus `describe()` resolving the primary key.
- `scan_op_streams_with_identity` — `Op::Scan` compiles to a real streamed
  `SELECT` and a single-table scan resolves `RowSchema::identity`.
- `non_select_returns_ack_shape_without_a_portal` — an `INSERT` gets
  `Shape::Ack { affected: Some(3) }`, not a table cursor.
- `quote_ident_survives_a_hostile_identifier` — end-to-end: a table named
  `weird"table` can be created and dropped through `quote_ident`.
