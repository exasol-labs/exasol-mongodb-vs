CREATE SCHEMA IF NOT EXISTS MONGODB_VS;

CREATE OR REPLACE RUST ADAPTER SCRIPT MONGODB_VS.MONGODB_ADAPTER AS
%udf_object /buckets/bfsdefault/rust/libmongodb_vs.so;
/

CREATE OR REPLACE RUST SCALAR SCRIPT MONGODB_VS.MONGODB_SCAN(spec VARCHAR(2000000))
EMITS (...) AS
%udf_object /buckets/bfsdefault/rust/libmongodb_vs.so;
/

