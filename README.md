<div align="center">

# Exasol MongoDB Virtual Schema

**Query MongoDB from Exasol with document-friendly SQL—dot paths, array iteration, and JSON reconstruction—without copying collections first.**

![Rust 1.94.1](https://img.shields.io/badge/Rust-1.94.1-000000?logo=rust)
![MongoDB 8 tested](https://img.shields.io/badge/MongoDB-8%20tested-47A248?logo=mongodb&logoColor=white)
![License MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![Coverage enforced](https://img.shields.io/badge/coverage-%E2%89%A585%25-brightgreen)

[Quick start](#quick-start) · [JSON-style SQL](docs/json-tables-sql.md) · [Data model](#how-documents-become-tables) · [Documentation](#documentation) · [Development](#development)

</div>

Exasol MongoDB Virtual Schema makes a MongoDB collection available as a set of
queryable Exasol virtual tables. Combined with the optional Exasol JSON Tables
SQL interface, MongoDB users can navigate embedded objects and arrays with
familiar document paths while retaining the full power of Exasol SQL.

Use it when analysts and applications need to combine MongoDB data with the SQL
and analytics capabilities of Exasol, while MongoDB remains the system of record.
The connector discovers fields, preserves nested structures, maps BSON values to
safe SQL types, and streams results directly from MongoDB. It is also fully
usable on its own through its native relational table-family interface.

> [!IMPORTANT]
> This project is currently at version 0.1 and is distributed as source. Its
> configuration and schema contract may evolve before 1.0. The automated suite
> tests MongoDB 8, builds the Linux Rust UDF artifact, and runs a separate live
> acceptance suite against Exasol Personal.

## Query MongoDB like a document

Given a collection containing:

```javascript
{
  _id: ObjectId("66b60c1f3dce4f58d74f97a1"),
  name: "Ada",
  profile: { city: "Copenhagen" },
  tags: ["rust", "analytics"]
}
```

the optional [Exasol JSON Tables SQL interface](docs/json-tables-sql.md) lets
you use quoted dot paths and array selectors directly:

```sql
SELECT
  "name",
  "profile.city" AS city,
  "tags[FIRST]" AS first_tag,
  "tags[LAST]" AS last_tag,
  "tags[SIZE]" AS tag_count
FROM MONGO_JSON."PEOPLE";
```

Expand every scalar array element into an ordered result row:

```sql
SELECT
  p."name",
  tag._index AS tag_index,
  tag
FROM MONGO_JSON."PEOPLE" p
JOIN VALUE tag IN p."tags"
ORDER BY p."name", tag._index;
```

Or reconstruct complete JSON documents after filtering them with SQL:

```sql
SELECT TO_JSON(*) AS document_json
FROM MONGO_JSON."PEOPLE"
WHERE "profile.city" = 'Copenhagen';
```

The JSON Tables wrapper is a companion query layer: it generates views over
this connector's virtual tables and does not copy the collection. See the
[setup and query guide](docs/json-tables-sql.md) for object arrays, polymorphic
values, explicit `null`, and session activation.

## Why use it?

- **Use familiar document paths.** Query nested fields and arrays with dot
  paths, bracket selectors, and array iterators through the optional JSON Tables
  interface.
- **Start without hand-writing a schema.** Infer tables and columns from MongoDB
  validators, indexes, and a bounded document sample.
- **Query nested data naturally.** Embedded objects and arrays become related
  virtual tables with stable identities and array positions.
- **Keep document semantics.** Missing fields, explicit `null`, empty strings,
  BSON types, and polymorphic fields remain distinguishable.
- **Avoid unnecessary transfer.** Projection, exact scalar filters, limits, and
  eligible top-N operations are pushed toward MongoDB conservatively.
- **Choose stability when needed.** Supply an explicit manifest to own and review
  the exposed schema instead of inferring it.
- **Keep credentials out of generated plans.** Queries refer to a named Exasol
  connection; credentials are not embedded in adapter notes or scan SQL.

## How documents become tables

Underneath the JSON-style interface, the connector exposes the example
document as a root table and child tables similar to:

```text
PEOPLE
  _id  mongo_id  name  profile|object  tags|array

PEOPLE_profile
  _id  city

PEOPLE_tags_arr
  _parent  _pos  _value
```

`_id` and `_parent` are stable relationship keys. `_pos` is the zero-based array
position, so array order and duplicate values are preserved.

```sql
SELECT p."name", t."_pos", t."_value" AS "tag"
FROM MONGO_DEMO."PEOPLE" p
JOIN MONGO_DEMO."PEOPLE_tags_arr" t
  ON t."_parent" = p."_id"
ORDER BY p."name", t."_pos";
```

See [Data model and BSON mapping](docs/data-model.md) for nested objects,
polymorphic values, null masks, empty strings, and the explicit manifest format.

## Quick start

### Prerequisites

- an Exasol deployment with Virtual Schema support and a [Rust Script Language Container](https://github.com/exasol-labs/language-container-rs) matching SDK 0.22.1 and Rust 1.94.1;
- a MongoDB deployment reachable from the Exasol runtime;
- permission to install a UDF library and create scripts, connections, and a
  virtual schema; and
- Docker when building the Linux artifact from source.

### 1. Build the connector

```bash
make build-so verify-so
```

This creates `target/release/libmongodb_vs.so` in the pinned Debian/glibc build
environment used for the supported artifact. Do not deploy a host-built library.

The required Rust SLC fingerprint is:

```text
0.22.1:rustc_1.94.1__e408947bf_2026-03-25_
```

Exasol compares this fingerprint exactly. The repository therefore pins both
`exasol-udf-sdk` and `exasol-udf-macros` to 0.22.1 and Rust to 1.94.1. Register
a matching SLC and configure the `RUST` language alias to use it before
installing the scripts; an SLC built for another SDK or compiler version cannot
load this artifact. `make verify-so` derives the SDK portion from `Cargo.lock`
and combines it with the toolchain portion in `rust-udf-fingerprint.txt` before
checking the artifact.

### 2. Install the Exasol scripts

Copy the library to:

```text
/buckets/bfsdefault/rust/libmongodb_vs.so
```

Then execute [`sql/install.sql`](sql/install.sql) in Exasol. It creates:

- `MONGODB_VS.MONGODB_ADAPTER`, the Virtual Schema adapter; and
- `MONGODB_VS.MONGODB_SCAN`, the streaming MongoDB scan UDF.

The exact BucketFS upload command depends on your Exasol deployment.
`sql/install.sql` assumes the `RUST` alias selects the required SLC and the
library is under `/buckets/bfsdefault/rust/`; adjust the alias or SQL path for
your deployment.

### 3. Register MongoDB and create a Virtual Schema

```sql
CREATE OR REPLACE CONNECTION MONGODB_CONNECTION
TO 'mongodb://mongodb.internal:27017/?authSource=admin'
USER 'analytics_reader'
IDENTIFIED BY 'replace-with-your-password';

CREATE VIRTUAL SCHEMA MONGO_DEMO
USING MONGODB_VS.MONGODB_ADAPTER WITH
  MONGODB_CONNECTION = 'MONGODB_CONNECTION'
  DATABASE = 'demo'
  COLLECTION = 'people';
```

With no `MANIFEST` property, the connector infers a deterministic schema from
the collection metadata and a bounded sample. The MongoDB user needs `find`
permission for sampling and queries; metadata and index permissions improve
inference but degrade explicitly when unavailable.

### 4. Choose your SQL interface

For the MongoDB-friendly interface shown above, follow the short
[JSON Tables wrapper setup](docs/json-tables-sql.md#install-a-wrapper-over-a-mongodb-virtual-schema),
activate it in the current session, and query document paths directly:

```sql
ALTER SESSION SET SQL_PREPROCESSOR_SCRIPT =
  MONGO_JSON_PP.MONGO_JSON_PREPROCESSOR;

SELECT "name", "profile.city", "tags[LAST]"
FROM MONGO_JSON."PEOPLE"
ORDER BY "name";
```

The connector can also be queried immediately without the optional wrapper by
using its native root and child tables:

```sql
SELECT "name"
FROM MONGO_DEMO."PEOPLE"
ORDER BY "name";

EXPLAIN VIRTUAL
SELECT "name"
FROM MONGO_DEMO."PEOPLE"
WHERE "name" IS NOT NULL
LIMIT 10;
```

The inferred root table name is derived deterministically from the collection
name. For a collection named `people`, it is `PEOPLE` unless a naming collision
requires a suffix.

For a complete runnable example, see [`sql/example.sql`](sql/example.sql).

## Schema management

Automatic inference is convenient for exploration and evolving collections.
Refresh it after intentional source-schema changes:

```sql
ALTER VIRTUAL SCHEMA MONGO_DEMO REFRESH;
```

For production contracts, you can instead pass a reviewed `MANIFEST` property.
The example in
[`examples/people.source_manifest.json`](examples/people.source_manifest.json)
describes root, object, scalar-array, object-array, and nested-array tables.

Inference is bounded and never claims that a sample proves complete collection
coverage. Queries fail on an incompatible BSON branch that is actually read,
rather than silently coercing or discarding the value.

## Current scope

Supported today:

- regular MongoDB collections;
- inferred or explicit schemas;
- nested objects, arrays, and arrays of arrays;
- ordered array rows and stable parent/child joins;
- common BSON scalar types and lossless numeric handling;
- complete root-document export as canonical Extended JSON through JSON Tables'
  zero-argument `TO_JSON()`, including fields outside the inferred schema;
- physical-column projection, exact scalar predicates composed with `AND`, `OR`, and `NOT`, null checks, `IN`,
  `LIMIT`, eligible top-N, and ungrouped `COUNT(*)` pushdown; and
- URI authentication settings plus Exasol connection USER/PASSWORD overrides.

Not yet part of the supported release scope:

- write operations;
- grouped and general aggregation or join execution inside MongoDB;
- automatic schema evolution during a query;
- views and time-series collections as verified inference sources;
- distributed scan partitioning; and
- a published compatibility matrix for replica sets, sharded clusters, and all
  authentication/TLS combinations.

## Documentation

| Document | Contents |
|---|---|
| [Configuration](docs/configuration.md) | Virtual Schema properties, credentials, inference controls, and refresh behavior |
| [Data model](docs/data-model.md) | Root/child tables, relationships, variants, nulls, empty strings, and BSON-to-SQL types |
| [JSON-style SQL](docs/json-tables-sql.md) | Optional dot paths, array access and iteration, JSON helpers, and document reconstruction with Exasol JSON Tables |
| [Schema inference](docs/schema-inference.md) | Validator, index, and sampling evidence; budgets; determinism; permissions |
| [Query pushdown](docs/pushdown.md) | Advertised operations, exact MongoDB translations, and conservative fallbacks |
| [Aggregation pushdown](docs/aggregation-pushdown.md) | Current `COUNT(*)` contract, fallbacks, and roadmap for additional aggregates |
| [Development](docs/development.md) | Tooling, tests, coverage, Linux artifact build, and live E2E execution |

## Development

The fast local checks are:

```bash
make test
make check
```

Before submitting a change, run:

```bash
make quality
```

The quality gate enforces formatting, warning-free Clippy, ShellCheck,
dependency policy, tests, at least 90% line and region coverage, and at least
85% function coverage. See [Development and testing](docs/development.md) for setup and the
MongoDB/Exasol integration suites.

Pull requests are welcome. Please include tests for behavioral changes and keep
the no-capability path as the correctness reference for new pushdown features.

## License

This project is licensed under the [MIT License](LICENSE).
