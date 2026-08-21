# Schema inference

When no explicit manifest is supplied, the connector builds a deterministic
table family from MongoDB evidence. Inference is bounded: it helps establish a
useful contract but does not claim that a sample proves every document shape.

## Evidence sources

Inference merges four kinds of information:

1. collection metadata and validation rules;
2. index definitions;
3. bounded document and array-element observations; and
4. connector defaults and any explicit configuration limits.

Provenance remains separate from conclusions. An indexed path is known to be
important, for example, but an index alone does not prove its scalar type,
requiredness, uniqueness as a relational key, or observed values.

## Collection validators

The connector extracts only validation facts it understands exactly, including:

- nested `properties` and scoped `required` lists;
- `bsonType` and supported JSON Schema `type` declarations;
- homogeneous array `items`;
- enum value types and `additionalProperties`;
- compatible type branches in `allOf`, `anyOf`, and `oneOf`; and
- safe conjunctive `$exists`, `$type`, equality, and `$in` predicates.

Unsupported validator fragments remain part of the inference fingerprint and
produce warnings. They are not interpreted approximately.

## Indexes

Index evidence retains ordered key paths and key kinds together with `unique`,
`sparse`, partial-filter, and hidden attributes. Index literals can add evidence,
but indexes do not fabricate collection values or scalar types.

MongoDB represents text indexes internally as `_fts` and `_ftsx` keys. The
connector reconstructs their user-facing source paths from the index `weights`
metadata and reports those paths with kind `text`. If a server omits the source
metadata, inference reports an explicit warning instead of exposing the internal
keys as collection fields.

## Sampling and budgets

Sampling reads documents in ascending MongoDB `_id` order and is bounded by:

- document count;
- total encoded bytes;
- nesting depth;
- inspected positions per array; and
- a per-operation time limit.

Array positions are distributed across the array rather than taking only a long
prefix. Reaching a budget records a warning and produces an intentionally
incomplete report. The stable `_id` order makes repeated inference over an
unchanged collection and configuration reproducible. It intentionally favors
repeatability over random exploration; use an explicit manifest when a reviewed
contract must cover known rare branches.

See [Configuration](configuration.md) for defaults and maximum values.

## Determinism and refresh

Documents are selected in stable `_id` order, and normalized evidence is
resolved by a pure deterministic step. Databases, collections, paths, fields,
BSON branches, and generated names are sorted before contract generation.

The inference fingerprint covers canonical collection metadata, UUID and
validator/options, complete index specifications, inference configuration, and
the resolved manifest. It deliberately excludes unstable sample order and raw
sample counts. Unchanged resolved evidence therefore produces byte-stable schema
metadata after `REFRESH`.

## Permissions

The inference report distinguishes unavailable metadata from metadata that was
successfully read but absent. Validator and index operations that receive MongoDB
`NotAuthorized` status degrade explicitly when other evidence is sufficient.
Sampling and ordinary queries still require collection read permission.

The compact report and fingerprint are stored in adapter notes. Credentials and
sample values are not.

## Schema changes

Run:

```sql
ALTER VIRTUAL SCHEMA MONGO_DEMO REFRESH;
```

after an intentional MongoDB schema change. Until refreshed, a query that reads
an incompatible BSON branch fails with the affected path and BSON type. Fields
not selected by a query are not read merely to search for unrelated drift.

Use an [explicit manifest](data-model.md#explicit-manifests) when schema changes
must go through review and deployment rather than inference.
