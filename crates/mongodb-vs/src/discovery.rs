use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use exasol_udf_sdk::connect_back::ConnectionObject;
use exasol_udf_sdk::error::UdfError;
use futures_util::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::error::ErrorKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::connection;
use crate::model::{
    BsonKind, ExplicitManifest, MANIFEST_FORMAT, MANIFEST_VERSION, ManifestColumn, PathKind,
    PathSegment, RelationshipSpec, RootSpec, TableSpec,
};

const MAX_SAMPLE_DOCUMENTS: u32 = 10_000;
const MAX_SAMPLE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ARRAY_ELEMENTS: usize = 1_000;
const MAX_INFERENCE_TIME_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceConfig {
    pub sample_size: u32,
    pub max_sample_bytes: usize,
    pub max_depth: usize,
    pub max_array_elements: usize,
    pub max_time_ms: u64,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            sample_size: 100,
            max_sample_bytes: 8 * 1024 * 1024,
            max_depth: 8,
            max_array_elements: 32,
            max_time_ms: 5_000,
        }
    }
}

impl InferenceConfig {
    pub fn validate(&self) -> Result<(), UdfError> {
        if self.sample_size > MAX_SAMPLE_DOCUMENTS {
            return Err(user(format!(
                "INFERENCE_SAMPLE_SIZE exceeds the maximum of {MAX_SAMPLE_DOCUMENTS}"
            )));
        }
        if self.max_sample_bytes == 0 || self.max_sample_bytes > MAX_SAMPLE_BYTES {
            return Err(user(format!(
                "INFERENCE_MAX_BYTES must be in 1..={MAX_SAMPLE_BYTES}"
            )));
        }
        if self.max_depth == 0 || self.max_depth > 32 {
            return Err(user("INFERENCE_MAX_DEPTH must be in 1..=32"));
        }
        if self.max_array_elements == 0 || self.max_array_elements > MAX_ARRAY_ELEMENTS {
            return Err(user(format!(
                "INFERENCE_ARRAY_ELEMENTS must be in 1..={MAX_ARRAY_ELEMENTS}"
            )));
        }
        if self.max_time_ms == 0 || self.max_time_ms > MAX_INFERENCE_TIME_MS {
            return Err(user(format!(
                "INFERENCE_MAX_TIME_MS must be in 1..={MAX_INFERENCE_TIME_MS}"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Available,
    NotAuthorized,
    NotRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEvidence {
    pub name: String,
    pub keys: Vec<IndexKeyEvidence>,
    pub unique: bool,
    pub sparse: bool,
    pub partial: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexKeyEvidence {
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathEvidenceReport {
    pub path: Vec<String>,
    pub declared: Vec<String>,
    pub observed: Vec<String>,
    pub indexed_by: Vec<String>,
    pub required: bool,
    pub present_count: u64,
    pub null_count: u64,
    pub additional_properties: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceReport {
    pub complete: bool,
    pub sampled_documents: u32,
    pub sampled_bytes: usize,
    pub metadata_status: EvidenceStatus,
    pub index_status: EvidenceStatus,
    pub sample_status: EvidenceStatus,
    pub paths: Vec<PathEvidenceReport>,
    pub indexes: Vec<IndexEvidence>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceResult {
    pub manifest: ExplicitManifest,
    pub fingerprint: String,
    pub report: InferenceReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ValueKind {
    Null,
    String,
    ObjectId,
    Int32,
    Int64,
    Integer,
    Double,
    NonFiniteDouble,
    Boolean,
    Decimal128,
    DateTime,
    Timestamp,
    ExtendedJson,
    Object,
    Array,
}

impl ValueKind {
    fn label(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::String => "string",
            Self::ObjectId => "object_id",
            Self::Int32 => "int32",
            Self::Int64 => "int64",
            Self::Integer => "integer",
            Self::Double => "double",
            Self::NonFiniteDouble => "non_finite_double",
            Self::Boolean => "boolean",
            Self::Decimal128 => "decimal128",
            Self::DateTime => "date_time",
            Self::Timestamp => "timestamp",
            Self::ExtendedJson => "extended_json",
            Self::Object => "object",
            Self::Array => "array",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct NodeEvidence {
    declared: BTreeSet<ValueKind>,
    observed: BTreeMap<ValueKind, u64>,
    indexed_by: BTreeSet<String>,
    required: bool,
    present_count: u64,
    null_count: u64,
    additional_properties: Option<bool>,
    fields: BTreeMap<String, NodeEvidence>,
    items: Option<Box<NodeEvidence>>,
}

#[derive(Debug, Default)]
struct Evidence {
    root: NodeEvidence,
    collection_metadata: Option<Document>,
    raw_indexes: Vec<Document>,
    indexes: Vec<IndexEvidence>,
    metadata_status: Option<EvidenceStatus>,
    index_status: Option<EvidenceStatus>,
    sample_status: Option<EvidenceStatus>,
    sampled_documents: u32,
    sampled_bytes: usize,
    complete: bool,
    warnings: BTreeSet<String>,
}

pub async fn infer(
    resolved: &ConnectionObject,
    database: &str,
    collection: &str,
    config: &InferenceConfig,
) -> Result<InferenceResult, UdfError> {
    config.validate()?;
    let client = connection::client(resolved).await?;
    let database_handle = client.database(database);
    let collection_handle = database_handle.collection::<Document>(collection);
    let mut evidence = Evidence {
        complete: false,
        ..Evidence::default()
    };

    match database_handle
        .run_command(doc! {
            "listCollections": 1,
            "filter": {"name": collection},
            "nameOnly": false,
            "maxTimeMS": config.max_time_ms as i64,
        })
        .await
    {
        Ok(reply) => {
            let metadata = first_batch_document(&reply).ok_or_else(|| {
                user(format!(
                    "MongoDB collection '{database}.{collection}' does not exist"
                ))
            })?;
            let namespace_type = metadata.get_str("type").unwrap_or("collection");
            if namespace_type != "collection" {
                return Err(user(format!(
                    "MongoDB namespace '{database}.{collection}' is a {namespace_type}; only regular collections are supported"
                )));
            }
            if let Ok(options) = metadata.get_document("options")
                && let Ok(validator) = options.get_document("validator")
            {
                extract_validator(
                    validator,
                    &mut evidence.root,
                    config.max_depth,
                    &mut evidence.warnings,
                );
            }
            evidence.collection_metadata = Some(metadata);
            evidence.metadata_status = Some(EvidenceStatus::Available);
        }
        Err(error) if is_not_authorized(&error) => {
            evidence.metadata_status = Some(EvidenceStatus::NotAuthorized);
            evidence.warnings.insert(
                "collection metadata is not authorized; validator and UUID evidence are unavailable"
                    .into(),
            );
        }
        Err(_) => return Err(mongo_error("reading collection metadata")),
    }

    match collection_handle
        .list_indexes()
        .max_time(Duration::from_millis(config.max_time_ms))
        .await
    {
        Ok(cursor) => {
            let models = cursor
                .try_collect::<Vec<_>>()
                .await
                .map_err(|_| mongo_error("reading collection indexes"))?;
            for model in models {
                let raw = mongodb::bson::to_document(&model)
                    .map_err(|_| user("failed to normalize MongoDB index metadata"))?;
                extract_index(&raw, &mut evidence);
                evidence.raw_indexes.push(raw);
            }
            evidence.index_status = Some(EvidenceStatus::Available);
        }
        Err(error) if is_not_authorized(&error) => {
            evidence.index_status = Some(EvidenceStatus::NotAuthorized);
            evidence.warnings.insert(
                "collection indexes are not authorized; index evidence is unavailable".into(),
            );
        }
        Err(_) => return Err(mongo_error("reading collection indexes")),
    }

    if config.sample_size == 0 {
        evidence.sample_status = Some(EvidenceStatus::NotRequested);
    } else {
        let pipeline = vec![doc! {"$sample": {"size": i64::from(config.sample_size)}}];
        match collection_handle
            .aggregate(pipeline)
            .max_time(Duration::from_millis(config.max_time_ms))
            .batch_size(config.sample_size)
            .await
        {
            Ok(mut cursor) => {
                while let Some(document) = cursor
                    .try_next()
                    .await
                    .map_err(|_| mongo_error("reading inference samples"))?
                {
                    let size = mongodb::bson::to_vec(&document)
                        .map_err(|_| user("failed to measure a MongoDB inference sample"))?
                        .len();
                    if evidence.sampled_bytes.saturating_add(size) > config.max_sample_bytes {
                        evidence.warnings.insert(format!(
                            "sample byte budget reached at {} documents; inference is incomplete",
                            evidence.sampled_documents
                        ));
                        break;
                    }
                    evidence.sampled_bytes += size;
                    evidence.sampled_documents += 1;
                    observe_document(
                        &document,
                        &mut evidence.root,
                        0,
                        config,
                        &mut evidence.warnings,
                    );
                }
                evidence.sample_status = Some(EvidenceStatus::Available);
                evidence.warnings.insert(format!(
                    "bounded sampling inspected at most {} documents; unobserved fields and branches remain possible",
                    config.sample_size
                ));
            }
            Err(error) if is_not_authorized(&error) => {
                evidence.sample_status = Some(EvidenceStatus::NotAuthorized);
                evidence.warnings.insert(
                    "document sampling is not authorized; observed evidence is unavailable".into(),
                );
            }
            Err(_) => return Err(mongo_error("opening inference sample cursor")),
        }
    }

    let manifest = resolve_manifest(collection, &evidence.root, &mut evidence.warnings)?;
    let fingerprint = fingerprint(&evidence, config, &manifest)?;
    let report = report(&evidence);
    Ok(InferenceResult {
        manifest,
        fingerprint,
        report,
    })
}

fn first_batch_document(reply: &Document) -> Option<Document> {
    reply
        .get_document("cursor")
        .ok()?
        .get_array("firstBatch")
        .ok()?
        .first()?
        .as_document()
        .cloned()
}

fn is_not_authorized(error: &mongodb::error::Error) -> bool {
    matches!(&*error.kind, ErrorKind::Command(command) if command.code == 13)
}

fn extract_validator(
    validator: &Document,
    root: &mut NodeEvidence,
    max_depth: usize,
    warnings: &mut BTreeSet<String>,
) {
    if let Ok(schema) = validator.get_document("$jsonSchema") {
        extract_schema(schema, root, true, 0, max_depth, warnings);
    }
    extract_query_predicates(validator, root, warnings);
}

fn extract_schema(
    schema: &Document,
    node: &mut NodeEvidence,
    requirements_unconditional: bool,
    depth: usize,
    max_depth: usize,
    warnings: &mut BTreeSet<String>,
) {
    if let Some(value) = schema.get("bsonType").or_else(|| schema.get("type")) {
        extract_declared_types(value, &mut node.declared, warnings);
    }
    let required = if requirements_unconditional {
        schema
            .get_array("required")
            .ok()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Bson::as_str)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default()
    } else {
        BTreeSet::new()
    };
    if let Some(Bson::Boolean(value)) = schema.get("additionalProperties") {
        node.additional_properties = Some(*value);
    }
    if let Ok(values) = schema.get_array("enum") {
        for value in values {
            node.declared.insert(value_kind(value));
        }
    }
    if depth >= max_depth {
        if schema.contains_key("properties") || schema.contains_key("items") {
            warnings.insert(format!(
                "validator inference depth budget {max_depth} reached; nested declarations are incomplete"
            ));
        }
        return;
    }
    if let Ok(properties) = schema.get_document("properties") {
        for (name, value) in properties {
            let child = node.fields.entry(name.clone()).or_default();
            child.required |= required.contains(name.as_str());
            if let Some(property_schema) = value.as_document() {
                extract_schema(
                    property_schema,
                    child,
                    requirements_unconditional,
                    depth + 1,
                    max_depth,
                    warnings,
                );
            }
        }
    }
    if let Ok(items) = schema.get_document("items") {
        let item = node.items.get_or_insert_with(Default::default);
        extract_schema(
            items,
            item,
            requirements_unconditional,
            depth + 1,
            max_depth,
            warnings,
        );
    } else if schema.get_array("items").is_ok() {
        warnings.insert("tuple-style validator items are preserved but not inferred".into());
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Ok(branches) = schema.get_array(keyword) {
            let unconditional = requirements_unconditional && keyword == "allOf";
            for branch in branches {
                if let Some(branch) = branch.as_document() {
                    extract_schema(branch, node, unconditional, depth, max_depth, warnings);
                }
            }
        }
    }
    for unsupported in ["not", "dependencies", "patternProperties"] {
        if schema.contains_key(unsupported) {
            warnings.insert(format!(
                "validator keyword '{unsupported}' is preserved for fingerprinting but not inferred"
            ));
        }
    }
}

fn extract_declared_types(
    value: &Bson,
    output: &mut BTreeSet<ValueKind>,
    warnings: &mut BTreeSet<String>,
) {
    let values: Vec<&str> = match value {
        Bson::String(value) => vec![value],
        Bson::Array(values) => values.iter().filter_map(Bson::as_str).collect(),
        _ => {
            warnings.insert("validator type declaration has an unsupported shape".into());
            return;
        }
    };
    for value in values {
        if matches!(value, "number" | "integer") {
            output.insert(ValueKind::Int32);
            output.insert(ValueKind::Int64);
            if value == "number" {
                output.insert(ValueKind::Double);
                output.insert(ValueKind::Decimal128);
            }
        } else if let Some(kind) = validator_kind(value) {
            output.insert(kind);
        } else {
            warnings.insert(format!(
                "validator type '{value}' is preserved but not exposed"
            ));
        }
    }
}

fn validator_kind(value: &str) -> Option<ValueKind> {
    match value {
        "null" => Some(ValueKind::Null),
        "string" => Some(ValueKind::String),
        "objectId" => Some(ValueKind::ObjectId),
        "int" => Some(ValueKind::Int32),
        "long" => Some(ValueKind::Int64),
        "double" => Some(ValueKind::Double),
        "bool" | "boolean" => Some(ValueKind::Boolean),
        "decimal" => Some(ValueKind::Decimal128),
        "date" => Some(ValueKind::DateTime),
        "timestamp" => Some(ValueKind::Timestamp),
        "object" => Some(ValueKind::Object),
        "array" => Some(ValueKind::Array),
        "binData"
        | "regex"
        | "javascript"
        | "javascriptWithScope"
        | "dbPointer"
        | "symbol"
        | "undefined"
        | "minKey"
        | "maxKey" => Some(ValueKind::ExtendedJson),
        _ => None,
    }
}

fn extract_query_predicates(
    predicate: &Document,
    root: &mut NodeEvidence,
    warnings: &mut BTreeSet<String>,
) {
    if let Ok(conjunctions) = predicate.get_array("$and") {
        for conjunction in conjunctions {
            if let Some(document) = conjunction.as_document() {
                extract_query_predicates(document, root, warnings);
            }
        }
    }
    for (path, value) in predicate {
        if path.starts_with('$') {
            if path != "$jsonSchema" && path != "$and" {
                warnings.insert(format!(
                    "validator predicate '{path}' is preserved for fingerprinting but not inferred"
                ));
            }
            continue;
        }
        let node = indexed_path_mut(root, path);
        if let Some(document) = value.as_document() {
            if matches!(document.get("$exists"), Some(Bson::Boolean(true))) {
                node.required = true;
            }
            if let Some(kind) = document.get("$type") {
                extract_declared_types(kind, &mut node.declared, warnings);
            }
            if let Some(equality) = document.get("$eq") {
                node.declared.insert(value_kind(equality));
            }
            if let Ok(values) = document.get_array("$in") {
                for value in values {
                    node.declared.insert(value_kind(value));
                }
            }
        } else {
            node.required = true;
            node.declared.insert(value_kind(value));
        }
    }
}

fn extract_index(index: &Document, evidence: &mut Evidence) {
    let name = index.get_str("name").unwrap_or("(unnamed)").to_owned();
    let mut keys = Vec::new();
    if let Ok(key_document) = index.get_document("key") {
        for (path, kind) in key_document {
            let kind = match kind {
                Bson::Int32(value) => value.to_string(),
                Bson::Int64(value) => value.to_string(),
                Bson::Double(value) => value.to_string(),
                Bson::String(value) => value.clone(),
                _ => "other".into(),
            };
            indexed_path_mut(&mut evidence.root, path)
                .indexed_by
                .insert(name.clone());
            keys.push(IndexKeyEvidence {
                path: path.clone(),
                kind,
            });
        }
    }
    evidence.indexes.push(IndexEvidence {
        name,
        keys,
        unique: index.get_bool("unique").unwrap_or(false),
        sparse: index.get_bool("sparse").unwrap_or(false),
        partial: index.contains_key("partialFilterExpression"),
        hidden: index.get_bool("hidden").unwrap_or(false),
    });
}

fn indexed_path_mut<'a>(root: &'a mut NodeEvidence, path: &str) -> &'a mut NodeEvidence {
    let mut node = root;
    for segment in path.split('.') {
        node = node.fields.entry(segment.to_owned()).or_default();
    }
    node
}

fn observe_document(
    document: &Document,
    node: &mut NodeEvidence,
    depth: usize,
    config: &InferenceConfig,
    warnings: &mut BTreeSet<String>,
) {
    for (name, value) in document {
        let child = node.fields.entry(name.clone()).or_default();
        child.present_count += 1;
        observe_value(value, child, depth + 1, config, warnings);
    }
}

fn observe_value(
    value: &Bson,
    node: &mut NodeEvidence,
    depth: usize,
    config: &InferenceConfig,
    warnings: &mut BTreeSet<String>,
) {
    let kind = value_kind(value);
    *node.observed.entry(kind).or_default() += 1;
    if kind == ValueKind::Null {
        node.null_count += 1;
    }
    if depth >= config.max_depth && matches!(value, Bson::Document(_) | Bson::Array(_)) {
        warnings.insert(format!(
            "inference depth budget {} reached; nested structure is incomplete",
            config.max_depth
        ));
        return;
    }
    match value {
        Bson::Document(document) => observe_document(document, node, depth, config, warnings),
        Bson::Array(values) => {
            let item = node.items.get_or_insert_with(Default::default);
            let indices = distributed_indices(values.len(), config.max_array_elements);
            if indices.len() < values.len() {
                warnings.insert(format!(
                    "array element budget {} reached; array branch inference is incomplete",
                    config.max_array_elements
                ));
            }
            for index in indices {
                item.present_count += 1;
                observe_value(&values[index], item, depth + 1, config, warnings);
            }
        }
        _ => {}
    }
}

fn distributed_indices(length: usize, limit: usize) -> Vec<usize> {
    if length <= limit {
        return (0..length).collect();
    }
    if limit == 1 {
        return vec![0];
    }
    (0..limit)
        .map(|index| index * (length - 1) / (limit - 1))
        .collect()
}

fn value_kind(value: &Bson) -> ValueKind {
    match value {
        Bson::Null => ValueKind::Null,
        Bson::String(_) => ValueKind::String,
        Bson::ObjectId(_) => ValueKind::ObjectId,
        Bson::Int32(_) => ValueKind::Int32,
        Bson::Int64(_) => ValueKind::Int64,
        Bson::Double(value) if value.is_finite() => ValueKind::Double,
        Bson::Double(_) => ValueKind::NonFiniteDouble,
        Bson::Boolean(_) => ValueKind::Boolean,
        Bson::Decimal128(_) => ValueKind::Decimal128,
        Bson::DateTime(_) => ValueKind::DateTime,
        Bson::Timestamp(_) => ValueKind::Timestamp,
        Bson::Document(_) => ValueKind::Object,
        Bson::Array(_) => ValueKind::Array,
        _ => ValueKind::ExtendedJson,
    }
}

fn resolve_manifest(
    collection: &str,
    root: &NodeEvidence,
    warnings: &mut BTreeSet<String>,
) -> Result<ExplicitManifest, UdfError> {
    let stem = identifier(collection, true);
    let mut tables = Vec::new();
    let mut relationships = Vec::new();
    build_table(
        &stem,
        &stem,
        PathKind::Object,
        &[],
        root,
        &mut tables,
        &mut relationships,
        warnings,
    );
    tables.sort_by(|left, right| left.table_name.cmp(&right.table_name));
    relationships.sort_by(|left, right| {
        (&left.parent_table, &left.child_table, &left.segment_name).cmp(&(
            &right.parent_table,
            &right.child_table,
            &right.segment_name,
        ))
    });
    let family_tables = tables
        .iter()
        .map(|table| table.table_name.clone())
        .collect();
    let manifest = ExplicitManifest {
        format: MANIFEST_FORMAT.into(),
        version: MANIFEST_VERSION,
        stem: stem.clone(),
        roots: vec![RootSpec {
            table_name: stem.clone(),
            family_tables,
        }],
        relationships,
        tables,
    };
    manifest.validate()?;
    Ok(manifest)
}

#[allow(clippy::too_many_arguments)]
fn build_table(
    stem: &str,
    table_name: &str,
    table_kind: PathKind,
    path: &[PathSegment],
    node: &NodeEvidence,
    tables: &mut Vec<TableSpec>,
    relationships: &mut Vec<RelationshipSpec>,
    warnings: &mut BTreeSet<String>,
) {
    let mut columns = Vec::new();
    if table_kind == PathKind::Object {
        columns.push(manifest_column("_id", "VARCHAR(64)", None, None, true));
    } else {
        if node_has_kind(node, ValueKind::Object) || node_has_kind(node, ValueKind::Array) {
            columns.push(manifest_column("_id", "VARCHAR(64)", None, None, true));
        }
        columns.push(manifest_column("_parent", "VARCHAR(64)", None, None, true));
        columns.push(manifest_column("_pos", "DECIMAL(18,0)", None, None, true));
    }

    if table_kind == PathKind::Array {
        if node_has_kind(node, ValueKind::Object) && all_kinds(node).len() > 1 {
            columns.push(manifest_column(
                "_value|object",
                "BOOLEAN",
                None,
                None,
                false,
            ));
        }
        if node_has_kind(node, ValueKind::Array) {
            columns.push(manifest_column(
                "_value|array",
                "DECIMAL(18,0)",
                None,
                None,
                node.required,
            ));
        }
        add_scalar_columns("_value", None, node, true, &mut columns, warnings);
    }
    if table_kind == PathKind::Object || node_has_kind(node, ValueKind::Object) {
        let physical_names = physical_field_names(&node.fields);
        for (source_name, child) in &node.fields {
            let base = &physical_names[source_name];
            if node_has_kind(child, ValueKind::Object) {
                columns.push(manifest_column(
                    &format!("{base}|object"),
                    "VARCHAR(64)",
                    Some(source_name),
                    None,
                    child.required,
                ));
            }
            if node_has_kind(child, ValueKind::Array) {
                columns.push(manifest_column(
                    &format!("{base}|array"),
                    "DECIMAL(18,0)",
                    Some(source_name),
                    None,
                    child.required,
                ));
            }
            add_scalar_columns(
                base,
                Some(source_name),
                child,
                false,
                &mut columns,
                warnings,
            );
        }
    }
    for (index, column) in columns.iter_mut().enumerate() {
        column.ordinal = index + 1;
    }

    let has_nested_array = node
        .fields
        .values()
        .any(|child| node_has_kind(child, ValueKind::Array) || node_has_nested_array(child))
        || node.items.as_deref().is_some_and(node_has_nested_array);
    tables.push(TableSpec {
        table_name: table_name.into(),
        path: display_path(path),
        path_segments: path.to_vec(),
        kind: table_kind,
        has_nested_array,
        root_table: stem.into(),
        columns,
    });

    for (name, child) in &node.fields {
        for kind in [PathKind::Object, PathKind::Array] {
            let value_kind = match kind {
                PathKind::Object => ValueKind::Object,
                PathKind::Array => ValueKind::Array,
            };
            if !node_has_kind(child, value_kind) {
                continue;
            }
            let mut child_path = path.to_vec();
            child_path.push(PathSegment {
                name: name.clone(),
                kind,
                direct: false,
            });
            let child_table = table_name_for(stem, &child_path);
            relationships.push(RelationshipSpec {
                parent_table: table_name.into(),
                child_table: child_table.clone(),
                segment_name: name.clone(),
                relation_kind: kind,
            });
            let child_node = if kind == PathKind::Array {
                child.items.as_deref().unwrap_or(child)
            } else {
                child
            };
            build_table(
                stem,
                &child_table,
                kind,
                &child_path,
                child_node,
                tables,
                relationships,
                warnings,
            );
        }
    }
    if table_kind == PathKind::Array
        && node_has_kind(node, ValueKind::Array)
        && let Some(items) = &node.items
    {
        let mut child_path = path.to_vec();
        child_path.push(PathSegment {
            name: String::new(),
            kind: PathKind::Array,
            direct: true,
        });
        let child_table = table_name_for(stem, &child_path);
        relationships.push(RelationshipSpec {
            parent_table: table_name.into(),
            child_table: child_table.clone(),
            segment_name: "_value".into(),
            relation_kind: PathKind::Array,
        });
        build_table(
            stem,
            &child_table,
            PathKind::Array,
            &child_path,
            items,
            tables,
            relationships,
            warnings,
        );
    }
}

fn node_has_nested_array(node: &NodeEvidence) -> bool {
    node_has_kind(node, ValueKind::Array)
        || node.fields.values().any(node_has_nested_array)
        || node.items.as_deref().is_some_and(node_has_nested_array)
}

fn add_scalar_columns(
    base: &str,
    source_name: Option<&str>,
    node: &NodeEvidence,
    value_column: bool,
    columns: &mut Vec<ManifestColumn>,
    warnings: &mut BTreeSet<String>,
) {
    let mut kinds = all_kinds(node)
        .into_iter()
        .filter(|kind| !matches!(kind, ValueKind::Null | ValueKind::Object | ValueKind::Array))
        .collect::<Vec<_>>();
    merge_integer_kinds(&mut kinds);
    kinds.sort_by(|left, right| {
        observed_count(node, *right)
            .cmp(&observed_count(node, *left))
            .then_with(|| left.cmp(right))
    });
    for (index, kind) in kinds.iter().enumerate() {
        if *kind == ValueKind::Timestamp {
            for (suffix, bson_kind) in [
                ("timestamp_time", BsonKind::TimestampTime),
                ("timestamp_increment", BsonKind::TimestampIncrement),
            ] {
                columns.push(manifest_column(
                    &format!("{base}|{suffix}"),
                    "DECIMAL(10,0)",
                    source_name,
                    Some(bson_kind),
                    node.required,
                ));
            }
            continue;
        }
        let name = if index == 0 {
            base.to_owned()
        } else {
            format!("{base}|{}", kind.label())
        };
        let (type_name, bson_kind) = scalar_mapping(*kind);
        columns.push(manifest_column(
            &name,
            type_name,
            source_name,
            Some(bson_kind),
            node.required,
        ));
        if *kind == ValueKind::String {
            let empty_name = if value_column {
                "_value|empty".into()
            } else {
                format!("{base}|empty")
            };
            columns.push(manifest_column(
                &empty_name,
                "BOOLEAN",
                source_name,
                None,
                false,
            ));
        }
    }
    if node_has_kind(node, ValueKind::Null) {
        let null_name = if value_column {
            "_value|n".into()
        } else {
            format!("{base}|n")
        };
        columns.push(manifest_column(
            &null_name,
            "BOOLEAN",
            source_name,
            None,
            false,
        ));
    }
    if kinds.is_empty() && !node_has_kind(node, ValueKind::Null) && !node.indexed_by.is_empty() {
        warnings.insert(format!(
            "indexed path '{}' has no declared or observed BSON type and is not exposed",
            source_name.unwrap_or(base)
        ));
    }
}

fn scalar_mapping(kind: ValueKind) -> (&'static str, BsonKind) {
    match kind {
        ValueKind::String => ("VARCHAR(2000000)", BsonKind::String),
        ValueKind::ObjectId => ("VARCHAR(24)", BsonKind::ObjectId),
        ValueKind::Int32 => ("DECIMAL(10,0)", BsonKind::Int32),
        ValueKind::Int64 => ("DECIMAL(19,0)", BsonKind::Int64),
        ValueKind::Integer => ("DECIMAL(19,0)", BsonKind::Integer),
        ValueKind::Double => ("DOUBLE PRECISION", BsonKind::Double),
        ValueKind::NonFiniteDouble => ("VARCHAR(100)", BsonKind::NonFiniteDouble),
        ValueKind::Boolean => ("BOOLEAN", BsonKind::Boolean),
        ValueKind::Decimal128 => ("VARCHAR(100)", BsonKind::Decimal128),
        ValueKind::DateTime => ("TIMESTAMP(3)", BsonKind::DateTime),
        ValueKind::ExtendedJson => ("VARCHAR(2000000)", BsonKind::ExtendedJson),
        _ => unreachable!("not a scalar kind"),
    }
}

fn merge_integer_kinds(kinds: &mut Vec<ValueKind>) {
    if kinds.contains(&ValueKind::Int32) && kinds.contains(&ValueKind::Int64) {
        kinds.retain(|kind| !matches!(kind, ValueKind::Int32 | ValueKind::Int64));
        kinds.push(ValueKind::Integer);
    }
}

fn observed_count(node: &NodeEvidence, kind: ValueKind) -> u64 {
    match kind {
        ValueKind::Integer => {
            node.observed.get(&ValueKind::Int32).copied().unwrap_or(0)
                + node.observed.get(&ValueKind::Int64).copied().unwrap_or(0)
        }
        _ => node.observed.get(&kind).copied().unwrap_or(0),
    }
}

fn all_kinds(node: &NodeEvidence) -> BTreeSet<ValueKind> {
    node.declared
        .iter()
        .chain(node.observed.keys())
        .copied()
        .collect()
}

fn node_has_kind(node: &NodeEvidence, kind: ValueKind) -> bool {
    node.declared.contains(&kind) || node.observed.contains_key(&kind)
}

fn manifest_column(
    name: &str,
    type_name: &str,
    source_name: Option<&str>,
    bson_type: Option<BsonKind>,
    is_required: bool,
) -> ManifestColumn {
    ManifestColumn {
        name: name.into(),
        type_name: type_name.into(),
        ordinal: 0,
        size: None,
        precision: None,
        scale: None,
        is_required,
        is_null_mask: name.ends_with("|n"),
        source_name: source_name.map(str::to_owned),
        bson_type,
    }
}

fn physical_field_name(source: &str) -> String {
    if source == "_id" {
        return "mongo_id".into();
    }
    if !source.is_empty()
        && !source.contains('|')
        && !matches!(source, "_parent" | "_pos" | "_value")
    {
        return source.into();
    }
    format!("field_{}", short_hash(source.as_bytes()))
}

fn physical_field_names(fields: &BTreeMap<String, NodeEvidence>) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    let mut used = BTreeSet::from([
        "_id".to_owned(),
        "_parent".to_owned(),
        "_pos".to_owned(),
        "_value".to_owned(),
    ]);
    for source in fields.keys() {
        let mut candidate = physical_field_name(source);
        if !used.insert(candidate.clone()) {
            candidate = format!("field_{}", short_hash(source.as_bytes()));
            let mut discriminator = 2;
            while !used.insert(candidate.clone()) {
                candidate = format!("field_{}_{}", short_hash(source.as_bytes()), discriminator);
                discriminator += 1;
            }
        }
        names.insert(source.clone(), candidate);
    }
    names
}

fn identifier(value: &str, uppercase: bool) -> String {
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if output.is_empty() || output.as_bytes()[0].is_ascii_digit() {
        output.insert_str(0, "T_");
    }
    if output != value {
        output.push('_');
        output.push_str(&short_hash(value.as_bytes()));
    }
    if uppercase {
        output.make_ascii_uppercase();
    }
    if output.len() > 96 {
        output.truncate(79);
        output.push('_');
        output.push_str(&short_hash(value.as_bytes()));
    }
    output
}

fn table_name_for(stem: &str, path: &[PathSegment]) -> String {
    let suffix = path
        .iter()
        .map(|segment| {
            let name = if segment.direct {
                "value".into()
            } else {
                identifier(&segment.name, false)
            };
            if segment.kind == PathKind::Array {
                format!("{name}_arr")
            } else {
                name
            }
        })
        .collect::<Vec<_>>()
        .join("_");
    identifier(&format!("{stem}_{suffix}"), false)
}

fn display_path(path: &[PathSegment]) -> String {
    if path.is_empty() {
        return "root".into();
    }
    let mut output = String::new();
    for segment in path {
        if segment.direct {
            output.push_str("[]");
            continue;
        }
        if !output.is_empty() {
            output.push('.');
        }
        output.push_str(&segment.name);
        if segment.kind == PathKind::Array {
            output.push_str("[]");
        }
    }
    output
}

fn short_hash(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    hex(&digest[..6])
}

fn fingerprint(
    evidence: &Evidence,
    config: &InferenceConfig,
    manifest: &ExplicitManifest,
) -> Result<String, UdfError> {
    let mut indexes = evidence
        .raw_indexes
        .iter()
        .map(canonical_bson_document)
        .collect::<Vec<_>>();
    indexes.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
    let payload = doc! {
        "version": 1,
        "collection": evidence.collection_metadata.as_ref().map(canonical_bson_document),
        "indexes": indexes,
        "config": mongodb::bson::to_bson(config).map_err(|_| user("failed to fingerprint inference configuration"))?,
        "manifest": mongodb::bson::to_bson(manifest).map_err(|_| user("failed to fingerprint inferred manifest"))?,
    };
    let encoded = mongodb::bson::to_vec(&payload)
        .map_err(|_| user("failed to encode inference fingerprint"))?;
    Ok(hex(&Sha256::digest(encoded)))
}

fn canonical_bson_document(document: &Document) -> Document {
    let mut entries = document.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    entries
        .into_iter()
        .map(|(key, value)| (key.clone(), canonical_bson(value)))
        .collect()
}

fn canonical_bson(value: &Bson) -> Bson {
    match value {
        Bson::Document(document) => Bson::Document(canonical_bson_document(document)),
        Bson::Array(values) => Bson::Array(values.iter().map(canonical_bson).collect()),
        _ => value.clone(),
    }
}

fn report(evidence: &Evidence) -> InferenceReport {
    let mut paths = Vec::new();
    collect_report_paths(&evidence.root, &mut Vec::new(), &mut paths);
    let mut indexes = evidence.indexes.clone();
    indexes.sort_by(|left, right| left.name.cmp(&right.name));
    InferenceReport {
        complete: evidence.complete,
        sampled_documents: evidence.sampled_documents,
        sampled_bytes: evidence.sampled_bytes,
        metadata_status: evidence
            .metadata_status
            .clone()
            .unwrap_or(EvidenceStatus::NotAuthorized),
        index_status: evidence
            .index_status
            .clone()
            .unwrap_or(EvidenceStatus::NotAuthorized),
        sample_status: evidence
            .sample_status
            .clone()
            .unwrap_or(EvidenceStatus::NotRequested),
        paths,
        indexes,
        warnings: evidence.warnings.iter().cloned().collect(),
    }
}

fn collect_report_paths(
    node: &NodeEvidence,
    path: &mut Vec<String>,
    output: &mut Vec<PathEvidenceReport>,
) {
    for (name, child) in &node.fields {
        path.push(name.clone());
        output.push(PathEvidenceReport {
            path: path.clone(),
            declared: child
                .declared
                .iter()
                .map(|kind| kind.label().into())
                .collect(),
            observed: child
                .observed
                .keys()
                .map(|kind| kind.label().into())
                .collect(),
            indexed_by: child.indexed_by.iter().cloned().collect(),
            required: child.required,
            present_count: child.present_count,
            null_count: child.null_count,
            additional_properties: child.additional_properties,
        });
        collect_report_paths(child, path, output);
        if let Some(items) = &child.items {
            path.push("[]".into());
            output.push(path_report(path, items));
            collect_report_paths(items, path, output);
            path.pop();
        }
        path.pop();
    }
}

fn path_report(path: &[String], node: &NodeEvidence) -> PathEvidenceReport {
    PathEvidenceReport {
        path: path.to_vec(),
        declared: node
            .declared
            .iter()
            .map(|kind| kind.label().into())
            .collect(),
        observed: node
            .observed
            .keys()
            .map(|kind| kind.label().into())
            .collect(),
        indexed_by: node.indexed_by.iter().cloned().collect(),
        required: node.required,
        present_count: node.present_count,
        null_count: node.null_count,
        additional_properties: node.additional_properties,
    }
}

fn hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn mongo_error(operation: &str) -> UdfError {
    UdfError::User(format!("MongoDB error while {operation}"))
}

fn user(message: impl Into<String>) -> UdfError {
    UdfError::User(message.into())
}

#[cfg(test)]
mod tests {
    use mongodb::bson::{Binary, oid::ObjectId, spec::BinarySubtype};

    use super::*;
    use crate::model::ColumnSource;

    fn fixture() -> Evidence {
        let validator = doc! {
            "$jsonSchema": {
                "bsonType": "object",
                "required": ["name"],
                "properties": {
                    "name": {"bsonType": "string"},
                    "age": {"bsonType": ["int", "long", "null"]},
                    "profile": {"bsonType": "object", "properties": {"city": {"bsonType": "string"}}},
                    "tags": {"bsonType": "array", "items": {"bsonType": "string"}}
                }
            },
            "status": {"$exists": true, "$type": "string"}
        };
        let mut evidence = Evidence::default();
        extract_validator(
            &validator,
            &mut evidence.root,
            InferenceConfig::default().max_depth,
            &mut evidence.warnings,
        );
        extract_index(
            &doc! {"name":"email_unique", "key":{"email":1}, "unique":true},
            &mut evidence,
        );
        let config = InferenceConfig::default();
        observe_document(
            &doc! {
                "_id": ObjectId::new(), "name":"Ada", "age": 42_i64,
                "profile":{"city":"Copenhagen"}, "tags":["rust", Bson::Null],
                "payload": Binary { subtype: BinarySubtype::Generic, bytes: vec![1,2] }
            },
            &mut evidence.root,
            0,
            &config,
            &mut evidence.warnings,
        );
        evidence
    }

    #[test]
    fn validator_index_and_sample_evidence_merge_with_provenance() {
        let evidence = fixture();
        assert!(evidence.root.fields["name"].required);
        assert!(
            evidence.root.fields["name"]
                .declared
                .contains(&ValueKind::String)
        );
        assert_eq!(evidence.root.fields["age"].observed[&ValueKind::Int64], 1);
        assert!(
            evidence.root.fields["email"]
                .indexed_by
                .contains("email_unique")
        );
        assert!(evidence.root.fields["status"].required);
        let report = report(&evidence);
        assert!(
            report
                .paths
                .iter()
                .any(|path| path.path == ["email"] && path.indexed_by == ["email_unique"])
        );
    }

    #[test]
    fn resolver_builds_stable_json_table_family() {
        let evidence = fixture();
        let mut warnings = evidence.warnings.clone();
        let first = resolve_manifest("people", &evidence.root, &mut warnings).unwrap();
        let second = resolve_manifest("people", &evidence.root, &mut warnings).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first
                .tables
                .iter()
                .map(|table| table.table_name.as_str())
                .collect::<Vec<_>>(),
            vec!["PEOPLE", "PEOPLE_profile", "PEOPLE_tags_arr"]
        );
        let root = first
            .tables
            .iter()
            .find(|table| table.table_name == "PEOPLE")
            .unwrap();
        assert!(
            root.columns
                .iter()
                .any(|column| column.name == "age" && column.type_name == "DECIMAL(19,0)")
        );
        assert!(
            root.columns.iter().any(|column| column.name == "payload"
                && column.bson_type == Some(BsonKind::ExtendedJson))
        );
        let tags = first
            .tables
            .iter()
            .find(|table| table.table_name == "PEOPLE_tags_arr")
            .unwrap();
        assert!(tags.columns.iter().any(|column| column.name == "_value"));
        assert!(tags.columns.iter().any(|column| column.name == "_value|n"));
    }

    #[test]
    fn hostile_names_and_array_sampling_are_deterministic_and_bounded() {
        assert_eq!(physical_field_name("a|n"), physical_field_name("a|n"));
        assert_ne!(physical_field_name("a|n"), "a|n");
        assert_eq!(distributed_indices(10, 3), vec![0, 4, 9]);
        assert_eq!(distributed_indices(2, 3), vec![0, 1]);
        assert_eq!(distributed_indices(10, 1), vec![0]);
        let fields = BTreeMap::from([
            ("_id".into(), NodeEvidence::default()),
            ("mongo_id".into(), NodeEvidence::default()),
        ]);
        let names = physical_field_names(&fields);
        assert_ne!(names["_id"], names["mongo_id"]);
        assert!(
            table_name_for(
                "ROOT",
                &[PathSegment {
                    name: "a.b$".into(),
                    kind: PathKind::Array,
                    direct: false,
                }]
            )
            .contains("arr")
        );
    }

    #[test]
    fn resolver_represents_arrays_of_arrays_as_direct_path_segments() {
        let mut root = NodeEvidence::default();
        let matrix = root.fields.entry("matrix".into()).or_default();
        matrix.observed.insert(ValueKind::Array, 1);
        let outer_item = matrix.items.get_or_insert_with(Default::default);
        outer_item.observed.insert(ValueKind::Array, 1);
        let inner_item = outer_item.items.get_or_insert_with(Default::default);
        inner_item.observed.insert(ValueKind::Int32, 1);
        let manifest = resolve_manifest("matrices", &root, &mut BTreeSet::new()).unwrap();
        let nested = manifest
            .tables
            .iter()
            .find(|table| table.path_segments.len() == 2)
            .unwrap();
        assert!(nested.path_segments[1].direct);
        assert_eq!(nested.path, "matrix[][]");
        assert!(nested.columns.iter().any(|column| column.name == "_value"));
        let outer = manifest
            .tables
            .iter()
            .find(|table| table.path_segments.len() == 1)
            .unwrap();
        assert!(outer.columns.iter().any(|column| column.name == "_id"));
        assert!(
            outer
                .columns
                .iter()
                .any(|column| column.name == "_value|array")
        );
    }

    #[test]
    fn scalar_and_nested_array_elements_share_an_advertised_direct_union() {
        let mut root = NodeEvidence::default();
        let mut warnings = BTreeSet::new();
        let config = InferenceConfig::default();
        for document in [
            doc! {"k":1, "arr":[1,2,3]},
            doc! {"k":3, "arr":[[1,2],[3]]},
            doc! {"k":5, "arr":["mixed",7]},
        ] {
            observe_document(&document, &mut root, 0, &config, &mut warnings);
        }

        let manifest = resolve_manifest("edge", &root, &mut warnings).unwrap();
        let models = manifest.models().unwrap();
        let outer = models
            .iter()
            .find(|table| table.table_name == "EDGE_arr_arr")
            .unwrap();
        assert!(outer.columns.iter().any(|column| {
            column.exasol_name == "_value" && column.source == ColumnSource::Value
        }));
        assert!(outer.columns.iter().any(|column| {
            column.exasol_name == "_value|string" && column.source == ColumnSource::Value
        }));
        assert!(outer.columns.iter().any(|column| {
            column.exasol_name == "_value|array" && column.source == ColumnSource::ValueArrayLength
        }));
        assert!(models.iter().any(|table| {
            table.table_name == "EDGE_arr_arr_value_arr"
                && table.path.last().is_some_and(|segment| segment.direct)
        }));
    }

    #[test]
    fn object_scalar_and_nested_array_elements_share_one_complete_row_union() {
        let mut root = NodeEvidence::default();
        let mut warnings = BTreeSet::new();
        let config = InferenceConfig::default();
        observe_document(
            &doc! {
                "poly": [
                    {"x": 1, "child": {"y": 2}, "inside": [3]},
                    [4, 5],
                    "tail",
                    true,
                    Bson::Null,
                ]
            },
            &mut root,
            0,
            &config,
            &mut warnings,
        );

        let manifest = resolve_manifest("mixed", &root, &mut warnings).unwrap();
        let models = manifest.models().unwrap();
        let outer = models
            .iter()
            .find(|table| table.table_name == "MIXED_poly_arr")
            .unwrap();
        for expected in [
            "_value",
            "_value|boolean",
            "_value|n",
            "_value|object",
            "_value|array",
            "x",
            "child|object",
            "inside|array",
        ] {
            assert!(
                outer
                    .columns
                    .iter()
                    .any(|column| column.exasol_name == expected),
                "missing mixed array branch {expected}"
            );
        }
        for expected in [
            "MIXED_poly_arr_child",
            "MIXED_poly_arr_inside_arr",
            "MIXED_poly_arr_value_arr",
        ] {
            assert!(
                models.iter().any(|table| table.table_name == expected),
                "missing mixed array child table {expected}"
            );
        }
        assert!(manifest.relationships.iter().any(|relationship| {
            relationship.parent_table == "MIXED_poly_arr"
                && relationship.child_table == "MIXED_poly_arr_child"
        }));
    }

    #[test]
    fn polymorphic_array_elements_keep_one_value_source_across_variants() {
        let mut root = NodeEvidence::default();
        let mut warnings = BTreeSet::new();
        let config = InferenceConfig::default();
        for document in [doc! {"v": [1, 2, 3]}, doc! {"v": ["a", "b"]}] {
            observe_document(&document, &mut root, 0, &config, &mut warnings);
        }

        let manifest = resolve_manifest("polyarr", &root, &mut warnings).unwrap();
        let table = manifest
            .models()
            .unwrap()
            .into_iter()
            .find(|table| table.table_name == "POLYARR_v_arr")
            .unwrap();
        let variants = table
            .columns
            .iter()
            .filter(|column| column.exasol_name.starts_with("_value"))
            .collect::<Vec<_>>();

        assert!(variants.iter().any(|column| {
            column.exasol_name == "_value" && column.source == ColumnSource::Value
        }));
        assert!(variants.iter().any(|column| {
            column.exasol_name == "_value|string" && column.source == ColumnSource::Value
        }));
    }

    #[test]
    fn canonical_fingerprint_ignores_document_key_order() {
        let mut left = Evidence {
            collection_metadata: Some(doc! {"b":2,"a":1}),
            ..Evidence::default()
        };
        let mut right = Evidence {
            collection_metadata: Some(doc! {"a":1,"b":2}),
            ..Evidence::default()
        };
        let mut warnings = BTreeSet::new();
        let manifest = resolve_manifest("empty", &NodeEvidence::default(), &mut warnings).unwrap();
        left.raw_indexes.push(doc! {"name":"x","key":{"a":1}});
        right.raw_indexes.push(doc! {"key":{"a":1},"name":"x"});
        assert_eq!(
            fingerprint(&left, &InferenceConfig::default(), &manifest).unwrap(),
            fingerprint(&right, &InferenceConfig::default(), &manifest).unwrap()
        );
    }

    #[test]
    fn budgets_reject_unbounded_configuration() {
        let config = InferenceConfig {
            sample_size: MAX_SAMPLE_DOCUMENTS + 1,
            ..InferenceConfig::default()
        };
        assert!(config.validate().is_err());
        let config = InferenceConfig {
            max_depth: 0,
            ..InferenceConfig::default()
        };
        assert!(config.validate().is_err());
        for config in [
            InferenceConfig {
                max_sample_bytes: 0,
                ..InferenceConfig::default()
            },
            InferenceConfig {
                max_sample_bytes: MAX_SAMPLE_BYTES + 1,
                ..InferenceConfig::default()
            },
            InferenceConfig {
                max_array_elements: 0,
                ..InferenceConfig::default()
            },
            InferenceConfig {
                max_array_elements: MAX_ARRAY_ELEMENTS + 1,
                ..InferenceConfig::default()
            },
            InferenceConfig {
                max_time_ms: 0,
                ..InferenceConfig::default()
            },
            InferenceConfig {
                max_time_ms: MAX_INFERENCE_TIME_MS + 1,
                ..InferenceConfig::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn exact_validator_fragments_are_extracted_and_opaque_fragments_warn() {
        let validator = doc! {
            "$jsonSchema": {
                "additionalProperties": false,
                "properties": {
                    "number": {"type":"number"},
                    "state": {"enum":["ready", Bson::Null]},
                    "tuple": {"bsonType":"array", "items":[{"bsonType":"string"}]},
                    "choice": {"anyOf":[{"bsonType":"object","required":["conditional"],"properties":{"conditional":{"bsonType":"bool"}}},{"bsonType":"date"}]},
                    "combined": {"allOf":[{"bsonType":"object","required":["certain"],"properties":{"certain":{"bsonType":"string"}}}]},
                    "unknown": {"bsonType": 42},
                    "opaque": {"not":{"bsonType":"string"}, "dependencies":{}, "patternProperties":{}}
                }
            },
            "$and": [
                {"status":{"$in":["a","b"]}},
                {"count":{"$eq":1}},
                {"present":{"$exists":true}}
            ],
            "$or": [{"ignored":1}]
        };
        let mut root = NodeEvidence::default();
        let mut warnings = BTreeSet::new();
        extract_validator(&validator, &mut root, 8, &mut warnings);
        assert_eq!(root.additional_properties, Some(false));
        assert!(
            root.fields["number"]
                .declared
                .contains(&ValueKind::Decimal128)
        );
        assert!(root.fields["state"].declared.contains(&ValueKind::Null));
        assert!(root.fields["combined"].fields["certain"].required);
        assert!(!root.fields["choice"].fields["conditional"].required);
        assert!(root.fields["present"].required);
        assert!(root.fields["status"].declared.contains(&ValueKind::String));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("tuple-style"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("not")));
        assert!(warnings.iter().any(|warning| warning.contains("$or")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("unsupported shape"))
        );
    }

    #[test]
    fn observations_cover_scalar_families_and_depth_and_array_budgets() {
        let config = InferenceConfig {
            max_depth: 2,
            max_array_elements: 2,
            ..InferenceConfig::default()
        };
        let mut root = NodeEvidence::default();
        let mut warnings = BTreeSet::new();
        observe_document(
            &doc! {
                "int": 1_i32,
                "long": 2_i64,
                "double": 1.5,
                "nonfinite": f64::INFINITY,
                "bool": true,
                "date": mongodb::bson::DateTime::from_millis(0),
                "timestamp": mongodb::bson::Timestamp {time:1, increment:2},
                "deep": {"nested":{"too":"far"}},
                "array": [0,1,2,3],
                "undefined": Bson::Undefined,
            },
            &mut root,
            0,
            &config,
            &mut warnings,
        );
        assert!(
            root.fields["nonfinite"]
                .observed
                .contains_key(&ValueKind::NonFiniteDouble)
        );
        assert!(
            root.fields["timestamp"]
                .observed
                .contains_key(&ValueKind::Timestamp)
        );
        assert_eq!(
            root.fields["array"].items.as_ref().unwrap().present_count,
            2
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("depth budget"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("array element budget"))
        );

        let manifest = resolve_manifest("types", &root, &mut warnings).unwrap();
        let columns = &manifest.tables[0].columns;
        assert!(
            columns
                .iter()
                .any(|column| column.name == "timestamp|timestamp_time")
        );
        assert!(
            columns
                .iter()
                .any(|column| column.bson_type == Some(BsonKind::NonFiniteDouble))
        );
    }
}
