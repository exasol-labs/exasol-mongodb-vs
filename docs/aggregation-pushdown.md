# Aggregation pushdown

Aggregation is delegated only when the MongoDB pipeline and Exasol SQL have the
same row set, null behavior, result cardinality, and result type. The first
supported operation is ungrouped `COUNT(*)`.

## Current execution contract

For a single-group `COUNT(*)`, the adapter places an exact translated filter
before MongoDB's `$count` stage. The scan UDF emits the one count value instead
of transferring the matching documents. MongoDB returns no document when the
input is empty; the UDF turns that case into one `DECIMAL(18,0)` value of zero to
preserve SQL aggregate semantics.

If a filter cannot be translated exactly, the scan stays row-producing and the
adapter's generated SQL performs both the filter and `COUNT(*)` in Exasol.
`COUNT(column)` also follows this exact fallback today. This deliberately keeps
MongoDB missing values, explicit nulls, empty strings, and polymorphic BSON
branches under the existing Exasol-facing column contract.

The same `$count` plan works for a root table and for a nested JSON table. In the
nested case it runs after path traversal and array unwinding, so it counts the
relational rows exposed by that table rather than root documents.

Use `EXPLAIN VIRTUAL` to distinguish the paths. A delegated count contains:

```json
"aggregation":{"kind":"count_star"}
```

and the scan emits a single internal `__jt_count` column. A fallback contains
`COUNT(...)` in the outer SQL and no aggregation in the serialized Mongo plan.

## Aggregate roadmap

| Exasol shape | MongoDB building block | Status and semantic work |
|---|---|---|
| Ungrouped `COUNT(*)` | `$count` | Supported, including exact filters, nested tables, and empty input. |
| `COUNT(column)` | `$group` with conditional `$sum` | Executed in Exasol for now. A remote implementation must use the exposed physical branch contract, not MongoDB's generic non-null test. |
| `COUNT(DISTINCT ...)` and tuple count | `$group` / set expressions | Deferred. Exasol tuple-null and value-equality semantics need an explicit contract, and large distinct sets can exceed MongoDB's group memory limits. |
| Grouped counts | `$group` | Deferred until group-key null/missing, string collation, BSON variants, output naming, `HAVING`, and ordering are proven equivalent. |
| `SUM` and `AVG` | `$sum` / `$avg` | Deferred. MongoDB ignores nonnumeric values and promotes numeric types on overflow; the connector must instead honor the advertised BSON branch and Exasol result type exactly. |
| `MIN` and `MAX` | `$min` / `$max` | Deferred. Mixed BSON ordering, string collation, missing values, and variant columns make a generic translation unsafe. |
| Boolean `EVERY` / `SOME` | `$min` / `$max` or conditional accumulators | Plausible after empty-input and null truth tables are covered. |
| Variance and standard deviation | `$stdDevPop` / `$stdDevSamp` | Deferred pending numeric coercion, floating-point, null, and result-type parity tests. |
| `LISTAGG` | `$push` plus string reduction | Not planned until ordering, collation, separator, overflow, and maximum-document-size behavior can match Exasol. |
| Approximate distinct count | no equivalent accuracy contract | Not currently a pushdown candidate. |

The implementation uses the existing typed `MongoPushdown` plan rather than a
parallel aggregate subsystem. This keeps filter eligibility, BSON path handling,
nested-table expansion, serialization, and runtime diagnostics in one execution
path. Additional aggregates should extend this typed plan one semantic unit at a
time.
