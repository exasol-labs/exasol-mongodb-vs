use std::collections::HashSet;

use exasol_udf_sdk::error::UdfError;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

pub const MANIFEST_FORMAT: &str = "exasol-json-tables-source-manifest";
pub const MANIFEST_VERSION: u32 = 1;
pub const MAX_MANIFEST_BYTES: usize = 1_500_000;
const MAX_TABLES: usize = 256;
const MAX_COLUMNS: usize = 10_000;
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplicitManifest {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub stem: String,
    #[serde(default)]
    pub roots: Vec<RootSpec>,
    #[serde(default)]
    pub relationships: Vec<RelationshipSpec>,
    pub tables: Vec<TableSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootSpec {
    pub table_name: String,
    #[serde(default)]
    pub family_tables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipSpec {
    pub parent_table: String,
    pub child_table: String,
    pub segment_name: String,
    pub relation_kind: PathKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableSpec {
    pub table_name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub path_segments: Vec<PathSegment>,
    pub kind: PathKind,
    #[serde(default)]
    pub has_nested_array: bool,
    #[serde(default)]
    pub root_table: String,
    pub columns: Vec<ManifestColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathSegment {
    pub name: String,
    pub kind: PathKind,
    /// Traverse the current value directly. This represents arrays whose
    /// elements are themselves arrays without inventing a field name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub direct: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    Object,
    Array,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestColumn {
    pub name: String,
    pub type_name: String,
    pub ordinal: usize,
    #[serde(default)]
    pub size: Option<u32>,
    #[serde(default)]
    pub precision: Option<u32>,
    #[serde(default)]
    pub scale: Option<u32>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_null_mask: bool,
    /// Exact field name in the current BSON object. This extension is needed
    /// when an Exasol-safe physical name differs from the source name.
    #[serde(default)]
    pub source_name: Option<String>,
    /// Precise BSON branch. When omitted, it is inferred from the JSON Tables
    /// suffix and Exasol type for compatibility with existing manifests.
    #[serde(default)]
    pub bson_type: Option<BsonKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BsonKind {
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
    TimestampTime,
    TimestampIncrement,
    ExtendedJson,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ColumnSource {
    RowId,
    ParentId,
    Position,
    Field { name: String },
    Value,
    NullMask { name: String },
    ValueNullMask,
    EmptyStringMask { name: String },
    ValueEmptyStringMask,
    ValueObjectMarker,
    ObjectLink { name: String },
    ArrayLength { name: String },
    ValueArrayLength,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SqlType {
    Varchar { size: u32 },
    Decimal { precision: u32, scale: u32 },
    Double,
    Boolean,
    Timestamp,
}

impl SqlType {
    pub fn parse(input: &str) -> Result<Self, UdfError> {
        let upper = input.trim().to_ascii_uppercase();
        if let Some(body) = upper
            .strip_prefix("VARCHAR(")
            .and_then(|value| value.strip_suffix(')'))
        {
            let size = body
                .parse::<u32>()
                .ok()
                .filter(|size| *size > 0)
                .ok_or_else(|| user("VARCHAR size must be a positive integer"))?;
            if size > 2_000_000 {
                return Err(user("VARCHAR size exceeds Exasol's 2000000 limit"));
            }
            return Ok(Self::Varchar { size });
        }
        if let Some(body) = upper
            .strip_prefix("DECIMAL(")
            .and_then(|value| value.strip_suffix(')'))
        {
            let (precision, scale) = body
                .split_once(',')
                .ok_or_else(|| user("DECIMAL requires precision and scale"))?;
            let precision = precision.trim().parse::<u32>().unwrap_or(0);
            let scale = scale.trim().parse::<u32>().unwrap_or(u32::MAX);
            if !(1..=36).contains(&precision) || scale > precision {
                return Err(user(
                    "DECIMAL requires 1..36 precision and scale <= precision",
                ));
            }
            return Ok(Self::Decimal { precision, scale });
        }
        match upper.as_str() {
            "DOUBLE" | "DOUBLE PRECISION" => Ok(Self::Double),
            "BOOLEAN" => Ok(Self::Boolean),
            "TIMESTAMP" | "TIMESTAMP(3)" => Ok(Self::Timestamp),
            _ => Err(user(format!("unsupported manifest type '{upper}'"))),
        }
    }

    pub fn exasol_type(&self) -> String {
        match self {
            Self::Varchar { size } => format!("VARCHAR({size})"),
            Self::Decimal { precision, scale } => format!("DECIMAL({precision},{scale})"),
            Self::Double => "DOUBLE PRECISION".into(),
            Self::Boolean => "BOOLEAN".into(),
            Self::Timestamp => "TIMESTAMP(3)".into(),
        }
    }

    pub fn metadata_type(&self) -> Json {
        match self {
            Self::Varchar { size } => json!({"type": "varchar", "size": size}),
            Self::Decimal { precision, scale } => {
                json!({"type": "decimal", "precision": precision, "scale": scale})
            }
            Self::Double => json!({"type": "double"}),
            Self::Boolean => json!({"type": "boolean"}),
            Self::Timestamp => json!({"type": "timestamp"}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnSpec {
    pub source: ColumnSource,
    pub exasol_name: String,
    pub sql_type: SqlType,
    pub bson_kind: Option<BsonKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableModel {
    pub table_name: String,
    pub kind: PathKind,
    pub path: Vec<PathSegment>,
    pub columns: Vec<ColumnSpec>,
}

impl ExplicitManifest {
    pub fn parse(input: &str) -> Result<Self, UdfError> {
        if input.len() > MAX_MANIFEST_BYTES {
            return Err(user(format!(
                "MANIFEST is {} bytes; maximum is {MAX_MANIFEST_BYTES}",
                input.len()
            )));
        }
        let manifest: Self = serde_json::from_str(input).map_err(|error| {
            user(format!(
                "MANIFEST is invalid JSON ({:?} at line {}, column {})",
                error.classify(),
                error.line(),
                error.column()
            ))
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), UdfError> {
        if self.format != MANIFEST_FORMAT {
            return Err(user(format!("MANIFEST format must be '{MANIFEST_FORMAT}'")));
        }
        if self.version != MANIFEST_VERSION {
            return Err(user(format!(
                "unsupported MANIFEST version {}; expected {MANIFEST_VERSION}",
                self.version
            )));
        }
        if self.tables.is_empty() || self.tables.len() > MAX_TABLES {
            return Err(user(format!(
                "MANIFEST must contain 1..={MAX_TABLES} tables"
            )));
        }
        let total_columns: usize = self.tables.iter().map(|table| table.columns.len()).sum();
        if total_columns > MAX_COLUMNS {
            return Err(user(format!(
                "MANIFEST contains {total_columns} columns; maximum is {MAX_COLUMNS}"
            )));
        }

        let mut table_names = HashSet::new();
        let mut root_count = 0;
        for table in &self.tables {
            if table.table_name.is_empty() || !table_names.insert(table.table_name.clone()) {
                return Err(user("MANIFEST table names must be non-empty and unique"));
            }
            if table.path_segments.len() > MAX_DEPTH {
                return Err(user(format!(
                    "table '{}' exceeds the maximum path depth of {MAX_DEPTH}",
                    table.table_name
                )));
            }
            if table.path_segments.is_empty() {
                root_count += 1;
            } else if table.path_segments.last().map(|segment| segment.kind) != Some(table.kind) {
                return Err(user(format!(
                    "table '{}' kind does not match its final path segment",
                    table.table_name
                )));
            }
            if table.columns.is_empty() {
                return Err(user(format!("table '{}' has no columns", table.table_name)));
            }
            let mut names = HashSet::new();
            let mut ordinals = HashSet::new();
            for column in &table.columns {
                if column.name.is_empty()
                    || !names.insert(column.name.clone())
                    || column.ordinal == 0
                    || !ordinals.insert(column.ordinal)
                {
                    return Err(user(format!(
                        "table '{}' has an empty/duplicate column name or ordinal",
                        table.table_name
                    )));
                }
                let _ = SqlType::parse(&column.type_name)?;
            }
            let model = table.to_model()?;
            let has = |source: &ColumnSource| model.columns.iter().any(|col| &col.source == source);
            if table.path_segments.is_empty() && !has(&ColumnSource::RowId) {
                return Err(user(format!(
                    "root table '{}' requires an '_id' row key",
                    table.table_name
                )));
            }
            if table.kind == PathKind::Object && !has(&ColumnSource::RowId) {
                return Err(user(format!(
                    "object table '{}' requires an '_id' row key",
                    table.table_name
                )));
            }
            if table.kind == PathKind::Array
                && (!has(&ColumnSource::ParentId) || !has(&ColumnSource::Position))
            {
                return Err(user(format!(
                    "array table '{}' requires '_parent' and '_pos' columns",
                    table.table_name
                )));
            }
        }
        if root_count != 1 {
            return Err(user(format!(
                "MANIFEST must contain exactly one root table, got {root_count}"
            )));
        }
        Ok(())
    }

    pub fn models(&self) -> Result<Vec<TableModel>, UdfError> {
        self.tables.iter().map(TableSpec::to_model).collect()
    }
}

impl TableSpec {
    fn to_model(&self) -> Result<TableModel, UdfError> {
        let mut columns = self.columns.clone();
        columns.sort_by_key(|column| column.ordinal);
        Ok(TableModel {
            table_name: self.table_name.clone(),
            kind: self.kind,
            path: self.path_segments.clone(),
            columns: columns
                .iter()
                .map(|column| column.to_model())
                .collect::<Result<_, _>>()?,
        })
    }
}

impl ManifestColumn {
    fn to_model(&self) -> Result<ColumnSpec, UdfError> {
        let sql_type = SqlType::parse(&self.type_name)?;
        let (source, suffix_kind) = derive_source(self);
        let bson_kind = self
            .bson_type
            .or(suffix_kind)
            .or_else(|| default_bson_kind(&source, &sql_type));
        validate_column_pair(&self.name, &source, bson_kind, &sql_type)?;
        Ok(ColumnSpec {
            source,
            exasol_name: self.name.clone(),
            sql_type,
            bson_kind,
        })
    }
}

fn derive_source(column: &ManifestColumn) -> (ColumnSource, Option<BsonKind>) {
    let source_name = column.source_name.as_deref();
    match column.name.as_str() {
        "_id" if source_name.is_some() => {
            return (
                ColumnSource::Field {
                    name: source_name.unwrap().into(),
                },
                None,
            );
        }
        "_id" => return (ColumnSource::RowId, None),
        "_parent" => return (ColumnSource::ParentId, None),
        "_pos" => return (ColumnSource::Position, None),
        "_value" => return (ColumnSource::Value, None),
        "_value|n" => return (ColumnSource::ValueNullMask, None),
        "_value|empty" => return (ColumnSource::ValueEmptyStringMask, None),
        "_value|object" => return (ColumnSource::ValueObjectMarker, None),
        "_value|array" => return (ColumnSource::ValueArrayLength, None),
        _ => {}
    }
    if let Some(suffix) = column.name.strip_prefix("_value|") {
        let suffix_kind = variant_suffix(suffix);
        if suffix_kind.is_some() || column.bson_type.is_some() {
            return (ColumnSource::Value, suffix_kind);
        }
    }
    if let Some(base) = column.name.strip_suffix("|n") {
        return (
            ColumnSource::NullMask {
                name: source_name.unwrap_or(base).into(),
            },
            None,
        );
    }
    if let Some(base) = column.name.strip_suffix("|object") {
        return (
            ColumnSource::ObjectLink {
                name: source_name.unwrap_or(base).into(),
            },
            None,
        );
    }
    if let Some(base) = column.name.strip_suffix("|array") {
        return (
            ColumnSource::ArrayLength {
                name: source_name.unwrap_or(base).into(),
            },
            None,
        );
    }
    if let Some(base) = column.name.strip_suffix("|empty") {
        return (
            ColumnSource::EmptyStringMask {
                name: source_name.unwrap_or(base).into(),
            },
            None,
        );
    }
    if let Some((base, suffix)) = column.name.rsplit_once('|')
        && let Some(kind) = variant_suffix(suffix)
    {
        return (
            ColumnSource::Field {
                name: source_name.unwrap_or(base).into(),
            },
            Some(kind),
        );
    }
    (
        ColumnSource::Field {
            name: source_name.unwrap_or(&column.name).into(),
        },
        None,
    )
}

fn variant_suffix(value: &str) -> Option<BsonKind> {
    match value.to_ascii_lowercase().as_str() {
        "string" => Some(BsonKind::String),
        "objectid" | "object_id" => Some(BsonKind::ObjectId),
        "int" | "int32" => Some(BsonKind::Int32),
        "long" | "int64" => Some(BsonKind::Int64),
        "integer" => Some(BsonKind::Integer),
        "number" | "double" => Some(BsonKind::Double),
        "non_finite_double" => Some(BsonKind::NonFiniteDouble),
        "boolean" | "bool" => Some(BsonKind::Boolean),
        "decimal128" => Some(BsonKind::Decimal128),
        "date" | "datetime" => Some(BsonKind::DateTime),
        "timestamp_time" => Some(BsonKind::TimestampTime),
        "timestamp_increment" => Some(BsonKind::TimestampIncrement),
        "extended_json" => Some(BsonKind::ExtendedJson),
        _ => None,
    }
}

fn default_bson_kind(source: &ColumnSource, sql_type: &SqlType) -> Option<BsonKind> {
    match source {
        ColumnSource::RowId
        | ColumnSource::ParentId
        | ColumnSource::Position
        | ColumnSource::NullMask { .. }
        | ColumnSource::ValueNullMask
        | ColumnSource::EmptyStringMask { .. }
        | ColumnSource::ValueEmptyStringMask
        | ColumnSource::ValueObjectMarker
        | ColumnSource::ObjectLink { .. }
        | ColumnSource::ArrayLength { .. }
        | ColumnSource::ValueArrayLength => None,
        ColumnSource::Field { .. } | ColumnSource::Value => Some(match sql_type {
            SqlType::Varchar { .. } => BsonKind::String,
            SqlType::Decimal { .. } => BsonKind::Integer,
            SqlType::Double => BsonKind::Double,
            SqlType::Boolean => BsonKind::Boolean,
            SqlType::Timestamp => BsonKind::DateTime,
        }),
    }
}

fn validate_column_pair(
    name: &str,
    source: &ColumnSource,
    bson_kind: Option<BsonKind>,
    sql_type: &SqlType,
) -> Result<(), UdfError> {
    let valid = match source {
        ColumnSource::RowId | ColumnSource::ParentId | ColumnSource::ObjectLink { .. } => {
            matches!(
                sql_type,
                SqlType::Varchar { size } if *size >= 64
            ) || matches!(
                sql_type,
                SqlType::Decimal {
                    precision: 18..=36,
                    scale: 0
                }
            )
        }
        ColumnSource::Position
        | ColumnSource::ArrayLength { .. }
        | ColumnSource::ValueArrayLength => {
            matches!(sql_type, SqlType::Decimal { scale: 0, .. })
        }
        ColumnSource::NullMask { .. }
        | ColumnSource::ValueNullMask
        | ColumnSource::EmptyStringMask { .. }
        | ColumnSource::ValueEmptyStringMask
        | ColumnSource::ValueObjectMarker => {
            matches!(sql_type, SqlType::Boolean)
        }
        ColumnSource::Field { .. } | ColumnSource::Value => match bson_kind {
            Some(
                BsonKind::String
                | BsonKind::ObjectId
                | BsonKind::Decimal128
                | BsonKind::NonFiniteDouble
                | BsonKind::ExtendedJson,
            ) => {
                matches!(sql_type, SqlType::Varchar { .. })
            }
            Some(
                BsonKind::Int32
                | BsonKind::Int64
                | BsonKind::Integer
                | BsonKind::TimestampTime
                | BsonKind::TimestampIncrement,
            ) => {
                matches!(sql_type, SqlType::Decimal { scale: 0, .. })
            }
            Some(BsonKind::Double) => matches!(sql_type, SqlType::Double),
            Some(BsonKind::Boolean) => matches!(sql_type, SqlType::Boolean),
            Some(BsonKind::DateTime) => matches!(sql_type, SqlType::Timestamp),
            None => false,
        },
    };
    if valid {
        Ok(())
    } else {
        Err(user(format!(
            "column '{name}' has an incompatible source/BSON/Exasol type combination"
        )))
    }
}

fn user(message: impl Into<String>) -> UdfError {
    UdfError::User(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json() -> String {
        json!({
            "format": MANIFEST_FORMAT,
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
                        {"name":"name|n","typeName":"BOOLEAN","ordinal":4,"isNullMask":true},
                        {"name":"tags|array","typeName":"DECIMAL(18,0)","ordinal":5}
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
                        {"name":"_value","typeName":"VARCHAR(2000000)","ordinal":3},
                        {"name":"_value|n","typeName":"BOOLEAN","ordinal":4,"isNullMask":true}
                    ]
                }
            ]
        }).to_string()
    }

    #[test]
    fn parses_json_tables_manifest_and_derives_sources() {
        let manifest = ExplicitManifest::parse(&manifest_json()).unwrap();
        let models = manifest.models().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].columns[0].source, ColumnSource::RowId);
        assert_eq!(
            models[0].columns[1].source,
            ColumnSource::Field { name: "_id".into() }
        );
        assert_eq!(
            models[0].columns[4].source,
            ColumnSource::ArrayLength {
                name: "tags".into()
            }
        );
        assert_eq!(models[1].columns[0].source, ColumnSource::ParentId);
        assert_eq!(models[1].columns[2].source, ColumnSource::Value);
    }

    #[test]
    fn hostile_names_do_not_need_delimiter_parsing() {
        let mut input: Json = serde_json::from_str(&manifest_json()).unwrap();
        input["tables"][0]["columns"][2]["name"] = json!("SQL \"name\"");
        input["tables"][0]["columns"][2]["sourceName"] = json!("a,b:c|[]$.");
        input["tables"][0]["columns"][3]["sourceName"] = json!("a,b:c|[]$.");
        let manifest = ExplicitManifest::parse(&input.to_string()).unwrap();
        let models = manifest.models().unwrap();
        let columns = &models[0].columns;
        assert_eq!(
            columns[2].source,
            ColumnSource::Field {
                name: "a,b:c|[]$.".into()
            }
        );
        assert_eq!(
            columns[3].source,
            ColumnSource::NullMask {
                name: "a,b:c|[]$.".into()
            }
        );
    }

    #[test]
    fn rejects_unknown_version_without_echoing_manifest() {
        let marker = "private-field-name";
        let input = format!(
            "{{\"format\":\"{MANIFEST_FORMAT}\",\"version\":99,\"marker\":\"{marker}\",\"tables\":[]}}"
        );
        let error = ExplicitManifest::parse(&input).unwrap_err().to_string();
        assert!(error.contains("unsupported MANIFEST version"));
        assert!(!error.contains(marker));
    }

    #[test]
    fn checked_in_example_is_a_valid_json_tables_manifest() {
        let input = include_str!("../../../examples/people.source_manifest.json");
        let manifest = ExplicitManifest::parse(input).unwrap();
        assert_eq!(manifest.models().unwrap().len(), 5);
    }

    #[test]
    fn rejects_oversized_manifest_before_parsing_it() {
        let input = "x".repeat(MAX_MANIFEST_BYTES + 1);
        let error = ExplicitManifest::parse(&input).unwrap_err().to_string();
        assert!(error.contains("maximum"));
        assert!(error.contains(&(MAX_MANIFEST_BYTES + 1).to_string()));
    }

    #[test]
    fn parses_every_supported_sql_type_and_rejects_invalid_types() {
        let cases = [
            (" varchar(12) ", SqlType::Varchar { size: 12 }),
            (
                "decimal(36, 9)",
                SqlType::Decimal {
                    precision: 36,
                    scale: 9,
                },
            ),
            ("double precision", SqlType::Double),
            ("boolean", SqlType::Boolean),
            ("timestamp(3)", SqlType::Timestamp),
        ];
        for (input, expected) in cases {
            let parsed = SqlType::parse(input).unwrap();
            assert_eq!(parsed, expected);
            assert!(!parsed.exasol_type().is_empty());
            assert!(parsed.metadata_type().is_object());
        }
        for invalid in [
            "VARCHAR(0)",
            "VARCHAR(2000001)",
            "DECIMAL(0,0)",
            "DECIMAL(5,6)",
            "TEXT",
        ] {
            assert!(SqlType::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn validation_rejects_broken_table_contracts() {
        type ManifestMutation = (&'static str, Box<dyn Fn(&mut Json)>);
        let base: Json = serde_json::from_str(&manifest_json()).unwrap();
        let mutations: &[ManifestMutation] = &[
            ("format", Box::new(|v| v["format"] = json!("wrong"))),
            (
                "duplicate table",
                Box::new(|v| v["tables"][1]["tableName"] = json!("PEOPLE")),
            ),
            (
                "missing root key",
                Box::new(|v| {
                    v["tables"][0]["columns"].as_array_mut().unwrap().remove(0);
                }),
            ),
            (
                "array key",
                Box::new(|v| {
                    v["tables"][1]["columns"].as_array_mut().unwrap().remove(0);
                }),
            ),
            (
                "kind",
                Box::new(|v| v["tables"][1]["kind"] = json!("object")),
            ),
            (
                "duplicate column",
                Box::new(|v| v["tables"][0]["columns"][1]["name"] = json!("_id")),
            ),
            (
                "empty columns",
                Box::new(|v| v["tables"][0]["columns"] = json!([])),
            ),
        ];
        for (label, mutate) in mutations {
            let mut value = base.clone();
            mutate(&mut value);
            assert!(
                ExplicitManifest::parse(&value.to_string()).is_err(),
                "{label}"
            );
        }
    }

    #[test]
    fn derives_masks_links_variants_and_value_sources() {
        let column = |name: &str, type_name: &str| ManifestColumn {
            name: name.into(),
            type_name: type_name.into(),
            ordinal: 1,
            size: None,
            precision: None,
            scale: None,
            is_required: false,
            is_null_mask: false,
            source_name: None,
            bson_type: None,
        };
        let cases = [
            ("_value|object", "BOOLEAN", ColumnSource::ValueObjectMarker),
            (
                "_value|empty",
                "BOOLEAN",
                ColumnSource::ValueEmptyStringMask,
            ),
            (
                "name|empty",
                "BOOLEAN",
                ColumnSource::EmptyStringMask {
                    name: "name".into(),
                },
            ),
            (
                "child|object",
                "VARCHAR(64)",
                ColumnSource::ObjectLink {
                    name: "child".into(),
                },
            ),
            (
                "items|array",
                "DECIMAL(18,0)",
                ColumnSource::ArrayLength {
                    name: "items".into(),
                },
            ),
            (
                "v|int32",
                "DECIMAL(10,0)",
                ColumnSource::Field { name: "v".into() },
            ),
            ("_value|string", "VARCHAR(20)", ColumnSource::Value),
            (
                "_value|non_finite_double",
                "VARCHAR(100)",
                ColumnSource::Value,
            ),
            (
                "_value|timestamp_time",
                "DECIMAL(10,0)",
                ColumnSource::Value,
            ),
        ];
        for (name, type_name, source) in cases {
            assert_eq!(column(name, type_name).to_model().unwrap().source, source);
        }
        assert_eq!(variant_suffix("OBJECT_ID"), Some(BsonKind::ObjectId));
        assert_eq!(variant_suffix("unknown"), None);
        let mut explicitly_typed = column("_value|future", "VARCHAR(20)");
        explicitly_typed.bson_type = Some(BsonKind::String);
        assert_eq!(
            explicitly_typed.to_model().unwrap().source,
            ColumnSource::Value
        );
        assert!(column("v|int32", "BOOLEAN").to_model().is_err());
    }
}
