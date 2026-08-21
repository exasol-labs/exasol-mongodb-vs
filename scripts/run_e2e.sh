#!/usr/bin/env bash
# End-to-end Milestones 1-3 verification for Exasol Personal on macOS.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOYMENT_DIR="${EXASOL_DEPLOYMENT_DIR:-$HOME/.exasol/personal/deployments/default}"
MONGO_IMAGE="${MONGODB_E2E_IMAGE:-mongo:8.0}"
MONGO_GATEWAY="${MONGODB_E2E_GATEWAY:-192.168.64.1}"
SKIP_BUILD="${MONGODB_E2E_SKIP_BUILD:-0}"
KEEP_RESOURCES="${MONGODB_E2E_KEEP_RESOURCES:-0}"
RUN_ID="${MONGODB_E2E_RUN_ID:-$(date +%s)-$$}"
RUN_TOKEN="$(printf '%s' "$RUN_ID" | tr -cd '[:alnum:]' | cut -c1-16)"
[[ -n "$RUN_TOKEN" ]] || RUN_TOKEN="$$"
RUN_TOKEN_UPPER="$(printf '%s' "$RUN_TOKEN" | tr '[:lower:]' '[:upper:]')"

MONGO_CONTAINER="${MONGODB_E2E_CONTAINER:-mongodb-vs-m1-${RUN_TOKEN}}"
MONGO_DATABASE="m1_${RUN_TOKEN}"
MONGO_COLLECTION="people_${RUN_TOKEN}"
VS_SCHEMA="MONGO_M1_${RUN_TOKEN_UPPER}"
INFERRED_SCHEMA="MONGO_M2_${RUN_TOKEN_UPPER}"
ORACLE_SCHEMA="MONGO_M3_ORACLE_${RUN_TOKEN_UPPER}"
CONNECTION_NAME="MONGODB_M1_${RUN_TOKEN_UPPER}"
INFERRED_ROOT="$(printf '%s' "$MONGO_COLLECTION" | tr '[:lower:]' '[:upper:]')"

for command in docker exasol jq scp; do
  command -v "$command" >/dev/null || {
    echo "error: required command not found: $command" >&2
    exit 1
  }
done

[[ -f "$DEPLOYMENT_DIR/deployment.json" ]] || {
  echo "error: Exasol deployment not found: $DEPLOYMENT_DIR" >&2
  exit 1
}

cleanup() {
  if [[ "$KEEP_RESOURCES" == "1" ]]; then
    echo "Keeping E2E resources: container=$MONGO_CONTAINER schema=$VS_SCHEMA connection=$CONNECTION_NAME"
    return
  fi
  exasol connect --deployment-dir "$DEPLOYMENT_DIR" --json=compact -c \
    "DROP VIRTUAL SCHEMA IF EXISTS \"$VS_SCHEMA\" CASCADE; DROP VIRTUAL SCHEMA IF EXISTS \"$INFERRED_SCHEMA\" CASCADE; DROP VIRTUAL SCHEMA IF EXISTS \"$ORACLE_SCHEMA\" CASCADE; DROP CONNECTION \"$CONNECTION_NAME\";" \
    >/dev/null 2>&1 || true
  docker rm -f "$MONGO_CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run -d --name "$MONGO_CONTAINER" -p 27017 "$MONGO_IMAGE" >/dev/null
MONGO_PORT="$(docker port "$MONGO_CONTAINER" 27017/tcp | head -n1 | awk -F: '{print $NF}')"

for _ in $(seq 1 30); do
  if docker exec "$MONGO_CONTAINER" mongosh --quiet --eval 'db.runCommand({ping:1}).ok' \
      2>/dev/null | grep -q '^1$'; then
    break
  fi
  sleep 1
done

docker exec "$MONGO_CONTAINER" mongosh --quiet --eval \
  "db=db.getSiblingDB('$MONGO_DATABASE');
  db.createCollection('$MONGO_COLLECTION', {validator: {
    \$jsonSchema: {bsonType:'object', required:['name'], properties: {
      name:{bsonType:'string'}, value:{bsonType:['int','long','string']},
      created_at:{bsonType:'date'},
      profile:{bsonType:'object',properties:{city:{bsonType:'string'}}},
      tags:{bsonType:'array',items:{bsonType:['string','null']}},
      poly:{bsonType:'array',items:{bsonType:['int','string']}},
      mixed:{bsonType:'array'},
      matrix:{bsonType:'array',items:{bsonType:'array',items:{bsonType:'int'}}},
      items:{bsonType:'array',items:{bsonType:'object',properties:{label:{bsonType:'string'},flags:{bsonType:'array',items:{bsonType:'bool'}}}}}
    }}
  }, validationAction:'warn', validationLevel:'moderate'});
  db.getCollection('$MONGO_COLLECTION').createIndex({'profile.city':1,value:-1},{name:'profile_value_partial',partialFilterExpression:{name:{\$exists:true}}});
  db.getCollection('$MONGO_COLLECTION').insertMany([
    {name:'Ada',empty_text:'',note:null,value:NumberLong('-9223372036854775808'),created_at:ISODate('2026-08-08T10:11:12.123Z'),profile:{city:'Copenhagen'},tags:['rust',null,'rust'],poly:[1,2,3],mixed:[[1,2],[3]],matrix:[[1,2],[3]],items:[{label:'one',flags:[true,false]},{label:'two',flags:[]}]},
    {name:'Grace',empty_text:'x',value:'forty-two',created_at:ISODate('2026-08-08T11:00:00.000Z'),profile:{},tags:[],poly:['a','b'],mixed:['mixed',7,8],matrix:[],items:[]},
    {name:'Linus',empty_text:'y',note:'present',value:NumberInt(7),created_at:ISODate('2026-08-08T12:00:00.000Z'),tags:['kernel'],poly:[],mixed:[],matrix:[[4]],items:[{label:'three',flags:[true]}]}
  ]);" >/dev/null

if [[ "$SKIP_BUILD" != "1" ]]; then
  make -C "$PROJECT_DIR" build-so
fi

SO="$PROJECT_DIR/target/release/libmongodb_vs.so"
"$PROJECT_DIR/scripts/verify_artifact.sh" "$SO"

SSH_PORT="$(jq -r '.connection.sshPort' "$DEPLOYMENT_DIR/deployment.json")"
SSH_KEY="$DEPLOYMENT_DIR/local/node_access.pem"
scp -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o LogLevel=ERROR -P "$SSH_PORT" "$SO" \
  root@127.0.0.1:/var/lib/exa/bucketfs/bfsdefault/rust/libmongodb_vs.so

exasol connect --deployment-dir "$DEPLOYMENT_DIR" --json=compact \
  -f "$PROJECT_DIR/sql/install.sql" >/dev/null

MANIFEST="$(jq -c . "$PROJECT_DIR/examples/people.source_manifest.json")"
MANIFEST_SQL="${MANIFEST//\'/\'\'}"
SQL="CREATE OR REPLACE CONNECTION \"$CONNECTION_NAME\"
TO 'mongodb://${MONGO_GATEWAY}:${MONGO_PORT}/?directConnection=true'
USER '' IDENTIFIED BY '';
CREATE VIRTUAL SCHEMA \"$VS_SCHEMA\"
USING MONGODB_VS.MONGODB_ADAPTER WITH
  MONGODB_CONNECTION='$CONNECTION_NAME'
  DATABASE='$MONGO_DATABASE'
  COLLECTION='$MONGO_COLLECTION'
  MANIFEST='$MANIFEST_SQL'
  BATCH_SIZE='2';
CREATE VIRTUAL SCHEMA \"$INFERRED_SCHEMA\"
USING MONGODB_VS.MONGODB_ADAPTER WITH
  MONGODB_CONNECTION='$CONNECTION_NAME'
  DATABASE='$MONGO_DATABASE'
  COLLECTION='$MONGO_COLLECTION'
  INFERENCE_SAMPLE_SIZE='100'
  INFERENCE_MAX_BYTES='1048576'
  INFERENCE_MAX_DEPTH='8'
  INFERENCE_ARRAY_ELEMENTS='32'
  INFERENCE_MAX_TIME_MS='5000'
  BATCH_SIZE='2';
CREATE VIRTUAL SCHEMA \"$ORACLE_SCHEMA\"
USING MONGODB_VS.MONGODB_ADAPTER WITH
  MONGODB_CONNECTION='$CONNECTION_NAME'
  DATABASE='$MONGO_DATABASE'
  COLLECTION='$MONGO_COLLECTION'
  MANIFEST='$MANIFEST_SQL'
  ENABLE_PUSHDOWN='false'
  BATCH_SIZE='2';
SELECT \"name\", \"empty_text\", \"empty_text|empty\", \"note\", \"note|n\", \"value\", \"value|string\"
FROM \"$VS_SCHEMA\".\"PEOPLE\" ORDER BY \"name\";
SELECT r.\"name\", p.\"city\"
FROM \"$VS_SCHEMA\".\"PEOPLE\" r
JOIN \"$VS_SCHEMA\".\"PEOPLE_profile\" p ON p.\"_id\" = r.\"profile|object\"
ORDER BY r.\"name\";
SELECT r.\"name\", t.\"_pos\", t.\"_value\", t.\"_value|n\"
FROM \"$VS_SCHEMA\".\"PEOPLE\" r
JOIN \"$VS_SCHEMA\".\"PEOPLE_tags_arr\" t ON t.\"_parent\" = r.\"_id\"
ORDER BY r.\"name\", t.\"_pos\";
SELECT r.\"name\", i.\"_pos\" AS item_pos, f.\"_pos\" AS flag_pos, f.\"_value\"
FROM \"$VS_SCHEMA\".\"PEOPLE\" r
JOIN \"$VS_SCHEMA\".\"PEOPLE_items_arr\" i ON i.\"_parent\" = r.\"_id\"
JOIN \"$VS_SCHEMA\".\"PEOPLE_items_arr_flags_arr\" f ON f.\"_parent\" = i.\"_id\"
ORDER BY r.\"name\", i.\"_pos\", f.\"_pos\";
SELECT \"name\", \"created_at\" FROM \"$VS_SCHEMA\".\"PEOPLE\" ORDER BY \"name\";
EXPLAIN VIRTUAL SELECT * FROM \"$VS_SCHEMA\".\"PEOPLE_tags_arr\";
SELECT \"name\" AS \"inferred_name\", \"empty_text|empty\" AS \"inferred_empty\", \"value\", \"value|string\"
FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\" ORDER BY \"name\";
SELECT r.\"name\" AS \"inferred_parent\", i.\"_pos\" AS \"inferred_item_pos\", f.\"_pos\" AS \"inferred_flag_pos\", f.\"_value\" AS \"inferred_flag\"
FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\" r
JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_items_arr\" i ON i.\"_parent\" = r.\"_id\"
JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_items_arr_flags_arr\" f ON f.\"_parent\" = i.\"_id\"
ORDER BY r.\"name\", i.\"_pos\", f.\"_pos\";
SELECT r.\"name\" AS \"matrix_parent\", outer_values.\"_pos\" AS \"outer_pos\", inner_values.\"_pos\" AS \"inner_pos\", inner_values.\"_value\" AS \"matrix_value\"
FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\" r
JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_matrix_arr\" outer_values ON outer_values.\"_parent\" = r.\"_id\"
JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_matrix_arr_value_arr\" inner_values ON inner_values.\"_parent\" = outer_values.\"_id\"
ORDER BY r.\"name\", outer_values.\"_pos\", inner_values.\"_pos\";
SELECT r.\"name\" AS \"poly_parent\", p.\"_pos\" AS \"poly_pos\", p.\"_value|string\" AS \"poly_string\"
FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\" r JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_poly_arr\" p ON p.\"_parent\" = r.\"_id\" ORDER BY r.\"name\", p.\"_pos\";
SELECT r.\"name\" AS \"poly_parent\", p.\"_pos\" AS \"poly_pos\", p.\"_value\" AS \"poly_number\"
FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\" r JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_poly_arr\" p ON p.\"_parent\" = r.\"_id\" ORDER BY r.\"name\", p.\"_pos\";
SELECT r.\"name\" AS \"poly_parent\", p.\"_pos\" AS \"poly_pos\", p.\"_value\" AS \"poly_number\", p.\"_value|string\" AS \"poly_string\"
FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\" r JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_poly_arr\" p ON p.\"_parent\" = r.\"_id\" ORDER BY r.\"name\", p.\"_pos\";
SELECT r.\"name\" AS \"mixed_parent\", outer_values.\"_pos\" AS \"outer_pos\", inner_values.\"_pos\" AS \"inner_pos\", inner_values.\"_value\" AS \"mixed_value\"
FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\" r
JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_mixed_arr\" outer_values ON outer_values.\"_parent\" = r.\"_id\"
JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_mixed_arr_value_arr\" inner_values ON inner_values.\"_parent\" = outer_values.\"_id\"
ORDER BY r.\"name\", outer_values.\"_pos\", inner_values.\"_pos\";
SELECT r.\"name\" AS \"mixed_outer_parent\", outer_values.\"_pos\" AS \"mixed_outer_pos\",
       outer_values.\"_value\" AS \"mixed_outer_number\", outer_values.\"_value|string\" AS \"mixed_outer_string\",
       outer_values.\"_value|array\" AS \"mixed_outer_array_length\"
FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\" r
JOIN \"$INFERRED_SCHEMA\".\"${INFERRED_ROOT}_mixed_arr\" outer_values ON outer_values.\"_parent\" = r.\"_id\"
ORDER BY r.\"name\", outer_values.\"_pos\";
EXPLAIN VIRTUAL SELECT * FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\";
ALTER VIRTUAL SCHEMA \"$VS_SCHEMA\" REFRESH;
ALTER VIRTUAL SCHEMA \"$INFERRED_SCHEMA\" REFRESH;
EXPLAIN VIRTUAL SELECT * FROM \"$INFERRED_SCHEMA\".\"$INFERRED_ROOT\";
SELECT COUNT(*) AS C FROM \"$VS_SCHEMA\".\"PEOPLE\";
SELECT COUNT(*) AS ROOT_COUNT FROM \"$VS_SCHEMA\".\"PEOPLE\";
SELECT COUNT(*) AS FILTERED_COUNT FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" BETWEEN 0 AND 7;
SELECT COUNT(*) AS EMPTY_COUNT FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" BETWEEN 100 AND 200;
SELECT COUNT(*) AS STRING_FILTER_COUNT FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"name\" = 'Ada';
SELECT COUNT(\"note\") AS NOTE_COUNT FROM \"$VS_SCHEMA\".\"PEOPLE\";
SELECT COUNT(*) AS NESTED_COUNT FROM \"$VS_SCHEMA\".\"PEOPLE_tags_arr\";
SELECT \"name\" AS \"or_exact_name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" = 7 OR \"value\" = -9223372036854775808 ORDER BY \"name\";
SELECT \"name\" AS \"or_oracle_name\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"value\" = 7 OR \"value\" = -9223372036854775808 ORDER BY \"name\";
SELECT \"name\" AS \"not_exact_name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE NOT (\"value\" BETWEEN 0 AND 7) ORDER BY \"name\";
SELECT \"name\" AS \"not_oracle_name\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE NOT (\"value\" BETWEEN 0 AND 7) ORDER BY \"name\";
SELECT \"name\" AS \"or_fallback_name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" = 7 OR \"name\" = 'Ada' ORDER BY \"name\";
SELECT \"name\" AS \"or_fallback_oracle_name\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"value\" = 7 OR \"name\" = 'Ada' ORDER BY \"name\";
SELECT COUNT(*) AS OR_COUNT FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" = 7 OR \"value\" = -9223372036854775808;
SELECT \"name\", \"value\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" IS NOT NULL AND \"value\" <= 7 ORDER BY \"value\" DESC NULLS LAST LIMIT 2;
SELECT \"name\", \"value\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"value\" IS NOT NULL AND \"value\" <= 7 ORDER BY \"value\" DESC NULLS LAST LIMIT 2;
SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" IN (7, -9223372036854775808) ORDER BY \"name\";
SELECT \"name\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"value\" IN (7, -9223372036854775808) ORDER BY \"name\";
SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" IS NULL ORDER BY \"name\";
SELECT \"name\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"value\" IS NULL ORDER BY \"name\";
SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"note|n\" = TRUE ORDER BY \"name\";
SELECT \"name\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"note|n\" = TRUE ORDER BY \"name\";
SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" LIMIT 2;
SELECT \"name\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" LIMIT 2;
SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"name\" = 'Ada' LIMIT 1;
SELECT \"name\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"name\" = 'Ada' LIMIT 1;
SELECT \"name\" AS \"between_pushed_name\", \"value\" AS \"between_pushed_value\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" BETWEEN 0 AND 7 ORDER BY \"name\";
SELECT \"name\" AS \"between_oracle_name\", \"value\" AS \"between_oracle_value\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"value\" BETWEEN 0 AND 7 ORDER BY \"name\";
SELECT \"name\" AS \"inclusive_range_name\", \"value\" AS \"inclusive_range_value\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" >= 0 AND \"value\" <= 7 ORDER BY \"name\";
EXPLAIN VIRTUAL SELECT \"name\", \"value\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" IS NOT NULL ORDER BY \"value\" DESC NULLS LAST LIMIT 2;
EXPLAIN VIRTUAL SELECT \"name\", \"value\" FROM \"$ORACLE_SCHEMA\".\"PEOPLE\" WHERE \"value\" IS NOT NULL ORDER BY \"value\" DESC NULLS LAST LIMIT 2;
EXPLAIN VIRTUAL SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"name\" = 'Ada' LIMIT 1;
EXPLAIN VIRTUAL SELECT \"name\", \"value\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" BETWEEN 0 AND 7;
EXPLAIN VIRTUAL SELECT \"name\", \"value\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" >= 0 AND \"value\" <= 7;
EXPLAIN VIRTUAL SELECT COUNT(*) FROM \"$VS_SCHEMA\".\"PEOPLE\";
EXPLAIN VIRTUAL SELECT COUNT(*) FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" BETWEEN 0 AND 7;
EXPLAIN VIRTUAL SELECT COUNT(*) FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"name\" = 'Ada';
EXPLAIN VIRTUAL SELECT COUNT(\"note\") FROM \"$VS_SCHEMA\".\"PEOPLE\";
EXPLAIN VIRTUAL SELECT COUNT(*) FROM \"$VS_SCHEMA\".\"PEOPLE_tags_arr\";
EXPLAIN VIRTUAL SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" = 7 OR \"value\" = -9223372036854775808;
EXPLAIN VIRTUAL SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE NOT (\"value\" BETWEEN 0 AND 7);
EXPLAIN VIRTUAL SELECT \"name\" FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" = 7 OR \"name\" = 'Ada';
EXPLAIN VIRTUAL SELECT COUNT(*) FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"value\" = 7 OR \"value\" = -9223372036854775808;"

set +e
RESULT="$(exasol connect --deployment-dir "$DEPLOYMENT_DIR" --json=compact -c "$SQL")"
EXASOL_STATUS=$?
set -e
if [[ "$EXASOL_STATUS" != "0" ]]; then
  echo "error: Exasol E2E statement failed:" >&2
  jq -r '.statements[] | select(.error != null) | .error.message' <<<"$RESULT" >&2 || true
  exit "$EXASOL_STATUS"
fi

if ! jq -e '
  ([.statements[] | select(.columns == ["name","empty_text","empty_text|empty","note","note|n","value","value|string"])][0].rows) == [
    ["Ada", null, true, null, true, "-9223372036854775808", null],
    ["Grace", "x", false, null, false, null, "forty-two"],
    ["Linus", "y", false, "present", false, "7", null]
  ]
' <<<"$RESULT" >/dev/null; then
  echo "error: root/variant/null result assertion failed" >&2
  jq -c '.statements[] | select(.columns | length > 0) | {columns,rows,error}' <<<"$RESULT" >&2
  exit 1
fi

if ! jq -e '
  ([.statements[] | select(.columns == ["name","city"])][0].rows) == [["Ada","Copenhagen"],["Grace",null]]
  and ([.statements[] | select(.columns == ["name","_pos","_value","_value|n"])][0].rows) == [
    ["Ada",0,"rust",false],["Ada",1,null,true],["Ada",2,"rust",false],["Linus",0,"kernel",false]
  ]
  and ([.statements[] | select(.columns == ["name","ITEM_POS","FLAG_POS","_value"])][0].rows) == [
    ["Ada",0,0,true],["Ada",0,1,false],["Linus",0,0,true]
  ]
  and ([.statements[] | select(.columns == ["name","created_at"])][0].rows) == [
    ["Ada","2026-08-08 10:11:12.123000"],
    ["Grace","2026-08-08 11:00:00.000000"],
    ["Linus","2026-08-08 12:00:00.000000"]
  ]
  and ([.statements[] | select(.columns == ["C"])][0].rows[0][0]) == 3
  and ([.statements[] | select(.columns == ["ROOT_COUNT"])][0].rows[0][0]) == 3
  and ([.statements[] | select(.columns == ["FILTERED_COUNT"])][0].rows[0][0]) == 1
  and ([.statements[] | select(.columns == ["EMPTY_COUNT"])][0].rows[0][0]) == 0
  and ([.statements[] | select(.columns == ["STRING_FILTER_COUNT"])][0].rows[0][0]) == 1
  and ([.statements[] | select(.columns == ["NOTE_COUNT"])][0].rows[0][0]) == 1
  and ([.statements[] | select(.columns == ["NESTED_COUNT"])][0].rows[0][0]) == 4
  and ([.statements[] | select(.columns == ["OR_COUNT"])][0].rows[0][0]) == 2
  and ([.statements[] | select(.columns == ["or_exact_name"])][0].rows) == [["Ada"],["Linus"]]
  and ([.statements[] | select(.columns == ["or_oracle_name"])][0].rows) == [["Ada"],["Linus"]]
  and ([.statements[] | select(.columns == ["not_exact_name"])][0].rows) == [["Ada"]]
  and ([.statements[] | select(.columns == ["not_oracle_name"])][0].rows) == [["Ada"]]
  and ([.statements[] | select(.columns == ["or_fallback_name"])][0].rows) == [["Ada"],["Linus"]]
  and ([.statements[] | select(.columns == ["or_fallback_oracle_name"])][0].rows) == [["Ada"],["Linus"]]
  and ([.statements[] | select(.columns == ["inferred_name","inferred_empty","value","value|string"])][0].rows) == [
    ["Ada",true,"-9223372036854775808",null],
    ["Grace",false,null,"forty-two"],
    ["Linus",false,"7",null]
  ]
  and ([.statements[] | select(.columns == ["inferred_parent","inferred_item_pos","inferred_flag_pos","inferred_flag"])][0].rows) == [
    ["Ada",0,0,true],["Ada",0,1,false],["Linus",0,0,true]
  ]
  and ([.statements[] | select(.columns == ["matrix_parent","outer_pos","inner_pos","matrix_value"])][0].rows) == [
    ["Ada",0,0,1],["Ada",0,1,2],["Ada",1,0,3],["Linus",0,0,4]
  ]
  and ([.statements[] | select(.columns == ["poly_parent","poly_pos","poly_string"])][0].rows) == [
    ["Ada",0,null],["Ada",1,null],["Ada",2,null],["Grace",0,"a"],["Grace",1,"b"]
  ]
  and ([.statements[] | select(.columns == ["poly_parent","poly_pos","poly_number"])][0].rows) == [
    ["Ada",0,1],["Ada",1,2],["Ada",2,3],["Grace",0,null],["Grace",1,null]
  ]
  and ([.statements[] | select(.columns == ["poly_parent","poly_pos","poly_number","poly_string"])][0].rows) == [
    ["Ada",0,1,null],["Ada",1,2,null],["Ada",2,3,null],["Grace",0,null,"a"],["Grace",1,null,"b"]
  ]
  and ([.statements[] | select(.columns == ["mixed_parent","outer_pos","inner_pos","mixed_value"])][0].rows) == [
    ["Ada",0,0,1],["Ada",0,1,2],["Ada",1,0,3]
  ]
  and ([.statements[] | select(.columns == ["mixed_outer_parent","mixed_outer_pos","mixed_outer_number","mixed_outer_string","mixed_outer_array_length"])][0].rows) == [
    ["Ada",0,null,null,2],["Ada",1,null,null,1],["Grace",0,null,"mixed",null],["Grace",1,7,null,null],["Grace",2,8,null,null]
  ]
  and ([.statements[] | select(.columns == ["between_pushed_name","between_pushed_value"])][0].rows) == [["Linus","7"]]
  and ([.statements[] | select(.columns == ["between_oracle_name","between_oracle_value"])][0].rows) == [["Linus","7"]]
  and ([.statements[] | select(.columns == ["inclusive_range_name","inclusive_range_value"])][0].rows) == [["Linus","7"]]
  and ([.statements[] | select(.columns == ["name","value"])] | map(.rows)) == [
    [["Linus","7"],["Ada","-9223372036854775808"]],
    [["Linus","7"],["Ada","-9223372036854775808"]]
  ]
  and ([.statements[] | select(.columns == ["name"])] | map(.rows)) == [
    [["Ada"],["Linus"]], [["Ada"],["Linus"]],
    [["Grace"]], [["Grace"]],
    [["Ada"]], [["Ada"]],
    [["Ada"],["Grace"]], [["Ada"],["Grace"]],
    [["Ada"]], [["Ada"]]
  ]
' <<<"$RESULT" >/dev/null; then
  echo "error: relationship/order/refresh result assertion failed" >&2
  jq -c '.statements[] | select(.columns | length > 0) | {columns,rows,error}' <<<"$RESULT" >&2
  exit 1
fi

PUSHDOWN_SQL="$(jq -r '.statements[] | select(.statementType == "EXPLAIN") | .rows[0][1]' <<<"$RESULT")"
grep -q 'MONGODB_SCAN' <<<"$PUSHDOWN_SQL"
grep -q 'nested' <<<"$PUSHDOWN_SQL"
if grep -Eqi 'mongodb://|password|identified by' <<<"$PUSHDOWN_SQL"; then
  echo "error: generated pushdown SQL contains connection credentials or URI" >&2
  exit 1
fi

if ! jq -e '
  [.statements[] | select(.statementType == "EXPLAIN") | .rows[0][1]] as $plans
  | ($plans | length) == 17 and $plans[1] == $plans[2]
    and ($plans[3] | contains("\"order_by\""))
    and ($plans[3] | contains("\"limit\":2"))
    and ($plans[4] | contains("\"pushdown\":{}"))
    and ($plans[5] | contains("WHERE (\"name\" ="))
    and ($plans[5] | contains("LIMIT 1"))
    and ($plans[5] | contains("\"pushdown\":{\"prefilter\":{"))
    and ($plans[6] | contains("\"op\":\"greater_equal\""))
    and ($plans[6] | contains("\"op\":\"less_equal\""))
    and ($plans[7] | contains("\"op\":\"greater_equal\""))
    and ($plans[7] | contains("\"op\":\"less_equal\""))
    and ($plans[8] | contains("\"aggregation\":{\"kind\":\"count_star\"}"))
    and ($plans[8] | contains("EMITS (\"__jt_count\" DECIMAL(18,0))"))
    and ($plans[9] | contains("\"aggregation\":{\"kind\":\"count_star\"}"))
    and ($plans[9] | contains("\"op\":\"greater_equal\""))
    and ($plans[9] | contains("\"op\":\"less_equal\""))
    and ($plans[10] | contains("SELECT COUNT(*)"))
    and ($plans[10] | contains("WHERE (\"name\" ="))
    and ($plans[10] | contains("\"pushdown\":{\"prefilter\":{"))
    and ($plans[11] | contains("SELECT COUNT(\"note\")"))
    and ($plans[11] | contains("\"pushdown\":{}"))
    and ($plans[12] | contains("\"aggregation\":{\"kind\":\"count_star\"}"))
    and ($plans[12] | contains("\"kind\":\"nested\""))
    and ($plans[13] | contains("\"kind\":\"or\""))
    and ($plans[14] | contains("\"kind\":\"not\""))
    and ($plans[15] | contains(" OR "))
    and ($plans[15] | contains("\"pushdown\":{\"prefilter\":{"))
    and ($plans[16] | contains("\"kind\":\"or\""))
    and ($plans[16] | contains("\"aggregation\":{\"kind\":\"count_star\"}"))
' <<<"$RESULT" >/dev/null; then
  echo "error: refresh or Milestone 3 EXPLAIN delegation assertion failed" >&2
  jq -c '[.statements[] | select(.statementType == "EXPLAIN") | .rows[0][1]]' <<<"$RESULT" >&2
  exit 1
fi

docker exec "$MONGO_CONTAINER" mongosh --quiet --eval \
  "db=db.getSiblingDB('$MONGO_DATABASE'); db.getCollection('$MONGO_COLLECTION').insertOne({name:'Drift',value:true});" \
  >/dev/null
set +e
DRIFT_RESULT="$(exasol connect --deployment-dir "$DEPLOYMENT_DIR" --json=compact -c \
  "SELECT \"value\" FROM \"$VS_SCHEMA\".\"PEOPLE\";")"
DRIFT_STATUS=$?
set -e
if [[ "$DRIFT_STATUS" == "0" ]] || ! jq -e '
  any(.statements[]; .error.message? | contains("unadvertised BSON type boolean"))
' <<<"$DRIFT_RESULT" >/dev/null; then
  echo "error: unadvertised BSON drift did not fail with the expected safe diagnostic" >&2
  jq -c '.statements[] | {rows,error}' <<<"$DRIFT_RESULT" >&2 || true
  exit 1
fi

# The opaque source-document projection must remain usable for the very row
# that the inferred scalar projection rejects. This is the contract consumed
# by Exasol JSON Tables' zero-argument TO_JSON() rewrite.
DRIFT_JSON_RESULT="$(exasol connect --deployment-dir "$DEPLOYMENT_DIR" --json=compact -c \
  "SELECT \"__mongodb_source_json\" AS SOURCE_JSON FROM \"$VS_SCHEMA\".\"PEOPLE\" WHERE \"name\" = 'Drift';")"
if ! jq -e '
  ([.statements[] | select(.columns == ["SOURCE_JSON"])][0].rows[0][0] | fromjson)
    | (.name == "Drift" and .value == true and (._id | has("$oid")))
' <<<"$DRIFT_JSON_RESULT" >/dev/null; then
  echo "error: source-document projection did not preserve the post-inference outlier" >&2
  jq -c '.statements[] | {columns,rows,error}' <<<"$DRIFT_JSON_RESULT" >&2 || true
  exit 1
fi

echo "E2E passed: JSON-table families, full source JSON including drift outliers, inference, conservative boolean/filter/limit/top-N/count pushdown, SQL three-valued NOT semantics, aggregate fallbacks, stable refresh/joins, drift failure, and secret-free EXPLAIN VIRTUAL."
