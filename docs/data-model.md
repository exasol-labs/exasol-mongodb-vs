# Data model and BSON mapping

MongoDB documents are hierarchical, while Exasol tables are rectangular. The
connector preserves the hierarchy as a deterministic family of related virtual
tables rather than flattening arrays into lossy strings.

This table-family model is fully implemented by this connector. Its manifest is
compatible with the source-manifest format associated with `exasol-json-tables`,
but that project is optional and is not needed to query MongoDB.
For its document-oriented dot paths, bracket selectors, array iterators, and
JSON helpers, see [JSON-style SQL with Exasol JSON Tables](json-tables-sql.md).

## Root and child tables

Each collection has one root table. Nested objects and arrays get child tables:

- an embedded object has one child row when the object exists;
- an array has one child row per element;
- arrays of objects combine both rules; and
- nested arrays create another array table for each level.

Structural columns connect these rows:

| Column | Meaning |
|---|---|
| `_id` | Stable identity of a root, object, or nested-array parent row. |
| `_parent` | Stable identity of the parent row for an array element. |
| `_pos` | Zero-based array position. Preserves ordering and duplicates. |
| `_value` | Scalar array-element value. |
| `_value\\|object` | `TRUE` for an object element when an array also contains other BSON kinds. |
| `_value\|array` | Nested-array length and branch marker on a polymorphic array-element row. |
| `<field>\|object` | Link from a row to an embedded-object child table. |
| `<field>\|array` | Array length and structural marker for an array child table. |

New inferred contracts use a SHA-256-derived `VARCHAR(64)` identity. Compatible
explicit manifests may use `DECIMAL(18,0)`, which receives a stable folded
identity.

## Missing, null, and empty string

These states are different in MongoDB and remain distinguishable:

- a missing field leaves all value branches and masks unset;
- an explicit BSON `null` sets `<field>|n = TRUE`;
- a present empty string sets `<field>|empty = TRUE`; and
- an ordinary value appears in exactly one scalar branch.

Scalar arrays use `_value|n` and `_value|empty` for the same purposes.
When objects, scalars, and nested arrays coexist, their branches remain in one
array table. `_value|object` identifies object rows, object fields are projected
on those rows, and `_value|array` contains the nested array's length. Every
scalar branch is `NULL` on both structural row kinds. The corresponding child
tables contain embedded objects and ordered nested-array elements.

The empty-string mask is necessary because Exasol represents an empty string as
SQL `NULL`. Query the mask when the distinction matters.

## Polymorphic fields

A MongoDB field can hold different BSON types in different documents. The
connector exposes a tagged union instead of coercing everything to one majority
type. One branch uses the base column name and additional branches receive stable
suffixes, for example:

```text
value          DECIMAL(19,0)   -- BSON int/long branch
value|string   VARCHAR(...)    -- BSON string branch
value|n        BOOLEAN         -- explicit BSON null
```

Only one value branch is populated for a document. Queries fail if they read a
BSON branch absent from the current contract; refresh the inferred schema or
update the explicit manifest after an intentional change.

### Aggregate polymorphic numeric fields

Aggregate the branches as one row-level value before applying an aggregate.
For example, if inference exposes an integer branch as `amount` and a finite
double branch as `amount|double`, use:

```sql
SELECT
  SUM(COALESCE(
    CAST("amount" AS DOUBLE PRECISION),
    "amount|double"
  )) AS amount_total,
  AVG(COALESCE(
    CAST("amount" AS DOUBLE PRECISION),
    "amount|double"
  )) AS amount_average,
  COUNT(COALESCE(
    CAST("amount" AS DOUBLE PRECISION),
    "amount|double"
  )) AS numeric_amount_count
FROM MONGO_DEMO."MEASUREMENTS";
```

Do not calculate the branches independently and then combine the aggregates.
For example, `AVG("amount") + AVG("amount|double")` has no useful meaning, and
`SUM("amount") + SUM("amount|double")` becomes `NULL` within any group where one
branch is absent. Row-level `COALESCE` preserves the tagged-union rule that at
most one scalar branch is populated for each source value.

The unsuffixed name belongs to the branch with the strongest inference evidence,
so inspect the generated column names and types rather than assuming that it is
the integer branch. A collection where doubles dominate might instead expose
`amount DOUBLE PRECISION` and `amount|integer DECIMAL(19,0)`; reverse the
branches as `COALESCE("amount", CAST("amount|integer" AS DOUBLE PRECISION))`.
Choose the common SQL type deliberately: converting a 64-bit integer to
`DOUBLE PRECISION` can lose precision, while converting arbitrary doubles to a
fixed-scale `DECIMAL` can round or overflow.

NaN and positive or negative infinity are not members of the finite-double
branch. They are exposed separately as canonical Extended JSON text in a
`non_finite_double` branch and must not be included in a numeric `COALESCE`.
Account for them explicitly when completeness matters, for example:

```sql
SELECT
  COUNT(COALESCE(
    CAST("amount" AS DOUBLE PRECISION),
    "amount|double"
  )) AS finite_numeric_count,
  COUNT("amount|non_finite_double") AS non_finite_count
FROM MONGO_DEMO."MEASUREMENTS";
```

Adjust the physical names in these examples to the inferred schema. In
particular, the base `amount` column itself may be the non-finite branch if that
branch has the strongest evidence.

When validating results directly in MongoDB, use BSON type predicates rather
than JavaScript `typeof`. The shell presents BSON Int32, Int64, and Double values
as JavaScript numbers in contexts where `typeof` cannot distinguish their source
types. Also remember that `{amount: {$type: "double"}}` includes NaN and both
infinities. A ground-truth filter for the connector's finite-double branch must
exclude those values explicitly:

```javascript
{
  amount: {
    $type: "double",
    $nin: [NaN, Infinity, -Infinity]
  }
}
```

## BSON-to-Exasol mapping

The general rule is: use a native Exasol scalar when the conversion is lossless;
otherwise preserve canonical Extended JSON text and the precise BSON tag.

| BSON value | Exasol representation | Notes |
|---|---|---|
| Boolean | `BOOLEAN` | Direct. |
| Int32 | `DECIMAL(10,0)` | May be unified with Int64. |
| Int64 | `DECIMAL(19,0)` | Never routed through `DOUBLE`. |
| Double | `DOUBLE PRECISION` | Non-finite values use a separate canonical-text branch. |
| Decimal128 | `VARCHAR(50)` canonical Extended JSON | Avoids precision loss. |
| String | `VARCHAR(2000000)` | Empty strings also set an empty-string mask. |
| ObjectId | lowercase hex `VARCHAR(24)` | Retains the ObjectId BSON tag for predicates. |
| DateTime | `TIMESTAMP(3)` | Exposed as a UTC instant. |
| BSON Timestamp | two `DECIMAL(10,0)` branches | Time and increment components; not a wall-clock DateTime. |
| Binary, regex, JavaScript, DBPointer, symbol | canonical Extended JSON in `VARCHAR(2000000)` | Exact BSON form retained as text. |
| Undefined, MinKey, MaxKey | tag-only variant | Never conflated with missing or null. |
| Document | object link and child table | Recursively queryable. |
| Array | length and ordered child table | Recursively queryable. |
| Null | `<field>\|n BOOLEAN` | Present explicit null. |

Values exceeding the supported `VARCHAR(2000000)` representation fail the row
instead of being truncated.

## Names

Physical Exasol names are deterministic and collision-safe. The manifest retains
the exact MongoDB source name separately, so fields containing dots, dollars,
quotes, or connector suffix delimiters remain addressable.

The canonical nested path is stored as typed `pathSegments`; a human-readable
path string is informational and is not parsed during scans. Eligible `WHERE`
predicates render those typed segments directly as native MongoDB dotted paths,
allowing nested-field and multikey indexes to participate without treating a
literal dot in a source field name as a separator.

## Explicit manifests

The manifest describes:

- the root and child tables;
- typed path segments;
- parent/child relationships;
- physical columns and Exasol types;
- exact source field names; and
- precise BSON branches.

See [`../examples/people.source_manifest.json`](../examples/people.source_manifest.json).
The optional `sourceName` field preserves the exact MongoDB field name, and
`bsonType` selects a branch such as `OBJECT_ID`, `INT64`, `DATE_TIME`,
`DECIMAL128`, or `EXTENDED_JSON`.
