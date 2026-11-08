# Named schema migrations

The numbered migrations in `src/db.rs` are frozen. Add new migrations here
as uniquely named SQL files, for example `ter-284-add-example.sql`. No
registry or next sequence number needs editing. The files are embedded in
the daemon and pending files run in lexical filename order.

Each file runs in its own transaction. Do not include transaction control
or change `PRAGMA user_version`. Applied filenames are recorded in
`schema_migrations` in the same transaction as the SQL. Never rename or edit
a shipped file. A newly added file runs even if its name sorts before an
already applied file. When migrations depend on each other, choose filenames
that sort in dependency order and ensure the prerequisite ships first.

Existing databases finish the unchanged legacy migrations before running
named files. `user_version` remains at the last legacy version.
