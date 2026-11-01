---
name: terrarium-db
description: Keep structured data of the project in the Terrarium DB (record types with a schema, records, and queries) with `terrarium db`, and show query results in wiki pages with a `db` block. Use it for lists of items, inventories, results, measurements, and decisions that you add to, filter, or count again later.
---

# The Terrarium DB

Each project has a small database: the DB. You make record types, each
with a schema (its fields), and records of these types. The operator sees
and edits the same data in the dashboard (Knowledge → DB). Wiki pages can
show query results with a `db` block.

## When to use the DB

Use the DB for structured data that repeats and that you will filter,
sort, or count:

- lists of items: bugs, tasks that are not cards, open questions, risks;
- inventories: services, hosts, dependencies, contacts, accounts;
- results: benchmark runs, test results, measurements over time;
- decisions: what was decided, when, why, and the status.

Do not use the DB for free text: write a wiki page. Do not use it for one
fact: write a memory (`terrarium mem new`). Do not use it for work to do:
make a card. A record can link to the others in a `text` field
(`[[TER-42]]`, a wiki path).

Before you make a type, look at the types that exist:

```sh
terrarium db type ls
terrarium db type show bug
```

Use a type that exists when it fits. Add a field to it before you make a
second type for the same items.

## Types and fields

A type has a name (`[a-z][a-z0-9_-]*`, for example `bug`, `load-test`) and
1 to 100 fields. A field has a name (`[a-z][a-z0-9_]*`), a kind, and
these options: required, a default, a description, unique.

| Kind | Value | Example `--field` |
|---|---|---|
| `string` | one line, at most 1000 characters | `title:string!` |
| `text` | long text, Markdown, at most 64 KiB | `notes:text` |
| `number` | a number | `value:number!` |
| `integer` | a whole number | `port:integer=8080` |
| `bool` | `true` or `false` | `healthy:bool=true` |
| `date` | `YYYY-MM-DD` | `reported:date!` |
| `datetime` | RFC 3339, stored in UTC | `run_at:datetime!` |
| `enum` | one of the values; their order is the sort order | `severity:enum(low,medium,high,critical)!=low` |
| `list` | a list of `string`, `integer`, or `number` | `tags:list(string)` |
| `ref` | a record of another type of the project | `service:ref(service)` |
| `json` | any JSON, at most 64 KiB | `extra:json` |

In a `--field` spec, `!` after the kind makes the field required, and
`=value` gives the default. These names are reserved: `id`, `type`,
`created`, `updated`, `created_by`, `updated_by`, `revision`, `sort`,
`limit`, `fields`, `count`, `group`, `view`. A record has at most 64 KiB.

The first `string` field of a type is its label: a ref shows it, and you
can give a ref value as this label instead of the record ID.

```sh
terrarium db type new service -d "The services we run" \
  --field 'name:string!' --field 'language:enum(rust,go,python,typescript)' \
  --field 'port:integer' --field 'healthy:bool=true' --field 'tags:list(string)' \
  --unique name
terrarium db type new bug \
  --field 'title:string!' --field 'severity:enum(low,medium,high,critical)!' \
  --field 'status:enum(open,in_progress,fixed,wontfix)!=open' \
  --field 'service:ref(service)' --field 'reported:date!' --field 'notes:text'
terrarium db type new run --from-file schema.json     # a list of fields, or {"description", "fields"}
```

### Change a schema

```sh
terrarium db type edit bug --field 'component:string'          # add a field (or replace one)
terrarium db type edit bug --field 'team:string!=core'          # a new required field needs a default; it fills every record
terrarium db type edit bug --rename component:area              # the data moves to the new name
terrarium db type edit bug --remove area --confirm              # the data of the field goes from every record
terrarium db type rm bug                                        # a type with no records
terrarium db type rm bug --records                              # with its records: project_admin only
```

A change that existing records do not fit (a new kind, a field that is
required now and has no value) is refused. The error lists up to 20 of
the records. Change or delete these records, then try again. Each change
makes a new revision of the type.

## Records

```sh
terrarium db rec new bug --set 'title=Login fails on Safari' --set severity=high \
  --set service=auth --set reported=2026-10-01 --set tags=ui,auth
terrarium db rec new bug --json-data - < bug.json                # a JSON object of the values
terrarium db rec get 01JB...                                      # one record
terrarium db rec set 01JB... --set status=fixed --set fixed=2026-10-04
terrarium db rec set 01JB... --unset notes                       # remove an optional value
terrarium db rec set 01JB... --set status=fixed --revision 3     # refused if someone changed it since revision 3
terrarium db rec rm 01JB...
terrarium db rec ls bug status=open sort:-severity
```

`--set field=value` takes the value in the kind of the field: a list is
`a,b,c` (or a JSON list), a bool is `true` or `false`, and `-` reads the
value from stdin (for a long `text`). A record that a ref of another
record points at cannot be deleted: the error lists the records that point
at it.

Every write is checked against the schema. A problem names the field:
`title: is required`, `severity: must be one of: low, medium, high`.

## Queries

```sh
terrarium db query 'type:bug status=open severity>=high sort:-severity limit:20'
terrarium db query 'type:bug group:component'                   # a count for each value
terrarium db query 'type:bug status=open count:true'            # only the count
terrarium db query 'type:benchmark service=auth run_at>-7d' --format csv
terrarium db query 'type:bug title~"login page"' --json
```

A query is terms separated by spaces:

| Term | Meaning |
|---|---|
| `type:bug` | the type (required) |
| `field=v`, `field!=v` | equal, not equal (a record with no value is "not equal") |
| `field>v`, `>=`, `<`, `<=` | numbers, dates, times, and enums in their declared order (`severity>=high`) |
| `field~text` | contains, without case; for a list: has the item |
| `field:null`, `field:!null` | has no value, has a value |
| `sort:field`, `sort:-field` | sort up or down; more than one is good (`sort:-severity,title`) |
| `limit:N` | 1 to 500 records (default 50) |
| `fields:a,b,c` | only these columns |
| `count:true` | only the number of records |
| `group:field` | the number of records for each value |
| `created>…`, `updated>…` | the record times; also `id`, `created_by`, `revision` |

Put a value with spaces in double quotes: `title="Login fails"`. A date
or time can also be `today`, `now`, `-7d`, `-12h`, `-30m`, or `-2w`. A ref
takes the record ID or its label: `service=auth`. A bad query gives the
position of the problem.

## Show data in the wiki

A fenced block with the language `db` in a wiki page shows the result of
its query when someone reads the page. The query runs with the rights of
the reader, in the project of the page.

````markdown
```db
type:bug status=open severity>=high sort:-severity limit:10 fields:title,severity,service
```
````

Add `view:` to choose the form:

| View | Shows |
|---|---|
| `view:table` (default) | a table of the `fields:` columns |
| `view:list` | a list of the first column |
| `view:count` | the number of records |
| `view:group` | with `group:field`, a table of the count for each value |

~~~sh
terrarium wiki new reports/bugs -m "Bug report" --body - <<'EOF'
# Open bugs

```db
type:bug status=open count:true view:count
```

```db
type:bug status=open group:severity view:group
```
EOF
~~~

## Rules

- Every agent of the project, and the operator, reads and writes the
  types and records of the project. Agents of other projects cannot.
- Keep the names short and clear. Write a description for each type and
  for each field whose meaning is not clear from its name.
- Do not put secrets in the DB. Terrarium replaces a known secret value
  with `[secret:<name>]`.
