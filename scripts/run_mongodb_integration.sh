#!/usr/bin/env bash

set -euo pipefail

mongo_image="${MONGODB_INTEGRATION_IMAGE:-mongo:8.0}"
run_id="${MONGODB_INTEGRATION_RUN_ID:-$(date +%s)-$$}"
run_token="$(printf '%s' "$run_id" | tr -cd '[:alnum:]' | cut -c1-16)"
container="mongodb-vs-inference-${run_token:-$$}"

for command in docker cargo; do
  command -v "$command" >/dev/null || {
    echo "error: required command not found: $command" >&2
    exit 1
  }
done

cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run -d --name "$container" -p 27017 \
  -e MONGO_INITDB_ROOT_USERNAME=root \
  -e MONGO_INITDB_ROOT_PASSWORD=secret \
  "$mongo_image" >/dev/null
mongo_port="$(docker port "$container" 27017/tcp | head -n1 | awk -F: '{print $NF}')"

for _ in $(seq 1 30); do
  if docker exec "$container" mongosh --quiet \
      -u root -p secret --authenticationDatabase admin \
      --eval 'db.runCommand({ping:1}).ok' 2>/dev/null | grep -q '^1$'; then
    break
  fi
  sleep 1
done

docker exec "$container" mongosh --quiet \
  -u root -p secret --authenticationDatabase admin --eval '
    db = db.getSiblingDB("inference");
    db.createCollection("people", {
      validator: {$jsonSchema: {
        bsonType: "object",
        required: ["name"],
        properties: {
          name: {bsonType: "string"},
          age: {bsonType: ["int", "long", "null"]},
          profile: {bsonType: "object", properties: {city: {bsonType: "string"}}},
          items: {bsonType: "array", items: {bsonType: "object", properties: {label: {bsonType: "string"}}}}
        }
      }},
      validationAction: "warn",
      validationLevel: "moderate"
    });
    db.people.createIndex({email: 1}, {name: "email_unique", unique: true, sparse: true});
    db.people.createIndex({"profile.city": 1, age: -1}, {name: "profile_age_partial", partialFilterExpression: {active: true}});
    db.people.createIndex({"account.id": 1}, {name: "account_type_partial", partialFilterExpression: {"account.id": {$type: "string"}}});
    db.people.createIndex({age: 1}, {name: "unsafe_or_partial", partialFilterExpression: {$or: [{unsafe: {$type: "string"}}, {unsafe: {$type: "int"}}]}});
    db.people.createIndex({name: "text"}, {name: "name_text"});
    db.people.insertMany([
      {name: "Ada", age: NumberLong(42), profile: {city: "Copenhagen"}, items: [{label: "one"}]},
      {name: "Grace", age: "unknown", profile: {}, items: []},
      {name: "Linus", age: null}
    ]);
    db.createCollection("deterministic_samples");
    const deterministicSamples = [];
    for (let id = 0; id < 30; id++) {
      deterministicSamples.push({
        _id: id,
        branch: id % 3 === 0 ? id : (id % 3 === 1 ? `value-${id}` : {nested: id})
      });
    }
    db.deterministic_samples.insertMany(deterministicSamples);
    db.createCollection("double_pushdown");
    const doublePushdown = [];
    for (let id = 0; id < 1000; id++) {
      doublePushdown.push({_id: id, score: id + 0.5});
    }
    doublePushdown.push({_id: 1000, score: NumberInt(100)});
    doublePushdown.push({_id: 1001, score: "100.5"});
    doublePushdown.push({_id: 1002, score: NaN});
    doublePushdown.push({_id: 1003, score: Infinity});
    doublePushdown.push({_id: 1004, score: -Infinity});
    db.double_pushdown.insertMany(doublePushdown);
    db.double_pushdown.createIndex({score: 1}, {name: "score_double"});
    db.createRole({
      role: "sampleOnly",
      privileges: [{resource: {db: "inference", collection: "people"}, actions: ["find"]}],
      roles: []
    });
    db.createUser({user: "sample", pwd: "sample-secret", roles: ["sampleOnly"]});
  ' >/dev/null

export MONGODB_INTEGRATION_ROOT_URI="mongodb://root:secret@127.0.0.1:${mongo_port}/?authSource=admin&directConnection=true"
export MONGODB_INTEGRATION_LIMITED_URI="mongodb://sample:sample-secret@127.0.0.1:${mongo_port}/inference?authSource=inference&directConnection=true"
cargo test --locked -p mongodb-vs --test discovery_integration -- --ignored --exact \
  discovers_validator_indexes_samples_and_permission_gaps
