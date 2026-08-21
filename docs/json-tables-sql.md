# JSON-style SQL with Exasol JSON Tables

The MongoDB Virtual Schema exposes every collection as a relational table
family. That native interface is complete and requires no additional project.
If you prefer to address a document with paths and array expressions, the
optional [Exasol JSON Tables](https://github.com/exasol-labs/exasol-json-tables)
wrapper can generate a JSON-aware SQL surface over the same virtual tables.

The two interfaces are complementary:

| Interface | Example | Best suited to |
|---|---|---|
| Native virtual tables | Join `PEOPLE` to `PEOPLE_tags_arr` | Portable SQL, explicit relationships, and direct control over the physical model |
| JSON Tables wrapper | `"profile.city"`, `"tags[LAST]"`, `JOIN item IN ...` | Document-oriented exploration and concise nested queries |

The wrapper does not copy the MongoDB collection. Its public views read the
Virtual Schema, and its session preprocessor rewrites the JSON-style syntax to
SQL over the connector's table family.

## Install a wrapper over a MongoDB Virtual Schema

First create the MongoDB Virtual Schema as described in the project
[quick start](../README.md#quick-start). Install the `exasol-json-tables` Python
package from its release or source checkout so that the
`exasol-json-tables` command is available.

Generate a wrapper package by introspecting the tables exposed by the Virtual
Schema:

```bash
exasol-json-tables wrap generate \
  --source-schema MONGO_DEMO \
  --wrapper-schema MONGO_JSON \
  --helper-schema MONGO_JSON_INTERNAL \
  --preprocessor-schema MONGO_JSON_PP \
  --preprocessor-script MONGO_JSON_PREPROCESSOR \
  --output-dir ./dist/mongo-json \
  --package-name mongo_json \
  --dsn '<exasol-host>:8563' \
  --user '<user>' \
  --password '<password>'
```

Install the generated views and preprocessor:

```bash
exasol-json-tables wrap install \
  --package-config ./dist/mongo-json/mongo_json_package.json \
  --dsn '<exasol-host>:8563' \
  --user '<user>' \
  --password '<password>'
```

You can verify both the generated files and installed database objects with:

```bash
exasol-json-tables wrap validate \
  --package-config ./dist/mongo-json/mongo_json_package.json \
  --check-installed \
  --dsn '<exasol-host>:8563' \
  --user '<user>' \
  --password '<password>'
```

Activate the generated preprocessor in each SQL session that uses JSON-style
syntax:

```sql
ALTER SESSION SET SQL_PREPROCESSOR_SCRIPT =
  MONGO_JSON_PP.MONGO_JSON_PREPROCESSOR;
```

The public wrapper views remain ordinary queryable views without this setting,
but dotted paths, bracket access, JSON helpers, and array iterators require it.
Applications should run the `ALTER SESSION` statement whenever they open or
check out a pooled connection. Exasol permits only one active SQL preprocessor
per session; deployments that already use one need a coordinating master
preprocessor.

The examples below assume a `people` collection with fields such as:

```javascript
{
  _id: ObjectId("66b60c1f3dce4f58d74f97a1"),
  name: "Ada",
  profile: { city: "Copenhagen" },
  tags: ["rust", "analytics"],
  items: [
    { label: "first", flags: [true, false] },
    { label: "second", flags: [true] }
  ]
}
```

The generated root view keeps the source root-table name. Quote it exactly if
the installed package reports a lowercase or otherwise case-sensitive name.

## Use several collections in one session

This connector exposes one MongoDB collection per Virtual Schema, and each
wrapper package still describes exactly one source schema. Generate and install
one wrapper package per collection; repeating `--source-schema` in one
`wrap generate` command is rejected rather than silently discarding a value.

Exasol allows only one active SQL preprocessor per session, but that
preprocessor can cover several installed wrappers. Generate a combined
preprocessor from their manifests. The `--wrapper-schema`, `--helper-schema`,
and `--manifest` lists are positional and must use the same order:

```bash
python3 -m exasol_json_tables.generate_wrapper_preprocessor_sql \
  --schema MONGO_JSON_PP \
  --script MONGO_JSON_PREPROCESSOR \
  --wrapper-schema MONGO_JSON_CUSTOMERS \
  --wrapper-schema MONGO_JSON_ORDERS \
  --helper-schema MONGO_JSON_CUSTOMERS_INTERNAL \
  --helper-schema MONGO_JSON_ORDERS_INTERNAL \
  --manifest ./dist/customers/customers_manifest.json \
  --manifest ./dist/orders/orders_manifest.json \
  --output ./dist/mongo-json-combined-preprocessor.sql
```

Execute the generated SQL in Exasol, then activate that one script. The chosen
preprocessor schema must contain the JSON Tables preprocessor library; installing
either package with the same `--preprocessor-schema MONGO_JSON_PP` creates it.

```sql
ALTER SESSION SET SQL_PREPROCESSOR_SCRIPT =
  MONGO_JSON_PP.MONGO_JSON_PREPROCESSOR;

SELECT
  c."customer_id",
  c."address.city",
  o."order_id",
  o."line_items[SIZE]" AS line_item_count
FROM MONGO_JSON_CUSTOMERS."CUSTOMERS" c
JOIN MONGO_JSON_ORDERS."ORDERS" o
  ON o."customer_id" = c."customer_id";
```

Applications should activate the combined script when opening or checking out
a connection. Regenerate it whenever any included wrapper is regenerated.

## Navigate objects with dot paths

A quoted path traverses embedded objects without exposing the connector's
object child table in the query:

```sql
SELECT
  "name",
  "profile.city" AS city
FROM MONGO_JSON."PEOPLE"
ORDER BY "name";
```

Paths may have multiple levels, for example `"contact.address.country"`.
Keep the complete path in double quotes. An unquoted `profile.city` means
`column city of table profile` to SQL and is not the wrapper syntax.

## Address array elements

Bracket selectors return one array element or array metadata:

```sql
SELECT
  "name",
  "tags[0]"     AS first_by_index,
  "tags[FIRST]" AS first_tag,
  "tags[LAST]"  AS last_tag,
  "tags[SIZE]"  AS tag_count,
  "items[LAST].label" AS last_item_label
FROM MONGO_JSON."PEOPLE";
```

Array indexes are zero-based. Supported selectors are numeric literals,
`FIRST`, `LAST`, `SIZE`, `?` or `PARAM` for a prepared parameter, and a direct
field name from the current row. A parameter selector still requires a client
that actually sends a prepared parameter. Arbitrary expressions inside
brackets, such as `"tags[index + 1]"`, are intentionally rejected.

Use bracket access when one element is sufficient. Expand the array when every
element should become a result row.

### Expand an object array

`JOIN <alias> IN <array>` emits one iterator row per object-array element.
`_index` is its zero-based position:

```sql
SELECT
  p."name",
  item._index AS item_index,
  item."label",
  item."flags[LAST]" AS last_flag
FROM MONGO_JSON."PEOPLE" p
JOIN item IN p."items"
ORDER BY p."name", item._index;
```

Iterator rows support the same dotted paths, bracket selectors, and JSON helper
functions as root documents.

### Expand a scalar array

Use `JOIN VALUE` when the array elements themselves are scalar values:

```sql
SELECT
  p."name",
  tag._index AS tag_index,
  tag AS tag
FROM MONGO_JSON."PEOPLE" p
JOIN VALUE tag IN p."tags"
ORDER BY p."name", tag._index;
```

A scalar `VALUE` iterator supports ordinary SQL on the scalar value, but JSON
path and helper syntax do not start from that iterator.

### Match fields on the same array element

Use a correlated iterator in `EXISTS` when multiple predicates must be true for
one object in an array:

```sql
SELECT p."name"
FROM MONGO_JSON."PEOPLE" p
WHERE EXISTS (
  SELECT 1
  FROM item IN p."items"
  WHERE item."label" = 'second'
    AND item."flags[FIRST]" = TRUE
);
```

Do not use `"items.label"` to mean “any item label.” An array is not an object;
choose an element with brackets or expand it with an iterator.

## Query polymorphic values

MongoDB fields can hold different BSON types across documents. The native
table family exposes these as physical variant branches such as `value` and
`value|string`. The wrapper presents one logical property and provides helpers
for inspecting and extracting it:

```sql
SELECT
  "name",
  JSON_TYPEOF("value")       AS value_type,
  JSON_AS_VARCHAR("value")   AS value_text,
  JSON_AS_DECIMAL("value")   AS value_number,
  JSON_AS_BOOLEAN("value")   AS value_boolean
FROM MONGO_JSON."PEOPLE";
```

An extractor returns `NULL` when the current branch is not compatible with its
target scalar type. If `JSON_TYPEOF(...)` reports `OBJECT` or `ARRAY`, navigate
that value with dot or bracket syntax instead. Use these JSON-aware helpers,
not Exasol's built-in `TYPEOF` or a plain `CAST`, when the original per-document
JSON type matters.

When querying the connector's native physical tables instead of this logical
JSON wrapper, aggregate polymorphic numeric branches row by row. For example,
use `SUM(COALESCE(CAST("amount" AS DOUBLE PRECISION),
"amount|double"))`, not separate sums for the integer and double columns. The
exact branch names depend on the inferred schema, and NaN and infinities remain
in a separate canonical-text branch. See [Aggregate polymorphic numeric
fields](data-model.md#aggregate-polymorphic-numeric-fields) for the complete
pattern and its precision caveats.

## Distinguish missing from explicit null

Both a missing property and an explicit MongoDB `null` appear as SQL `NULL` in
the logical value. `JSON_IS_EXPLICIT_NULL` preserves the distinction:

```sql
SELECT
  "name",
  CASE
    WHEN JSON_IS_EXPLICIT_NULL("note") THEN 'explicit-null'
    WHEN "note" IS NULL THEN 'missing'
    ELSE 'value'
  END AS note_state
FROM MONGO_JSON."PEOPLE";
```

The connector also distinguishes the empty string internally because Exasol
represents it as SQL `NULL`; the wrapper reconstructs that contract from the
source-family masks.

## Reconstruct JSON documents

For an unmodified MongoDB root document, use zero-argument `TO_JSON()`. It
returns the source document captured by the connector rather than rebuilding
the document from inferred columns, so fields and BSON branches that occur only
in later outlier documents are retained:

```sql
SELECT TO_JSON() AS source_document_json
FROM MONGO_DEMO."PEOPLE";
```

The same call works on `MONGO_JSON."PEOPLE"` after generating a JSON Tables
wrapper; the wrapper passes the connector contract column through unchanged.

The result is canonical MongoDB Extended JSON. This preserves BSON-specific
values such as `ObjectId`, `Decimal128`, binary data, dates, and timestamps in
valid JSON. Exasol limits a `VARCHAR` value to 2,000,000 characters; the query
fails with the document column and measured length if canonical Extended JSON
for one MongoDB document exceeds that limit.

Zero-argument `TO_JSON()` is available only on a MongoDB root that exposes the
connector source-document contract. It is intentionally not available after
joining, aggregating, or otherwise reshaping rows. For those results, use JSON
Tables' existing `TO_JSON(*)` or `TO_JSON(column, ...)` reconstruction on an
ordinary view/table or a wrapped structured-result family.

`TO_JSON(*)` serializes a wrapped root recursively, including its nested object
and array branches:

```sql
SELECT TO_JSON(*) AS document_json
FROM MONGO_JSON."PEOPLE";
```

Select only named top-level branches when a complete document is unnecessary:

```sql
SELECT TO_JSON("name", "profile", "tags") AS document_json
FROM MONGO_JSON."PEOPLE";
```

In a joined query, qualify the selected top-level properties. Joined
`TO_JSON(*)` and nested or bracket expressions inside `TO_JSON(...)` are not
supported:

```sql
SELECT TO_JSON(p."name", p."profile") AS document_json
FROM MONGO_JSON."PEOPLE" p
JOIN ANALYTICS.PEOPLE_FLAGS f
  ON f.MONGO_ID = p."mongo_id";
```

## Operational guidance and boundaries

- Activate the preprocessor for every session that authors JSON-style SQL.
  Stable downstream consumers can instead query an ordinary view or table
  created from a wrapper query.
- Keep paths and source property names quoted. Prefer uppercase SQL-safe aliases
  when publishing columns for other tools.
- Qualify root helper arguments in joined queries, for example
  `JSON_TYPEOF(p."value")`.
- Path and helper syntax does not currently start from a derived-table root.
  Resolve the JSON expression in the inner query or query the wrapper directly.
- With PyExasol, use `execute()` and `fetchall()` for iterator queries.
  `export_to_pandas()` uses an `EXPORT` wrapper for which iterator rewrites are
  not reliable; publish an ordinary view or table before exporting instead.
- Refresh the MongoDB Virtual Schema after an intentional source-schema change,
  then regenerate and reinstall its JSON Tables wrapper so both contracts stay
  aligned.
- A MongoDB BSON Date maps directly to Exasol `TIMESTAMP(3)`. An ISO-8601 value
  imported as a MongoDB string remains a SQL `VARCHAR`; a plain `CAST` then
  depends on `NLS_TIMESTAMP_FORMAT` and commonly rejects the `T` separator. Use
  an explicit format for known input, for example
  `TO_TIMESTAMP("created_at", 'YYYY-MM-DD"T"HH24:MI:SS.FF3"Z"')` for a UTC
  millisecond string, or normalize the source data to BSON Date.

For portable SQL or precise control of joins and pushdown, query the connector's
native tables directly. See [Data model and BSON mapping](data-model.md) for the
underlying representation.
