# Linux desktop verification checklist (issue #47)

The human half of the pass. Run `scripts/verify-linux.sh` first — it fetches the
newest packaged build, verifies checksums, checks the glibc floor, launches the
app in an isolated `DATAGREP_CONFIG_DIR`, and proves the process starts, opens
its engine, and survives a restart. **It proves nothing below this line.** Every
item here needs eyes because it is about what renders and what a click does, and
the one prior desktop session (21 Aug) found two shipping blockers CI could not
see.

Ordering is by likelihood of breakage, judged from what has actually broken in
this project — not by feature list.

## Setup

- Do the pass twice if possible: once under KDE, once under GNOME. At minimum
  note which desktop and session type (`echo $XDG_CURRENT_DESKTOP $XDG_SESSION_TYPE`).
- Launch from a terminal with the command the script prints at the end — stderr
  is the only diagnostic channel, and reusing the script's config sandbox means
  anything you save feeds the restart test in item 6.
- Live engines (from `notes/testing.md`):

  ```
  docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=secret --name dg-pg postgres:16
  docker run --rm -d -p 9200:9200 -e discovery.type=single-node \
    -e xpack.security.enabled=false --name dg-es \
    docker.elastic.co/elasticsearch/elasticsearch:8.15.0
  ```

  ES needs ~60s to come up; seed it with a few docs before item 5:

  ```
  for i in 1 2 3; do curl -s -XPOST localhost:9200/verify/_doc/$i \
    -H 'Content-Type: application/json' -d "{\"name\":\"doc $i\",\"count\":$i}"; done
  curl -s -XPOST localhost:9200/verify/_refresh
  ```

- For anything wrong: capture a screenshot, the stderr log the script points at,
  and the exact query/action. That triple is what routes a bug to a fix PR.

## 1. The results grid actually draws rows

The macOS grid silently rendered nothing for weeks while every layer under it
worked; highest prior probability of the same class of bug here.

- **Do:** add the Postgres connection, expand the schema tree, run
  `select * from pg_catalog.pg_tables limit 10`.
- **Correct:** ten visible rows with values in cells, a row-number gutter, and a
  status bar that states the row count honestly.
- **If wrong:** screenshot the empty/garbled grid, note whether the status bar
  *claims* rows loaded (that distinguishes a paint bug from a fetch bug), attach
  stderr.

## 2. Connection dialog saves, and Test Connection tells the truth

The dialog literally could not save on 21 Aug because it sent a deleted `env`
key (#34) — dialog-to-engine drift is proven to happen and CI cannot see it.

- **Do:** create, save, close, reopen, and edit a connection for Postgres and
  for Elasticsearch. Then press Test Connection three times: against the live
  engine, against a port nothing listens on (`localhost:1`), and against a
  non-routable host that hangs (`10.255.255.1`).
- **Correct:** saves persist across dialog reopen; Test reports success fast,
  refusal fast and legibly, and the hang case returns with a timeout message
  rather than freezing the dialog.
- **If wrong:** the exact field values, the error text verbatim, and stderr —
  a save failure here is engine-contract drift, not cosmetics.

## 3. Appearance: force dark, force light, follow-system

Force-dark was white-on-white in every input until PR #46, and the bug only
existed in the composition of two individually-correct PRs — exactly the kind of
thing only a rendered window shows.

- **Do:** switch force light → force dark → follow-system. Under follow-system,
  flip the OS theme (KDE: System Settings → Colors; GNOME: dark style toggle)
  while the app is running.
- **Correct:** every text surface readable in both modes — editor, grid, tree,
  dialogs, history panel, inspector. The system flip repaints live without a
  restart.
- **If wrong:** screenshot each unreadable surface; note the mode and whether it
  needed a restart to take.

## 4. Safety features do things, not just render

The profile list stopped emitting the keys the UI gated on (#41), which killed
read-only, confirm-writes and marker on *both* platforms silently. The Linux
enforcement code has never once executed.

- **Do:** mark a connection with a colour, enable read-only, then run
  `create table verify_ro (id int)` on it. Separately enable confirm-writes and
  run an `insert`.
- **Correct:** the coloured band is visible above the results area, the tooltip
  on the connection mentions the state, the read-only write is **refused with a
  message that says why**, and confirm-writes shows a prompt whose Cancel really
  cancels (no table appears).
- **If wrong:** this is a shipping blocker, not a bug — a write going through on
  a read-only connection outranks everything else on this list. Capture the SQL,
  the banner state, and whether the engine or the UI let it pass.

## 5. The Elasticsearch editing chain, end to end

The largest block of never-executed UI code in the tree: inline editors, staged
repaints, EditingSurface, halt-and-report, ConflictResolution. Compile-verified
only.

- **Do:** query the `verify` index, double-click a `_source` field, edit it,
  and watch it stage. Stage edits on two docs. Read the commit confirmation
  wording, then commit. Then force a real 409: stage an edit on a doc, bump it
  behind the app's back —

  ```
  curl -s -XPOST localhost:9200/verify/_update/1 \
    -H 'Content-Type: application/json' -d '{"doc":{"count":99}}'
  ```

  — and commit. Walk the conflict review and try both Rebase and Discard Mine.
- **Correct:** staged cells repaint distinctly; the confirmation never claims
  the batch stops at the first failure for a multi-doc `_bulk` commit ("nothing
  is rolled back" is the only honest claim); the report sheet accounts for every
  row; the 409 shows the three-way loaded / on-the-server-now / you-typed view;
  Discard Mine drops the edit, Rebase re-stages it over the fresh doc and a
  re-commit then succeeds.
- **If wrong:** the report sheet contents verbatim, the `curl` you ran, and
  `curl localhost:9200/verify/_doc/1` before and after — server truth versus UI
  claim is the whole question here.

## 6. History and tabs survive a real quit and relaunch

The stores (`history/*.jsonl`, `tabs/session.json`) are written on use, so the
script's restart check never touches them. Session restore has run only in CI.

- **Do:** run a few distinct queries, open a second editor tab with unexecuted
  text in it, quit via the window close button, relaunch with the same
  `DATAGREP_CONFIG_DIR`, and check the history panel filter and the retention
  dialog while you are in there.
- **Correct:** history lists the queries, filtering narrows them, tabs come back
  with their text and the active tab preserved, and `ls` of the config dir shows
  `history/` and `tabs/` alongside `profiles.sqlite`.
- **If wrong:** tar up the config dir before relaunching a second time — the
  on-disk state is the evidence, and it also feeds the macOS byte-compatibility
  question from the issue.

## 7. Inspector dock, identity chip, envelope

Implemented against verified ABI payloads, but layout and theming on a real
desktop are unproven — and the ES envelope moved out of the grid columns
(PR #20), so the inspector is now the *only* place `_index`/`_id` appear.

- **Do:** open the inspector on a SQL cell and on an ES cell; check the ES one
  leads with the envelope; check the identity chip on the connection.
- **Correct:** readable in both themes, nested values expand, no clipped or
  overlapping text (the connection dialog clipped on first real launch —
  layout bugs are proven likely here).
- **If wrong:** screenshot plus theme mode.

## 8. Keyboard traversal and the copy path

Window-manager-dependent by nature; CI cannot hold an opinion.

- **Do:** Tab between sidebar, editor and grid; arrow around the grid; copy a
  cell, a row and a rectangular selection; paste into a text editor.
- **Correct:** focus is always visibly somewhere, no traversal dead-ends, and
  the paste contains the cell values without the row-number gutter.
- **If wrong:** the key sequence, the desktop (KDE and GNOME differ here), and
  what the clipboard actually contained.

## Closing the loop

Every item driven once, on each desktop tried, closes this file's share of #47.
Anything found gets its own fix PR — do not batch fixes into the verification
branch.
