use std::collections::HashSet;

use chrono::DateTime as ChronoDateTime;
use exasol_udf_sdk::context::UdfContext;
use exasol_udf_sdk::error::UdfError;
use exasol_udf_sdk::value::{Decimal, Value};
use futures_util::TryStreamExt;
use mongodb::bson::{Bson, Document};
use sha2::{Digest, Sha256};

use crate::connection;
use crate::model::{BsonKind, ColumnSource, ColumnSpec, PathKind, PathSegment, SqlType};
use crate::mongo_plan::{MongoReadPlan, ROOT_ID_FIELD, position_field, projected_value};
use crate::pushdown::{AGGREGATE_COUNT_FIELD, MongoAggregation};
use crate::wire::MongoScanSpec;

pub fn run_scan(ctx: &mut dyn UdfContext) -> Result<(), UdfError> {
    let input = ctx
        .get_string(0)?
        .ok_or_else(|| UdfError::User("MONGODB_SCAN requires a non-NULL scan spec".into()))?;
    let spec = MongoScanSpec::from_json(input)?;

    // Connect-back is synchronous and must happen before Tokio owns the thread.
    let resolved = connection::resolve(ctx, &spec.connection_name)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| UdfError::User("failed to create the MongoDB scan runtime".into()))?;
    runtime.block_on(run_cursor(ctx, &spec, &resolved))
}

async fn run_cursor(
    ctx: &mut dyn UdfContext,
    spec: &MongoScanSpec,
    resolved: &exasol_udf_sdk::connect_back::ConnectionObject,
) -> Result<(), UdfError> {
    let client = connection::client(resolved).await?;
    let collection = client
        .database(&spec.database)
        .collection::<Document>(&spec.collection);

    match spec.plan.pipeline_with(&spec.pushdown, &spec.columns) {
        None => {
            let mut cursor = collection
                .find(Document::new())
                .batch_size(spec.batch_size)
                .await
                .map_err(|_| mongo_error("opening the root cursor"))?;
            while let Some(document) = cursor
                .try_next()
                .await
                .map_err(|_| mongo_error("reading the root cursor"))?
            {
                let root_id = document
                    .get("_id")
                    .ok_or_else(|| UdfError::User("MongoDB root document has no '_id'".into()))?;
                emit_row(ctx, spec, root_id, &Bson::Document(document.clone()), &[])?;
            }
        }
        Some(pipeline) => {
            let mut cursor = collection
                .aggregate(pipeline)
                .batch_size(spec.batch_size)
                .await
                .map_err(|_| mongo_error("opening the nested cursor"))?;
            if spec.pushdown.aggregation == Some(MongoAggregation::CountStar) {
                let document = cursor
                    .try_next()
                    .await
                    .map_err(|_| mongo_error("reading the aggregate cursor"))?;
                let count = document
                    .as_ref()
                    .map(aggregate_count)
                    .transpose()?
                    .unwrap_or(0);
                if cursor
                    .try_next()
                    .await
                    .map_err(|_| mongo_error("reading the aggregate cursor"))?
                    .is_some()
                {
                    return Err(UdfError::User(
                        "MongoDB returned multiple rows for a single-group aggregate".into(),
                    ));
                }
                ctx.emit(&[Value::Numeric(Decimal {
                    unscaled: i128::from(count),
                    scale: 0,
                })])?;
                return Ok(());
            }
            while let Some(document) = cursor
                .try_next()
                .await
                .map_err(|_| mongo_error("reading the nested cursor"))?
            {
                let root_id = document.get(ROOT_ID_FIELD).ok_or_else(|| {
                    UdfError::User("MongoDB nested row has no root identity".into())
                })?;
                let current = projected_value(&document).ok_or_else(|| {
                    UdfError::User("MongoDB nested row has no projected value".into())
                })?;
                let ordinals = collect_ordinals(&document, spec.plan.path())?;
                emit_row(ctx, spec, root_id, current, &ordinals)?;
            }
        }
    }
    Ok(())
}

fn aggregate_count(document: &Document) -> Result<i64, UdfError> {
    match document.get(AGGREGATE_COUNT_FIELD) {
        Some(Bson::Int32(value)) if *value >= 0 => Ok(i64::from(*value)),
        Some(Bson::Int64(value)) if *value >= 0 => Ok(*value),
        _ => Err(UdfError::User(
            "MongoDB returned an invalid COUNT result".into(),
        )),
    }
}

fn collect_ordinals(document: &Document, path: &[PathSegment]) -> Result<Vec<u64>, UdfError> {
    path.iter()
        .enumerate()
        .filter(|(_, segment)| segment.kind == PathKind::Array)
        .map(|(index, _)| {
            let field = position_field(index);
            let value = document
                .get(&field)
                .ok_or_else(|| UdfError::User("MongoDB array row has no array position".into()))?;
            match value {
                Bson::Int32(value) if *value >= 0 => Ok(*value as u64),
                Bson::Int64(value) if *value >= 0 => Ok(*value as u64),
                _ => Err(UdfError::User(
                    "MongoDB returned an invalid array position".into(),
                )),
            }
        })
        .collect()
}

fn emit_row(
    ctx: &mut dyn UdfContext,
    spec: &MongoScanSpec,
    root_id: &Bson,
    current: &Bson,
    ordinals: &[u64],
) -> Result<(), UdfError> {
    validate_scalar_branches(current, &spec.columns)?;
    let row = spec
        .columns
        .iter()
        .map(|column| column_value(column, &spec.plan, root_id, current, ordinals))
        .collect::<Result<Vec<_>, _>>()?;
    ctx.emit(&row)
}

fn validate_scalar_branches(current: &Bson, columns: &[ColumnSpec]) -> Result<(), UdfError> {
    let mut checked = HashSet::new();
    for column in columns {
        let key = match &column.source {
            ColumnSource::Field { name } => Some((false, name.as_str())),
            ColumnSource::Value => Some((true, "")),
            ColumnSource::ValueArrayLength => Some((true, "")),
            _ => None,
        };
        let Some(key) = key else { continue };
        if !checked.insert(key) {
            continue;
        }
        let value = if key.0 {
            Some(current)
        } else {
            current
                .as_document()
                .and_then(|document| document.get(key.1))
        };
        let Some(value) = value else { continue };
        if matches!(value, Bson::Null) {
            continue;
        }
        let accepted = columns
            .iter()
            .filter(|candidate| match (&candidate.source, key.0) {
                (ColumnSource::Value, true) => true,
                (ColumnSource::ValueArrayLength, true) => true,
                (ColumnSource::Field { name }, false) => name == key.1,
                _ => false,
            })
            .any(|candidate| {
                matches!(candidate.source, ColumnSource::ValueArrayLength)
                    && matches!(value, Bson::Array(_))
                    || candidate
                        .bson_kind
                        .is_some_and(|kind| kind_accepts(kind, value))
            });
        if !accepted {
            let field = if key.0 { "array value" } else { key.1 };
            return Err(UdfError::User(format!(
                "MongoDB {field} has unadvertised BSON type {}; refresh or change MANIFEST",
                bson_tag(value)
            )));
        }
    }
    Ok(())
}

fn column_value(
    column: &ColumnSpec,
    plan: &MongoReadPlan,
    root_id: &Bson,
    current: &Bson,
    ordinals: &[u64],
) -> Result<Value, UdfError> {
    match &column.source {
        ColumnSource::RowId => structural_id_value(
            root_id,
            plan.path(),
            ordinals,
            &column.sql_type,
            &column.exasol_name,
        ),
        ColumnSource::ParentId => {
            let parent_path = plan
                .path()
                .get(..plan.path().len().saturating_sub(1))
                .unwrap_or(&[]);
            let parent_ordinals = ordinals
                .get(..ordinals.len().saturating_sub(1))
                .unwrap_or(&[]);
            structural_id_value(
                root_id,
                parent_path,
                parent_ordinals,
                &column.sql_type,
                &column.exasol_name,
            )
        }
        ColumnSource::Position => ordinals
            .last()
            .copied()
            .map(|value| integer_value(i128::from(value), &column.sql_type, &column.exasol_name))
            .transpose()?
            .ok_or_else(|| UdfError::User("array row is missing its position".into())),
        ColumnSource::Field { name } => {
            let value = current
                .as_document()
                .and_then(|document| document.get(name));
            scalar_value(value, column)
        }
        ColumnSource::Value => scalar_value(Some(current), column),
        ColumnSource::ValueArrayLength => match current {
            Bson::Array(values) => {
                integer_value(values.len() as i128, &column.sql_type, &column.exasol_name)
            }
            _ => Ok(Value::Null),
        },
        ColumnSource::NullMask { name } => Ok(Value::Bool(matches!(
            current
                .as_document()
                .and_then(|document| document.get(name)),
            Some(Bson::Null)
        ))),
        ColumnSource::ValueNullMask => Ok(Value::Bool(matches!(current, Bson::Null))),
        ColumnSource::EmptyStringMask { name } => Ok(Value::Bool(matches!(
            current
                .as_document()
                .and_then(|document| document.get(name)),
            Some(Bson::String(value)) if value.is_empty()
        ))),
        ColumnSource::ValueEmptyStringMask => Ok(Value::Bool(matches!(
            current,
            Bson::String(value) if value.is_empty()
        ))),
        ColumnSource::ObjectLink { name } => {
            let value = current
                .as_document()
                .and_then(|document| document.get(name));
            match value {
                None | Some(Bson::Null) => Ok(Value::Null),
                Some(Bson::Document(_)) => {
                    let mut path = plan.path().to_vec();
                    path.push(PathSegment {
                        name: name.clone(),
                        kind: PathKind::Object,
                        direct: false,
                    });
                    structural_id_value(
                        root_id,
                        &path,
                        ordinals,
                        &column.sql_type,
                        &column.exasol_name,
                    )
                }
                Some(other) => Err(structural_drift(name, "object", other)),
            }
        }
        ColumnSource::ArrayLength { name } => {
            let value = current
                .as_document()
                .and_then(|document| document.get(name));
            match value {
                None | Some(Bson::Null) => Ok(Value::Null),
                Some(Bson::Array(values)) => {
                    integer_value(values.len() as i128, &column.sql_type, &column.exasol_name)
                }
                Some(other) => Err(structural_drift(name, "array", other)),
            }
        }
    }
}

fn scalar_value(value: Option<&Bson>, column: &ColumnSpec) -> Result<Value, UdfError> {
    let Some(value) = value else {
        return Ok(Value::Null);
    };
    if matches!(value, Bson::Null) {
        return Ok(Value::Null);
    }
    let kind = column
        .bson_kind
        .expect("validated scalar column has BSON kind");
    if !kind_accepts(kind, value) {
        return Ok(Value::Null);
    }
    match (kind, value) {
        (BsonKind::String, Bson::String(value)) => {
            string_value(value.clone(), &column.sql_type, &column.exasol_name)
        }
        (BsonKind::ObjectId, Bson::ObjectId(value)) => {
            string_value(value.to_hex(), &column.sql_type, &column.exasol_name)
        }
        (BsonKind::Int32, Bson::Int32(value)) => {
            integer_value(i128::from(*value), &column.sql_type, &column.exasol_name)
        }
        (BsonKind::Int64, Bson::Int64(value)) => {
            integer_value(i128::from(*value), &column.sql_type, &column.exasol_name)
        }
        (BsonKind::Integer, Bson::Int32(value)) => {
            integer_value(i128::from(*value), &column.sql_type, &column.exasol_name)
        }
        (BsonKind::Integer, Bson::Int64(value)) => {
            integer_value(i128::from(*value), &column.sql_type, &column.exasol_name)
        }
        (BsonKind::Double, Bson::Double(value)) => Ok(Value::Double(*value)),
        (BsonKind::Boolean, Bson::Boolean(value)) => Ok(Value::Bool(*value)),
        (BsonKind::DateTime, Bson::DateTime(value)) => {
            let timestamp = ChronoDateTime::from_timestamp_millis(value.timestamp_millis())
                .ok_or_else(|| {
                    UdfError::User(format!(
                        "column '{}' contains a BSON DateTime outside Exasol's supported range",
                        column.exasol_name
                    ))
                })?;
            Ok(Value::Timestamp(timestamp.naive_utc()))
        }
        (BsonKind::TimestampTime, Bson::Timestamp(value)) => integer_value(
            i128::from(value.time),
            &column.sql_type,
            &column.exasol_name,
        ),
        (BsonKind::TimestampIncrement, Bson::Timestamp(value)) => integer_value(
            i128::from(value.increment),
            &column.sql_type,
            &column.exasol_name,
        ),
        (BsonKind::Decimal128 | BsonKind::NonFiniteDouble | BsonKind::ExtendedJson, _) => {
            string_value(
                canonical_ext_json(value),
                &column.sql_type,
                &column.exasol_name,
            )
        }
        _ => unreachable!("BSON branch compatibility checked above"),
    }
}

fn kind_accepts(kind: BsonKind, value: &Bson) -> bool {
    match (kind, value) {
        (BsonKind::String, Bson::String(_))
        | (BsonKind::ObjectId, Bson::ObjectId(_))
        | (BsonKind::Int32, Bson::Int32(_))
        | (BsonKind::Int64, Bson::Int64(_))
        | (BsonKind::Integer, Bson::Int32(_) | Bson::Int64(_))
        | (BsonKind::Boolean, Bson::Boolean(_))
        | (BsonKind::Decimal128, Bson::Decimal128(_))
        | (BsonKind::TimestampTime | BsonKind::TimestampIncrement, Bson::Timestamp(_)) => true,
        (BsonKind::Double, Bson::Double(value)) => value.is_finite(),
        (BsonKind::NonFiniteDouble, Bson::Double(value)) => !value.is_finite(),
        (BsonKind::DateTime, Bson::DateTime(value)) => {
            ChronoDateTime::from_timestamp_millis(value.timestamp_millis()).is_some()
        }
        (BsonKind::ExtendedJson, value) => {
            matches!(
                value,
                Bson::Binary(_)
                    | Bson::RegularExpression(_)
                    | Bson::JavaScriptCode(_)
                    | Bson::JavaScriptCodeWithScope(_)
                    | Bson::DbPointer(_)
                    | Bson::Symbol(_)
                    | Bson::Undefined
                    | Bson::MinKey
                    | Bson::MaxKey
            ) || matches!(
                value,
                Bson::DateTime(value)
                    if ChronoDateTime::from_timestamp_millis(value.timestamp_millis()).is_none()
            )
        }
        _ => false,
    }
}

fn integer_value(value: i128, sql_type: &SqlType, column: &str) -> Result<Value, UdfError> {
    let SqlType::Decimal {
        precision,
        scale: 0,
    } = sql_type
    else {
        return Err(UdfError::User(format!(
            "column '{column}' is not an integer DECIMAL"
        )));
    };
    let digits = value.unsigned_abs().to_string().len() as u32;
    if digits > *precision {
        return Err(UdfError::User(format!(
            "column '{column}' integer requires {digits} digits but is DECIMAL({precision},0)"
        )));
    }
    Ok(Value::Numeric(Decimal {
        unscaled: value,
        scale: 0,
    }))
}

fn string_value(value: String, sql_type: &SqlType, column: &str) -> Result<Value, UdfError> {
    let SqlType::Varchar { size } = sql_type else {
        return Err(UdfError::User(format!("column '{column}' is not VARCHAR")));
    };
    let length = value.chars().count();
    if length > *size as usize {
        return Err(UdfError::User(format!(
            "column '{column}' value has {length} characters but is VARCHAR({size})"
        )));
    }
    Ok(Value::String(value))
}

#[cfg(test)]
fn stable_id(root_id: &Bson, path: &[PathSegment], ordinals: &[u64]) -> Result<String, UdfError> {
    Ok(hex_digest(&stable_digest(root_id, path, ordinals)?))
}

fn structural_id_value(
    root_id: &Bson,
    path: &[PathSegment],
    ordinals: &[u64],
    sql_type: &SqlType,
    column: &str,
) -> Result<Value, UdfError> {
    let digest = stable_digest(root_id, path, ordinals)?;
    match sql_type {
        SqlType::Varchar { .. } => string_value(hex_digest(&digest), sql_type, column),
        SqlType::Decimal {
            precision,
            scale: 0,
        } => {
            // Existing exasol-json-tables manifests use DECIMAL(18,0) keys.
            // Fold 120 digest bits into the declared width; VARCHAR(64) keeps
            // the complete SHA-256 when a new contract can choose its type.
            let mut bytes = [0u8; 16];
            bytes[1..].copy_from_slice(&digest[..15]);
            let raw = i128::from_be_bytes(bytes);
            let modulus = 10i128.pow(*precision);
            integer_value(raw % modulus, sql_type, column)
        }
        _ => Err(UdfError::User(format!(
            "structural column '{column}' must be VARCHAR(64+) or DECIMAL(18..36,0)"
        ))),
    }
}

fn stable_digest(
    root_id: &Bson,
    path: &[PathSegment],
    ordinals: &[u64],
) -> Result<[u8; 32], UdfError> {
    let encoded_root = mongodb::bson::to_vec(&mongodb::bson::doc! {"_id": root_id.clone()})
        .map_err(|_| UdfError::User("failed to encode MongoDB document identity".into()))?;
    let mut digest = Sha256::new();
    digest.update(b"exasol-mongodb-vs-row-id\0v1");
    put_bytes(&mut digest, &encoded_root);
    digest.update((path.len() as u32).to_be_bytes());
    for segment in path {
        digest.update([match segment.kind {
            PathKind::Object => 0,
            PathKind::Array => 1,
        }]);
        digest.update([u8::from(segment.direct)]);
        put_bytes(&mut digest, segment.name.as_bytes());
    }
    digest.update((ordinals.len() as u32).to_be_bytes());
    for ordinal in ordinals {
        digest.update(ordinal.to_be_bytes());
    }
    Ok(digest.finalize().into())
}

fn put_bytes(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u32).to_be_bytes());
    digest.update(value);
}

fn hex_digest(value: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn canonical_ext_json(value: &Bson) -> String {
    value.clone().into_canonical_extjson().to_string()
}

fn bson_tag(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "boolean",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascript-with-scope",
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binary",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "dateTime",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal128",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

fn structural_drift(field: &str, expected: &str, actual: &Bson) -> UdfError {
    UdfError::User(format!(
        "MongoDB field '{field}' is {}, expected {expected}; refresh or change MANIFEST",
        bson_tag(actual)
    ))
}

fn mongo_error(operation: &str) -> UdfError {
    // Driver errors can include the URI or server reply. Keep observable errors
    // generic until Milestone 4 introduces structured redaction.
    UdfError::User(format!("MongoDB error while {operation}"))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mongodb::bson::{
        Binary, DateTime, Decimal128, Timestamp, oid::ObjectId, spec::BinarySubtype,
    };

    use super::*;

    fn scalar(name: &str, source: ColumnSource, kind: BsonKind, sql_type: SqlType) -> ColumnSpec {
        ColumnSpec {
            source,
            exasol_name: name.into(),
            sql_type,
            bson_kind: Some(kind),
        }
    }

    #[test]
    fn validates_mongodb_count_results() {
        assert_eq!(
            aggregate_count(&mongodb::bson::doc! {AGGREGATE_COUNT_FIELD: 7}).unwrap(),
            7
        );
        assert_eq!(
            aggregate_count(&mongodb::bson::doc! {AGGREGATE_COUNT_FIELD: i64::MAX}).unwrap(),
            i64::MAX
        );
        for invalid in [
            mongodb::bson::doc! {},
            mongodb::bson::doc! {AGGREGATE_COUNT_FIELD: -1},
            mongodb::bson::doc! {AGGREGATE_COUNT_FIELD: "7"},
            mongodb::bson::doc! {AGGREGATE_COUNT_FIELD: 7.0},
        ] {
            assert!(aggregate_count(&invalid).is_err());
        }
    }

    #[test]
    fn converts_all_direct_scalar_families() {
        let string = scalar(
            "s",
            ColumnSource::Value,
            BsonKind::String,
            SqlType::Varchar { size: 10 },
        );
        assert_eq!(
            scalar_value(Some(&Bson::String("".into())), &string).unwrap(),
            Value::String("".into())
        );
        let oid = ObjectId::new();
        let object_id = scalar(
            "id",
            ColumnSource::Value,
            BsonKind::ObjectId,
            SqlType::Varchar { size: 24 },
        );
        assert_eq!(
            scalar_value(Some(&Bson::ObjectId(oid)), &object_id).unwrap(),
            Value::String(oid.to_hex())
        );
        let integer = scalar(
            "n",
            ColumnSource::Value,
            BsonKind::Int64,
            SqlType::Decimal {
                precision: 19,
                scale: 0,
            },
        );
        assert_eq!(
            scalar_value(Some(&Bson::Int64(i64::MIN)), &integer).unwrap(),
            Value::Numeric(Decimal {
                unscaled: i64::MIN as i128,
                scale: 0
            })
        );
        let date = scalar(
            "d",
            ColumnSource::Value,
            BsonKind::DateTime,
            SqlType::Timestamp,
        );
        assert!(matches!(
            scalar_value(Some(&Bson::DateTime(DateTime::from_millis(0))), &date).unwrap(),
            Value::Timestamp(_)
        ));

        let boolean = scalar(
            "b",
            ColumnSource::Value,
            BsonKind::Boolean,
            SqlType::Boolean,
        );
        assert_eq!(
            scalar_value(Some(&Bson::Boolean(true)), &boolean).unwrap(),
            Value::Bool(true)
        );
        let double = scalar("f", ColumnSource::Value, BsonKind::Double, SqlType::Double);
        assert_eq!(
            scalar_value(Some(&Bson::Double(1.5)), &double).unwrap(),
            Value::Double(1.5)
        );
        let decimal = scalar(
            "decimal",
            ColumnSource::Value,
            BsonKind::Decimal128,
            SqlType::Varchar { size: 100 },
        );
        let decimal_value = Bson::Decimal128(Decimal128::from_str("123.45").unwrap());
        assert!(matches!(
            scalar_value(Some(&decimal_value), &decimal).unwrap(),
            Value::String(value) if value.contains("$numberDecimal")
        ));
        let nonfinite = scalar(
            "nonfinite",
            ColumnSource::Value,
            BsonKind::NonFiniteDouble,
            SqlType::Varchar { size: 100 },
        );
        assert!(matches!(
            scalar_value(Some(&Bson::Double(f64::NAN)), &nonfinite).unwrap(),
            Value::String(value) if value.contains("$numberDouble")
        ));
        let extended = scalar(
            "binary",
            ColumnSource::Value,
            BsonKind::ExtendedJson,
            SqlType::Varchar { size: 200 },
        );
        let binary = Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: vec![0, 1, 2],
        });
        assert!(matches!(
            scalar_value(Some(&binary), &extended).unwrap(),
            Value::String(value) if value.contains("$binary")
        ));
        let timestamp = Bson::Timestamp(Timestamp {
            time: 42,
            increment: 7,
        });
        let time = scalar(
            "time",
            ColumnSource::Value,
            BsonKind::TimestampTime,
            SqlType::Decimal {
                precision: 10,
                scale: 0,
            },
        );
        assert_eq!(
            scalar_value(Some(&timestamp), &time).unwrap(),
            Value::Numeric(Decimal {
                unscaled: 42,
                scale: 0
            })
        );
    }

    #[test]
    fn variants_are_exclusive_and_unadvertised_drift_fails() {
        let columns = vec![
            scalar(
                "v",
                ColumnSource::Field { name: "v".into() },
                BsonKind::Integer,
                SqlType::Decimal {
                    precision: 19,
                    scale: 0,
                },
            ),
            scalar(
                "v|string",
                ColumnSource::Field { name: "v".into() },
                BsonKind::String,
                SqlType::Varchar { size: 20 },
            ),
        ];
        let current = Bson::Document(mongodb::bson::doc! {"v": "hello"});
        validate_scalar_branches(&current, &columns).unwrap();
        assert_eq!(
            scalar_value(current.as_document().unwrap().get("v"), &columns[0]).unwrap(),
            Value::Null
        );
        assert_eq!(
            scalar_value(current.as_document().unwrap().get("v"), &columns[1]).unwrap(),
            Value::String("hello".into())
        );
        let drift = Bson::Document(mongodb::bson::doc! {"v": true});
        assert!(validate_scalar_branches(&drift, &columns).is_err());
    }

    #[test]
    fn array_value_variants_route_to_the_matching_advertised_branch() {
        let columns = vec![
            scalar(
                "_value",
                ColumnSource::Value,
                BsonKind::Integer,
                SqlType::Decimal {
                    precision: 19,
                    scale: 0,
                },
            ),
            scalar(
                "_value|string",
                ColumnSource::Value,
                BsonKind::String,
                SqlType::Varchar { size: 20 },
            ),
        ];

        let string = Bson::String("hello".into());
        validate_scalar_branches(&string, &columns).unwrap();
        assert_eq!(
            scalar_value(Some(&string), &columns[0]).unwrap(),
            Value::Null
        );
        assert_eq!(
            scalar_value(Some(&string), &columns[1]).unwrap(),
            Value::String("hello".into())
        );

        let integer = Bson::Int32(7);
        validate_scalar_branches(&integer, &columns).unwrap();
        assert_eq!(
            scalar_value(Some(&integer), &columns[0]).unwrap(),
            Value::Numeric(Decimal {
                unscaled: 7,
                scale: 0,
            })
        );
        assert_eq!(
            scalar_value(Some(&integer), &columns[1]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn direct_array_values_route_to_the_advertised_length_branch() {
        let columns = vec![
            scalar(
                "_value",
                ColumnSource::Value,
                BsonKind::Int32,
                SqlType::Decimal {
                    precision: 10,
                    scale: 0,
                },
            ),
            scalar(
                "_value|string",
                ColumnSource::Value,
                BsonKind::String,
                SqlType::Varchar { size: 100 },
            ),
            ColumnSpec {
                source: ColumnSource::ValueArrayLength,
                exasol_name: "_value|array".into(),
                sql_type: SqlType::Decimal {
                    precision: 18,
                    scale: 0,
                },
                bson_kind: None,
            },
        ];
        let array = Bson::Array(vec![Bson::Int32(1), Bson::Int32(2)]);
        validate_scalar_branches(&array, &columns).unwrap();
        assert_eq!(
            column_value(
                &columns[2],
                &MongoReadPlan::RootFind,
                &Bson::Int32(1),
                &array,
                &[]
            )
            .unwrap(),
            Value::Numeric(Decimal {
                unscaled: 2,
                scale: 0
            })
        );
        assert_eq!(
            column_value(
                &columns[2],
                &MongoReadPlan::RootFind,
                &Bson::Int32(1),
                &Bson::String("mixed".into()),
                &[]
            )
            .unwrap(),
            Value::Null
        );
        assert!(validate_scalar_branches(&Bson::Boolean(true), &columns).is_err());
    }

    #[test]
    fn missing_and_explicit_null_are_distinct_with_mask() {
        let column = ColumnSpec {
            source: ColumnSource::NullMask {
                name: "note".into(),
            },
            exasol_name: "note|n".into(),
            sql_type: SqlType::Boolean,
            bson_kind: None,
        };
        let plan = MongoReadPlan::RootFind;
        let root = Bson::Int32(1);
        let missing = Bson::Document(Document::new());
        let explicit = Bson::Document(mongodb::bson::doc! {"note": Bson::Null});
        assert_eq!(
            column_value(&column, &plan, &root, &missing, &[]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            column_value(&column, &plan, &root, &explicit, &[]).unwrap(),
            Value::Bool(true)
        );

        let empty_mask = ColumnSpec {
            source: ColumnSource::EmptyStringMask {
                name: "note".into(),
            },
            exasol_name: "note|empty".into(),
            sql_type: SqlType::Boolean,
            bson_kind: None,
        };
        let empty = Bson::Document(mongodb::bson::doc! {"note": ""});
        assert_eq!(
            column_value(&empty_mask, &plan, &root, &empty, &[]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            column_value(&empty_mask, &plan, &root, &missing, &[]).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn stable_ids_are_repeatable_and_path_and_position_sensitive() {
        let root = Bson::String("same".into());
        let path = vec![PathSegment {
            name: "items".into(),
            kind: PathKind::Array,
            direct: false,
        }];
        let a = stable_id(&root, &path, &[0]).unwrap();
        assert_eq!(a, stable_id(&root, &path, &[0]).unwrap());
        assert_ne!(a, stable_id(&root, &path, &[1]).unwrap());
        assert_ne!(a, stable_id(&root, &[], &[]).unwrap());
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn collects_nested_array_positions_and_rejects_invalid_values() {
        let path = vec![
            PathSegment {
                name: "a".into(),
                kind: PathKind::Array,
                direct: false,
            },
            PathSegment {
                name: "b".into(),
                kind: PathKind::Object,
                direct: false,
            },
            PathSegment {
                name: "c".into(),
                kind: PathKind::Array,
                direct: false,
            },
        ];
        let document = mongodb::bson::doc! { position_field(0): 2_i32, position_field(2): 5_i64 };
        assert_eq!(collect_ordinals(&document, &path).unwrap(), vec![2, 5]);
        assert!(collect_ordinals(&Document::new(), &path).is_err());
        let invalid = mongodb::bson::doc! { position_field(0): -1_i32, position_field(2): 5_i64 };
        assert!(collect_ordinals(&invalid, &path).is_err());
    }

    #[test]
    fn converts_structural_columns_and_detects_structural_drift() {
        let root = Bson::String("root".into());
        let plan = MongoReadPlan::Nested {
            path: vec![PathSegment {
                name: "items".into(),
                kind: PathKind::Array,
                direct: false,
            }],
            table_kind: PathKind::Array,
        };
        let current = Bson::Document(mongodb::bson::doc! {
            "child": {"x": 1}, "values": [1, 2], "bad_object": 1, "bad_array": true
        });
        let varchar = SqlType::Varchar { size: 64 };
        let decimal = SqlType::Decimal {
            precision: 18,
            scale: 0,
        };
        let value = |source, name: &str, sql_type| ColumnSpec {
            source,
            exasol_name: name.into(),
            sql_type,
            bson_kind: None,
        };
        assert!(matches!(
            column_value(
                &value(ColumnSource::RowId, "_id", varchar.clone()),
                &plan,
                &root,
                &current,
                &[3]
            )
            .unwrap(),
            Value::String(_)
        ));
        assert!(matches!(
            column_value(
                &value(ColumnSource::ParentId, "_parent", decimal.clone()),
                &plan,
                &root,
                &current,
                &[3]
            )
            .unwrap(),
            Value::Numeric(_)
        ));
        assert_eq!(
            column_value(
                &value(ColumnSource::Position, "_pos", decimal.clone()),
                &plan,
                &root,
                &current,
                &[3]
            )
            .unwrap(),
            Value::Numeric(Decimal {
                unscaled: 3,
                scale: 0
            })
        );
        assert!(
            column_value(
                &value(ColumnSource::Position, "_pos", decimal.clone()),
                &plan,
                &root,
                &current,
                &[]
            )
            .is_err()
        );
        assert!(matches!(
            column_value(
                &value(
                    ColumnSource::ObjectLink {
                        name: "child".into()
                    },
                    "child",
                    varchar
                ),
                &plan,
                &root,
                &current,
                &[3]
            )
            .unwrap(),
            Value::String(_)
        ));
        assert_eq!(
            column_value(
                &value(
                    ColumnSource::ArrayLength {
                        name: "values".into()
                    },
                    "values",
                    decimal
                ),
                &plan,
                &root,
                &current,
                &[3]
            )
            .unwrap(),
            Value::Numeric(Decimal {
                unscaled: 2,
                scale: 0
            })
        );
        assert!(
            column_value(
                &value(
                    ColumnSource::ObjectLink {
                        name: "bad_object".into()
                    },
                    "bad",
                    SqlType::Varchar { size: 64 }
                ),
                &plan,
                &root,
                &current,
                &[3]
            )
            .is_err()
        );
        assert!(
            column_value(
                &value(
                    ColumnSource::ArrayLength {
                        name: "bad_array".into()
                    },
                    "bad",
                    SqlType::Decimal {
                        precision: 18,
                        scale: 0
                    }
                ),
                &plan,
                &root,
                &current,
                &[3]
            )
            .is_err()
        );
    }

    #[test]
    fn conversion_guardrails_reject_overflow_and_wrong_physical_types() {
        assert!(
            integer_value(
                1000,
                &SqlType::Decimal {
                    precision: 2,
                    scale: 0
                },
                "n"
            )
            .is_err()
        );
        assert!(integer_value(1, &SqlType::Double, "n").is_err());
        assert!(string_value("four".into(), &SqlType::Varchar { size: 3 }, "s").is_err());
        assert!(string_value("x".into(), &SqlType::Boolean, "s").is_err());
        assert!(structural_id_value(&Bson::Int32(1), &[], &[], &SqlType::Boolean, "id").is_err());

        let increment = scalar(
            "inc",
            ColumnSource::Value,
            BsonKind::TimestampIncrement,
            SqlType::Decimal {
                precision: 10,
                scale: 0,
            },
        );
        assert_eq!(
            scalar_value(
                Some(&Bson::Timestamp(Timestamp {
                    time: 1,
                    increment: 7
                })),
                &increment
            )
            .unwrap(),
            Value::Numeric(Decimal {
                unscaled: 7,
                scale: 0
            })
        );
        assert_eq!(scalar_value(None, &increment).unwrap(), Value::Null);
        assert_eq!(
            scalar_value(Some(&Bson::Null), &increment).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn bson_classification_covers_supported_extended_values() {
        assert!(kind_accepts(BsonKind::ExtendedJson, &Bson::Undefined));
        assert!(kind_accepts(BsonKind::ExtendedJson, &Bson::MinKey));
        assert!(kind_accepts(BsonKind::ExtendedJson, &Bson::MaxKey));
        assert!(!kind_accepts(
            BsonKind::Double,
            &Bson::Double(f64::INFINITY)
        ));
        assert!(mongo_error("reading").to_string().contains("reading"));
        for value in [
            Bson::Array(vec![]),
            Bson::Document(Document::new()),
            Bson::Null,
            Bson::RegularExpression(mongodb::bson::Regex {
                pattern: "x".into(),
                options: "".into(),
            }),
            Bson::JavaScriptCode("x".into()),
            Bson::Undefined,
            Bson::MaxKey,
            Bson::MinKey,
        ] {
            assert!(!bson_tag(&value).is_empty());
        }
    }
}
