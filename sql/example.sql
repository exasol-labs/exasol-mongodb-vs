CREATE OR REPLACE CONNECTION MONGODB_M1
TO 'mongodb://192.168.64.1:27018/?directConnection=true'
USER ''
IDENTIFIED BY '';

DROP VIRTUAL SCHEMA IF EXISTS MONGO_DEMO CASCADE;

-- Omit MANIFEST to infer one deterministic JSON-table family from collection
-- validators, indexes, and bounded samples. Pass an explicit MANIFEST when a
-- fixed user-owned contract is preferable.
CREATE VIRTUAL SCHEMA MONGO_DEMO
USING MONGODB_VS.MONGODB_ADAPTER WITH
  MONGODB_CONNECTION = 'MONGODB_M1'
  DATABASE = 'demo'
  COLLECTION = 'people'
  INFERENCE_SAMPLE_SIZE = '100'
  INFERENCE_MAX_BYTES = '8388608'
  BATCH_SIZE = '128';

SELECT "mongo_id", "name" FROM MONGO_DEMO."PEOPLE" ORDER BY "name";
EXPLAIN VIRTUAL SELECT "mongo_id", "name" FROM MONGO_DEMO."PEOPLE";
