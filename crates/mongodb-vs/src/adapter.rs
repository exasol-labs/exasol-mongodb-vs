use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};

use exasol_udf_sdk::context::UdfContext;
use exasol_udf_sdk::error::UdfError;

use crate::connection;
use crate::discovery::{self, InferenceConfig, InferenceReport};
use crate::model::{ExplicitManifest, TableModel};
use crate::mongo_plan::MongoReadPlan;
use crate::pushdown::{self, CAPABILITIES, MongoPushdown};
use crate::wire::{MAX_SCAN_SPEC_BYTES, MongoScanSpec, SCAN_SPEC_VERSION};

const NOTES_VERSION: u32 = 1;
const MAX_ADAPTER_NOTES_BYTES: usize = 1_800_000;
const MAX_GENERATED_SQL_BYTES: usize = 2_000_000;
const PROP_CONNECTION: &str = "MONGODB_CONNECTION";
const PROP_DATABASE: &str = "DATABASE";
const PROP_COLLECTION: &str = "COLLECTION";
const PROP_MANIFEST: &str = "MANIFEST";
const PROP_BATCH_SIZE: &str = "BATCH_SIZE";
const PROP_SAMPLE_SIZE: &str = "INFERENCE_SAMPLE_SIZE";
const PROP_MAX_BYTES: &str = "INFERENCE_MAX_BYTES";
const PROP_MAX_DEPTH: &str = "INFERENCE_MAX_DEPTH";
const PROP_ARRAY_ELEMENTS: &str = "INFERENCE_ARRAY_ELEMENTS";
const PROP_MAX_TIME_MS: &str = "INFERENCE_MAX_TIME_MS";
const PROP_ENABLE_PUSHDOWN: &str = "ENABLE_PUSHDOWN";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AdapterNotes {
    version: u32,
    connection_name: String,
    database: String,
    collection: String,
    manifest: ExplicitManifest,
    batch_size: u32,
    #[serde(default = "default_true")]
    pushdown_enabled: bool,
    #[serde(default)]
    inference_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inference_report: Option<InferenceReport>,
}

struct AdapterConfig {
    connection_name: String,
    database: String,
    collection: String,
    manifest: Option<ExplicitManifest>,
    batch_size: u32,
    inference: InferenceConfig,
    pushdown_enabled: bool,
}

pub fn adapter_call(ctx: &mut dyn UdfContext, input: &str) -> Result<String, UdfError> {
    let request: Json = serde_json::from_str(input).map_err(|error| {
        UdfError::User(format!("Virtual Schema request is invalid JSON: {error}"))
    })?;
    Ok(dispatch(ctx, &request)?.to_string())
}

fn dispatch(ctx: &mut dyn UdfContext, request: &Json) -> Result<Json, UdfError> {
    match request.get("type").and_then(Json::as_str) {
        Some("getCapabilities") => {
            let enabled =
                property_bool(&effective_properties(request), PROP_ENABLE_PUSHDOWN, true)?;
            Ok(
                json!({"type": "getCapabilities", "capabilities": if enabled { CAPABILITIES } else { &[] }}),
            )
        }
        Some("createVirtualSchema") | Some("refresh") | Some("setProperties") => {
            create_or_refresh(ctx, request)
        }
        Some("dropVirtualSchema") => Ok(json!({"type": "dropVirtualSchema"})),
        Some("pushdown") => pushdown(ctx, request),
        other => Err(UdfError::User(format!(
            "unsupported Virtual Schema request type '{}'",
            other.unwrap_or("(missing)")
        ))),
    }
}

fn create_or_refresh(ctx: &dyn UdfContext, request: &Json) -> Result<Json, UdfError> {
    let props = if request.get("type").and_then(Json::as_str) == Some("setProperties") {
        merge_set_properties(request)
    } else {
        effective_properties(request)
    };
    let config = parse_config(&props)?;
    let (manifest, inference_fingerprint, inference_report) = if let Some(manifest) =
        config.manifest
    {
        (manifest, String::new(), None)
    } else {
        let resolved = connection::resolve(ctx, &config.connection_name)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| UdfError::User("failed to create the MongoDB discovery runtime".into()))?;
        let result = runtime.block_on(discovery::infer(
            &resolved,
            &config.database,
            &config.collection,
            &config.inference,
        ))?;
        (result.manifest, result.fingerprint, Some(result.report))
    };
    let notes = AdapterNotes {
        version: NOTES_VERSION,
        connection_name: config.connection_name,
        database: config.database,
        collection: config.collection,
        manifest,
        batch_size: config.batch_size,
        pushdown_enabled: config.pushdown_enabled,
        inference_fingerprint,
        inference_report,
    };
    let models = notes.manifest.models()?;
    let tables = models.iter().map(table_metadata).collect::<Vec<_>>();
    let serialized_notes = serde_json::to_string(&notes)
        .map_err(|error| UdfError::User(format!("failed to serialize adapter notes: {error}")))?;
    if serialized_notes.len() > MAX_ADAPTER_NOTES_BYTES {
        return Err(UdfError::User(format!(
            "adapterNotes are {} bytes; maximum is {MAX_ADAPTER_NOTES_BYTES}",
            serialized_notes.len()
        )));
    }

    let response_type = request
        .get("type")
        .and_then(Json::as_str)
        .unwrap_or("createVirtualSchema");
    let mut response = json!({
        "type": response_type,
        "schemaMetadata": {
            "tables": tables,
            "adapterNotes": serialized_notes,
        },
    });
    if let Some(requested) = request.get("requestedTables") {
        response["requestedTables"] = requested.clone();
    }
    Ok(response)
}

fn table_metadata(table: &TableModel) -> Json {
    let columns = table
        .columns
        .iter()
        .map(|column| {
            json!({
                "name": column.exasol_name,
                "dataType": column.sql_type.metadata_type(),
            })
        })
        .collect::<Vec<_>>();
    json!({"name": table.table_name, "columns": columns})
}

fn pushdown(ctx: &dyn UdfContext, request: &Json) -> Result<Json, UdfError> {
    let notes = notes_from_request(request)?;
    let involved = request
        .get("involvedTables")
        .and_then(Json::as_array)
        .and_then(|tables| tables.first())
        .and_then(|table| table.get("name"))
        .and_then(Json::as_str)
        .ok_or_else(|| UdfError::User("pushdown request has no involved table".into()))?;
    let table = notes
        .manifest
        .models()?
        .into_iter()
        .find(|table| table.table_name == involved)
        .ok_or_else(|| {
            UdfError::User(format!(
                "pushdown table '{involved}' is not present in the resolved manifest"
            ))
        })?;

    let all_columns = table.columns.clone();
    let query = notes
        .pushdown_enabled
        .then(|| pushdown::plan(request, &all_columns))
        .transpose()?;
    let scan_columns = if let Some(query) = &query {
        query
            .required
            .iter()
            .map(|name| {
                all_columns
                    .iter()
                    .find(|column| column.exasol_name == *name)
                    .cloned()
                    .expect("pushdown planner validated required columns")
            })
            .collect()
    } else {
        all_columns.clone()
    };
    let spec = MongoScanSpec {
        version: SCAN_SPEC_VERSION,
        connection_name: notes.connection_name,
        database: notes.database,
        collection: notes.collection,
        plan: MongoReadPlan::for_table(&table),
        columns: scan_columns,
        batch_size: notes.batch_size,
        pushdown: query
            .as_ref()
            .map(|query| query.mongo.clone())
            .unwrap_or_else(MongoPushdown::default),
        inference_fingerprint: notes.inference_fingerprint,
    };
    let serialized_spec = spec.to_json()?;
    debug_assert!(serialized_spec.len() <= MAX_SCAN_SPEC_BYTES);
    let spec_literal = quote_sql_string(&serialized_spec);
    let emits = if query
        .as_ref()
        .is_some_and(|query| query.mongo.aggregation.is_some())
    {
        format!(
            "{} DECIMAL(18,0)",
            quote_ident(pushdown::AGGREGATE_COUNT_FIELD)
        )
    } else {
        spec.columns
            .iter()
            .map(|column| {
                format!(
                    "{} {}",
                    quote_ident(&column.exasol_name),
                    column.sql_type.exasol_type()
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let udf = format!(
        "{}.{}",
        quote_ident(&ctx.script_schema()),
        quote_ident("MONGODB_SCAN")
    );
    let udf_select = format!("SELECT {udf}({spec_literal}) EMITS ({emits})");
    let sql = if let Some(query) = &query {
        pushdown::render_outer_sql(&udf_select, query, &all_columns)?
    } else {
        udf_select
    };
    if sql.len() > MAX_GENERATED_SQL_BYTES {
        return Err(UdfError::User(format!(
            "generated pushdown SQL is {} bytes; maximum is {MAX_GENERATED_SQL_BYTES}",
            sql.len()
        )));
    }
    Ok(json!({"type": "pushdown", "sql": sql}))
}

fn parse_config(props: &Json) -> Result<AdapterConfig, UdfError> {
    let required = |name: &str| {
        props
            .get(name)
            .and_then(Json::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| UdfError::User(format!("property '{name}' is required")))
    };
    let manifest = props
        .get(PROP_MANIFEST)
        .and_then(Json::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ExplicitManifest::parse)
        .transpose()?;
    let batch_size = props
        .get(PROP_BATCH_SIZE)
        .and_then(Json::as_str)
        .unwrap_or("128")
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| UdfError::User("property 'BATCH_SIZE' must be a positive integer".into()))?;
    let defaults = InferenceConfig::default();
    let inference = InferenceConfig {
        sample_size: numeric_property(props, PROP_SAMPLE_SIZE, defaults.sample_size)?,
        max_sample_bytes: numeric_property(props, PROP_MAX_BYTES, defaults.max_sample_bytes)?,
        max_depth: numeric_property(props, PROP_MAX_DEPTH, defaults.max_depth)?,
        max_array_elements: numeric_property(
            props,
            PROP_ARRAY_ELEMENTS,
            defaults.max_array_elements,
        )?,
        max_time_ms: numeric_property(props, PROP_MAX_TIME_MS, defaults.max_time_ms)?,
    };
    inference.validate()?;
    let pushdown_enabled = property_bool(props, PROP_ENABLE_PUSHDOWN, true)?;
    Ok(AdapterConfig {
        connection_name: required(PROP_CONNECTION)?,
        database: required(PROP_DATABASE)?,
        collection: required(PROP_COLLECTION)?,
        manifest,
        batch_size,
        inference,
        pushdown_enabled,
    })
}

fn property_bool(props: &Json, name: &str, default: bool) -> Result<bool, UdfError> {
    match props.get(name).and_then(Json::as_str) {
        None => Ok(default),
        Some(value) if value.eq_ignore_ascii_case("true") => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("false") => Ok(false),
        Some(_) => Err(UdfError::User(format!(
            "property '{name}' must be 'true' or 'false'"
        ))),
    }
}

const fn default_true() -> bool {
    true
}

fn numeric_property<T>(props: &Json, name: &str, default: T) -> Result<T, UdfError>
where
    T: std::str::FromStr + Copy,
{
    match props.get(name) {
        None => Ok(default),
        Some(Json::String(value)) => value.parse().map_err(|_| {
            UdfError::User(format!("property '{name}' must be a non-negative integer"))
        }),
        Some(_) => Err(UdfError::User(format!(
            "property '{name}' must be a string containing a non-negative integer"
        ))),
    }
}

fn notes_from_request(request: &Json) -> Result<AdapterNotes, UdfError> {
    let value = request
        .get("schemaMetadataInfo")
        .and_then(|metadata| metadata.get("adapterNotes"))
        .and_then(Json::as_str)
        .ok_or_else(|| UdfError::User("pushdown request is missing adapterNotes".into()))?;
    if value.len() > MAX_ADAPTER_NOTES_BYTES {
        return Err(UdfError::User(
            "pushdown adapterNotes exceed the supported size".into(),
        ));
    }
    let notes: AdapterNotes = serde_json::from_str(value).map_err(|_| {
        UdfError::User("pushdown adapterNotes are invalid; refresh the Virtual Schema".into())
    })?;
    if notes.version != NOTES_VERSION {
        return Err(UdfError::User(format!(
            "unsupported adapterNotes version {}; refresh the Virtual Schema",
            notes.version
        )));
    }
    notes.manifest.validate()?;
    Ok(notes)
}

fn effective_properties(request: &Json) -> Json {
    let mut values = Map::new();
    if let Some(persisted) = request
        .get("schemaMetadataInfo")
        .and_then(|metadata| metadata.get("properties"))
        .and_then(Json::as_object)
    {
        values.extend(persisted.clone());
    }
    if let Some(current) = request.get("properties").and_then(Json::as_object) {
        for (key, value) in current {
            values.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    Json::Object(values)
}

fn merge_set_properties(request: &Json) -> Json {
    let mut values = request
        .get("schemaMetadataInfo")
        .and_then(|metadata| metadata.get("properties"))
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(current) = request.get("properties").and_then(Json::as_object) {
        for (key, value) in current {
            if value.is_null() {
                values.remove(key);
            } else {
                values.insert(key.clone(), value.clone());
            }
        }
    }
    Json::Object(values)
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn quote_sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use exasol_udf_sdk::connect_back::ConnectionObject;
    use exasol_udf_sdk::value::Value;

    use super::*;

    struct Context {
        script_schema: String,
        connections: HashMap<String, ConnectionObject>,
    }

    impl UdfContext for Context {
        fn num_columns(&self) -> usize {
            0
        }
        fn get(&self, _col: usize) -> Result<&Value, UdfError> {
            unreachable!()
        }
        fn emit(&mut self, _values: &[Value]) -> Result<(), UdfError> {
            unreachable!()
        }
        fn next(&mut self) -> Result<bool, UdfError> {
            unreachable!()
        }
        fn script_schema(&self) -> String {
            self.script_schema.clone()
        }
        fn connection(&self, name: &str) -> Result<ConnectionObject, UdfError> {
            self.connections
                .get(name)
                .cloned()
                .ok_or_else(|| UdfError::User("missing".into()))
        }
    }

    fn context() -> Context {
        Context {
            script_schema: "MONGO_VS".into(),
            connections: HashMap::from([(
                "MONGO_CONN".into(),
                ConnectionObject {
                    kind: "".into(),
                    address: "mongodb://mongo:27017".into(),
                    user: "secret-user".into(),
                    password: "secret-password".into(),
                },
            )]),
        }
    }

    fn manifest() -> Json {
        json!({
            "format": "exasol-json-tables-source-manifest",
            "version": 1,
            "stem": "people",
            "tables": [
                {
                    "tableName": "PEOPLE",
                    "path": "root",
                    "pathSegments": [],
                    "kind": "object",
                    "columns": [
                        {"name":"_id","typeName":"VARCHAR(64)","ordinal":1},
                        {"name":"mongo_id","sourceName":"_id","bsonType":"OBJECT_ID","typeName":"VARCHAR(24)","ordinal":2},
                        {"name":"name","typeName":"VARCHAR(2000000)","ordinal":3},
                        {"name":"tags|array","typeName":"DECIMAL(18,0)","ordinal":4}
                    ]
                },
                {
                    "tableName": "PEOPLE_tags_arr",
                    "path": "tags[]",
                    "pathSegments": [{"name":"tags","kind":"array"}],
                    "kind": "array",
                    "columns": [
                        {"name":"_parent","typeName":"VARCHAR(64)","ordinal":1},
                        {"name":"_pos","typeName":"DECIMAL(18,0)","ordinal":2},
                        {"name":"_value","typeName":"VARCHAR(2000000)","ordinal":3}
                    ]
                }
            ]
        })
    }

    fn create_request() -> Json {
        json!({
            "type": "createVirtualSchema",
            "properties": {
                "MONGODB_CONNECTION": "MONGO_CONN",
                "DATABASE": "demo",
                "COLLECTION": "people",
                "MANIFEST": manifest().to_string(),
            }
        })
    }

    #[test]
    fn create_advertises_manifest_family_and_secret_free_versioned_notes() {
        let response = dispatch(&mut context(), &create_request()).unwrap();
        let tables = response["schemaMetadata"]["tables"].as_array().unwrap();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0]["name"], "PEOPLE");
        let notes = response["schemaMetadata"]["adapterNotes"].as_str().unwrap();
        assert!(notes.contains("MONGO_CONN"));
        assert!(notes.contains("\"version\":1"));
        assert!(!notes.contains("secret-user"));
        assert!(!notes.contains("secret-password"));
        assert!(!notes.contains("mongodb://"));
    }

    #[test]
    fn create_does_not_resolve_connection_for_explicit_metadata() {
        let mut ctx = context();
        ctx.connections.clear();
        assert!(dispatch(&mut ctx, &create_request()).is_ok());
    }

    #[test]
    fn pushdown_selects_the_involved_table_and_quotes_hostile_names() {
        let created = dispatch(&mut context(), &create_request()).unwrap();
        let request = json!({
            "type": "pushdown",
            "schemaMetadataInfo": {"adapterNotes": created["schemaMetadata"]["adapterNotes"]},
            "involvedTables": [{"name": "PEOPLE_tags_arr", "columns": []}],
            "pushdownRequest": {
                "type": "select",
                "selectList": [
                    {"type":"column","name":"_parent"},
                    {"type":"column","name":"_pos"},
                    {"type":"column","name":"_value"}
                ]
            },
        });
        let response = dispatch(&mut context(), &request).unwrap();
        let sql = response["sql"].as_str().unwrap();
        assert!(sql.starts_with(
            "SELECT \"_parent\", \"_pos\", \"_value\" FROM (SELECT \"MONGO_VS\".\"MONGODB_SCAN\"("
        ));
        assert!(sql.contains("EMITS (\"_parent\" VARCHAR(64), \"_pos\" DECIMAL(18,0)"));
        assert!(!sql.contains("secret-user"));
        assert!(!sql.contains("secret-password"));
        assert!(!sql.contains("mongodb://"));
    }

    #[test]
    fn pushdown_emits_one_remote_count_and_keeps_count_column_local() {
        let created = dispatch(&mut context(), &create_request()).unwrap();
        let aggregate_request = |arguments: Json| {
            json!({
                "type": "pushdown",
                "schemaMetadataInfo": {"adapterNotes": created["schemaMetadata"]["adapterNotes"]},
                "involvedTables": [{"name": "PEOPLE", "columns": []}],
                "pushdownRequest": {
                    "type": "select",
                    "aggregationType": "single_group",
                    "selectList": [{
                        "type":"function_aggregate",
                        "name":"count",
                        "arguments": arguments,
                        "distinct":false
                    }],
                "selectListDataTypes": [{"type":"decimal", "precision":18, "scale":0}]
                }
            })
        };

        let remote = dispatch(&mut context(), &aggregate_request(json!([]))).unwrap();
        let remote_sql = remote["sql"].as_str().unwrap();
        assert!(remote_sql.starts_with("SELECT \"__jt_count\""));
        assert!(remote_sql.contains("EMITS (\"__jt_count\" DECIMAL(18,0))"));
        assert!(remote_sql.contains("\"aggregation\":{\"kind\":\"count_star\"}"));

        let local = dispatch(
            &mut context(),
            &aggregate_request(json!([{"type":"column", "name":"name"}])),
        )
        .unwrap();
        let local_sql = local["sql"].as_str().unwrap();
        assert!(local_sql.starts_with("SELECT COUNT(\"name\")"));
        assert!(local_sql.contains("EMITS (\"name\" VARCHAR(2000000))"));
        assert!(!local_sql.contains("\"aggregation\""));
    }

    #[test]
    fn adapter_protocol_handles_lifecycle_and_rejects_bad_requests() {
        let mut ctx = context();
        let capabilities = dispatch(&mut ctx, &json!({"type":"getCapabilities"})).unwrap();
        assert_eq!(capabilities["capabilities"], json!(CAPABILITIES));
        assert_eq!(
            dispatch(
                &mut ctx,
                &json!({"type":"getCapabilities", "properties":{"ENABLE_PUSHDOWN":"false"}})
            )
            .unwrap()["capabilities"],
            json!([])
        );
        assert_eq!(
            dispatch(&mut ctx, &json!({"type":"dropVirtualSchema"})).unwrap()["type"],
            "dropVirtualSchema"
        );
        assert!(
            dispatch(&mut ctx, &json!({}))
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        assert!(
            adapter_call(&mut ctx, "not-json")
                .unwrap_err()
                .to_string()
                .contains("invalid JSON")
        );
        assert!(
            adapter_call(&mut ctx, &json!({"type":"getCapabilities"}).to_string())
                .unwrap()
                .contains("getCapabilities")
        );
    }

    #[test]
    fn property_updates_merge_replace_and_remove_values() {
        let persisted = create_request()["properties"].clone();
        let created = dispatch(
            &mut context(),
            &json!({
                "type": "refresh",
                "schemaMetadataInfo": {"properties": persisted},
                "properties": {},
                "requestedTables": ["PEOPLE"]
            }),
        )
        .unwrap();
        assert_eq!(created["requestedTables"], json!(["PEOPLE"]));

        let updated = dispatch(
            &mut context(),
            &json!({
                "type": "setProperties",
                "schemaMetadataInfo": {"properties": create_request()["properties"].clone()},
                "properties": {"BATCH_SIZE": "7"}
            }),
        )
        .unwrap();
        let notes: AdapterNotes =
            serde_json::from_str(updated["schemaMetadata"]["adapterNotes"].as_str().unwrap())
                .unwrap();
        assert_eq!(notes.batch_size, 7);

        let removed = merge_set_properties(&json!({
            "schemaMetadataInfo": {"properties": {"A":"1", "B":"2"}},
            "properties": {"A": null, "B":"3"}
        }));
        assert_eq!(removed, json!({"B":"3"}));
    }

    #[test]
    fn configuration_and_pushdown_errors_are_actionable() {
        let mut request = create_request();
        request["properties"]["BATCH_SIZE"] = json!("0");
        assert!(
            dispatch(&mut context(), &request)
                .unwrap_err()
                .to_string()
                .contains("positive integer")
        );
        request["properties"]["BATCH_SIZE"] = json!("128");
        request["properties"]["DATABASE"] = json!("");
        assert!(
            dispatch(&mut context(), &request)
                .unwrap_err()
                .to_string()
                .contains("DATABASE")
        );

        let created = dispatch(&mut context(), &create_request()).unwrap();
        let notes = created["schemaMetadata"]["adapterNotes"].clone();
        for request in [
            json!({"type":"pushdown", "schemaMetadataInfo":{"adapterNotes":notes.clone()}, "involvedTables":[]}),
            json!({"type":"pushdown", "schemaMetadataInfo":{"adapterNotes":notes.clone()}, "involvedTables":[{"name":"UNKNOWN"}]}),
            json!({"type":"pushdown", "schemaMetadataInfo":{"adapterNotes":"not-json"}, "involvedTables":[{"name":"PEOPLE"}]}),
        ] {
            assert!(dispatch(&mut context(), &request).is_err());
        }
        let mut stale: Json = serde_json::from_str(notes.as_str().unwrap()).unwrap();
        stale["version"] = json!(99);
        let error = dispatch(
            &mut context(),
            &json!({
                "type":"pushdown",
                "schemaMetadataInfo":{"adapterNotes":stale.to_string()},
                "involvedTables":[{"name":"PEOPLE"}]
            }),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("refresh"));
    }

    #[test]
    fn manifest_is_optional_and_inference_budgets_are_validated() {
        let mut properties = create_request()["properties"].clone();
        properties.as_object_mut().unwrap().remove(PROP_MANIFEST);
        properties[PROP_SAMPLE_SIZE] = json!("0");
        properties[PROP_MAX_DEPTH] = json!("4");
        let config = parse_config(&properties).unwrap();
        assert!(config.manifest.is_none());
        assert_eq!(config.inference.sample_size, 0);
        assert_eq!(config.inference.max_depth, 4);

        properties[PROP_MAX_DEPTH] = json!("0");
        assert!(parse_config(&properties).is_err());
        properties[PROP_MAX_DEPTH] = json!("4");
        properties[PROP_SAMPLE_SIZE] = json!("many");
        assert!(parse_config(&properties).is_err());
    }
}
