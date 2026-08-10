# Query pushdown

The connector minimizes MongoDB work and network transfer when it can prove that
MongoDB and Exasol produce the same result. Correctness takes priority over
delegation rate.

## Advertised operations

The capability set covers:

- physical-column projection;
- boolean composition with `AND`, `OR`, and `NOT`;
- equality and inequality/range comparisons, including inclusive `BETWEEN`;
- constant `IN` lists;
- `IS NULL` and `IS NOT NULL`;
- ungrouped `COUNT(*)` when every filter is exact;
- `LIMIT`; and
- bare-column ordering for eligible bounded top-N queries.

Regular expressions, `LIKE`, array expressions, offsets, grouped or general
aggregation, and execution of joins inside MongoDB are not currently advertised.

## Count aggregation

An eligible single-group `COUNT(*)` becomes a MongoDB `$count` stage after any
exact filter and after nested-table row expansion. This transfers one value
instead of every matching row. The runtime maps MongoDB's empty aggregate cursor
to zero, preserving SQL empty-input semantics.

`COUNT(column)` and counts with an inexact filter are accepted but evaluated in
the generated outer Exasol SQL. See [Aggregation pushdown](aggregation-pushdown.md)
for the semantic contract and aggregate roadmap.

## Exact MongoDB translation

The adapter parses Exasol expressions into a typed intermediate representation.
MongoDB receives a filter only when the entire filter is exact; a conjunction is
not partially delegated. The same all-or-nothing rule applies to `OR` and `NOT`.

Negation is pushed through `AND` and `OR` to guarded leaf predicates using De
Morgan's laws. A direct MongoDB `$not` around the compiled expression would turn
a missing, null, or wrong BSON branch from SQL `UNKNOWN` into true. Guarded leaf
negation preserves SQL three-valued `WHERE` semantics. MongoDB's query-level
`$nor` is not needed, and Exasol does not expose a separate `NOR` predicate node;
`NOT (a OR b)` uses the same typed boolean plan.

Eligible MongoDB predicates currently include:

- integer comparisons with BSON type guards;
- boolean predicates;
- ObjectId predicates converted from their exposed hex strings;
- timestamp predicates converted to BSON DateTime;
- explicit-null and empty-string preservation masks; and
- scalar `IS NULL`/`IS NOT NULL` based on the physical branch.

For eligible predicates on nested object or array tables, the scan first renders
the manifest's typed path segments as a native MongoDB dotted identifier. For
example, a predicate on `quantity` in the `items[]` table adds an early match of
the following shape before projecting and unwinding the table rows:

```javascript
{"items.quantity": {"$type": "int", "$gt": 2}}
```

That placement lets MongoDB use ordinary nested-field and multikey indexes. The
normal typed `$expr` predicate still runs after path traversal, so different
array elements cannot accidentally satisfy the type guard and comparison for a
single emitted row.

The early match is deliberately conservative. Safe conjuncts may be retained
from an `AND`; every branch of an `OR` must be representable; and `NOT`, `IS
NULL`, and `<>` stay in the exact post-traversal filter when array semantics
could create false negatives. Literal MongoDB field names containing `.` or
starting with `$`, plus direct nested-array levels, continue to use `$getField`
instead of ambiguous dotted syntax.

String equality and constant `IN` receive a dotted-path prefilter as well. The
connector uses an anchored, escaped, case-sensitive regular expression and
models Exasol's rule that `VARCHAR` equality ignores trailing ASCII spaces. The
original predicate remains in the generated Exasol SQL; consequently an early
`LIMIT`, remote `COUNT(*)`, and other optimizations that require an exact remote
filter remain disabled. Prefix-anchored expressions can use simple binary
MongoDB indexes, including multikey indexes. Collation-aware indexes generally
cannot accelerate `$regex`, but results remain correct because MongoDB's regex
matching does not inherit a broader collection collation.

String inequality and range predicates remain entirely in Exasol because their
collation and padding order is not assumed equivalent. Double predicates also
remain in Exasol until finite and non-finite branches can be guarded without
affecting early limits.

## Limits and top-N

A plain `LIMIT` can reach MongoDB when every preceding filter is exact. If a
filter remains in Exasol, applying the limit first could discard qualifying rows,
so the MongoDB limit is withheld.

`ORDER BY ... LIMIT` reaches MongoDB only for eligible integer, boolean, or
timestamp branches. The generated pipeline adds explicit null-rank keys to match
the requested `NULLS FIRST` or `NULLS LAST` behavior.

## Exasol semantic backstop

The SQL returned by the adapter reapplies delegated filters, ordering, and limits
over the scan result. This keeps Exasol as the final semantic authority and makes
the optimized path directly comparable with the no-capability path.

Hidden scan columns carry filter and ordering dependencies. Hidden sibling
branches are also included when a polymorphic field is selected, so projection
does not weaken BSON drift validation. Only requested columns leave the outer
query.

Set:

```sql
ENABLE_PUSHDOWN = 'false'
```

when creating a Virtual Schema to advertise no capabilities. This is useful for
operational rollback and result-parity diagnosis.

Use `EXPLAIN VIRTUAL` to inspect the generated scan plan. Plans contain the named
Exasol connection, namespace, typed MongoDB plan, and selected column contract;
they do not contain connection credentials.
