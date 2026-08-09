# Configuration reference

The adapter is configured with properties on `CREATE VIRTUAL SCHEMA`. Property
values are strings, following the Exasol Virtual Schema protocol.

## Required properties

| Property | Meaning |
|---|---|
| `MONGODB_CONNECTION` | Name of an Exasol connection containing the MongoDB URI and, optionally, credentials. |
| `DATABASE` | Exact MongoDB database name. |
| `COLLECTION` | Exact MongoDB collection name. |

## Optional properties

| Property | Default | Meaning |
|---|---:|---|
| `MANIFEST` | inferred | JSON string containing an explicit `exasol-json-tables-source-manifest` version 1 contract. When omitted, the collection is inferred. Despite the format name, using the separate `exasol-json-tables` project is not required. |
| `BATCH_SIZE` | `128` | Positive MongoDB cursor batch size. |
| `ENABLE_PUSHDOWN` | `true` | Advertise supported query-pushdown capabilities. Set to `false` for operational rollback or result-oracle testing. |
| `INFERENCE_SAMPLE_SIZE` | `100` | Maximum sampled documents, from `0` to `10000`. Use `0` for validator/index-only inference. |
| `INFERENCE_MAX_BYTES` | `8388608` | Maximum encoded bytes inspected across sampled documents; maximum `67108864`. |
| `INFERENCE_MAX_DEPTH` | `8` | Maximum inferred nesting depth; maximum `32`. |
| `INFERENCE_ARRAY_ELEMENTS` | `32` | Distributed positions inspected per sampled array; maximum `1000`. |
| `INFERENCE_MAX_TIME_MS` | `5000` | MongoDB time limit per metadata, index, or sampling operation; maximum `60000`. |

## Connections and credentials

The connection address is a MongoDB URI:

```sql
CREATE OR REPLACE CONNECTION MONGODB_CONNECTION
TO 'mongodb://mongo.example.net:27017/?authSource=admin&replicaSet=rs0'
USER 'analytics_reader'
IDENTIFIED BY 'secret';
```

When Exasol connection USER and PASSWORD values are present, they override
credentials embedded in the URI. Prefer the connection fields so generated SQL
and ordinary configuration remain free of secrets.

Credentials are resolved only during schema inference and inside the scan UDF.
They are not serialized into adapter notes or scan plans.

## Inferred schema

The minimal inferred configuration is:

```sql
CREATE VIRTUAL SCHEMA MONGO_DEMO
USING MONGODB_VS.MONGODB_ADAPTER WITH
  MONGODB_CONNECTION = 'MONGODB_CONNECTION'
  DATABASE = 'demo'
  COLLECTION = 'people';
```

Use `ALTER VIRTUAL SCHEMA ... REFRESH` after intentional source-schema changes.
An unchanged evidence contract produces deterministic metadata and fingerprint.

## Explicit schema

An explicit manifest freezes the exposed tables and column types:

```sql
CREATE VIRTUAL SCHEMA MONGO_DEMO
USING MONGODB_VS.MONGODB_ADAPTER WITH
  MONGODB_CONNECTION = 'MONGODB_CONNECTION'
  DATABASE = 'demo'
  COLLECTION = 'people'
  MANIFEST = '{...manifest JSON...}';
```

See [`../examples/people.source_manifest.json`](../examples/people.source_manifest.json)
for a complete example and [Data model](data-model.md) for its semantics.

Explicit-manifest creation performs no MongoDB I/O. MongoDB is contacted only
when a virtual table is queried.

## Changing properties

Use Exasol's `ALTER VIRTUAL SCHEMA ... SET` and `REFRESH` operations according to
your Exasol version. Changes to connection, namespace, manifest, or inference
budgets rebuild the schema metadata. Invalid or out-of-range values fail with a
user-facing configuration error.
