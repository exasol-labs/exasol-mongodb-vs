use exasol_udf_sdk::error::UdfError;
use serde::{Deserialize, Serialize};

use crate::model::ColumnSpec;
use crate::mongo_plan::MongoReadPlan;
use crate::pushdown::MongoPushdown;

pub const SCAN_SPEC_VERSION: u32 = 1;
pub const MAX_SCAN_SPEC_BYTES: usize = 1_800_000;

/// Secret-free wire contract passed from the adapter to the scan UDF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MongoScanSpec {
    pub version: u32,
    pub connection_name: String,
    pub database: String,
    pub collection: String,
    pub plan: MongoReadPlan,
    pub columns: Vec<ColumnSpec>,
    pub batch_size: u32,
    #[serde(default)]
    pub pushdown: MongoPushdown,
    #[serde(default)]
    pub inference_fingerprint: String,
}

impl MongoScanSpec {
    pub fn to_json(&self) -> Result<String, UdfError> {
        if self.version != SCAN_SPEC_VERSION {
            return Err(UdfError::User(format!(
                "cannot serialize unsupported MongoScanSpec version {}",
                self.version
            )));
        }
        let value = serde_json::to_string(self).map_err(|error| {
            UdfError::User(format!("failed to serialize MongoScanSpec: {error}"))
        })?;
        if value.len() > MAX_SCAN_SPEC_BYTES {
            return Err(UdfError::User(format!(
                "MongoScanSpec is {} bytes; maximum is {MAX_SCAN_SPEC_BYTES}",
                value.len()
            )));
        }
        Ok(value)
    }

    /// Parse without echoing the input, which can contain sensitive field names.
    pub fn from_json(input: &str) -> Result<Self, UdfError> {
        if input.len() > MAX_SCAN_SPEC_BYTES {
            return Err(UdfError::User(format!(
                "MongoScanSpec is {} bytes; maximum is {MAX_SCAN_SPEC_BYTES}",
                input.len()
            )));
        }
        let spec: Self = serde_json::from_str(input).map_err(|error| {
            UdfError::User(format!(
                "MongoScanSpec is invalid ({:?} at line {}, column {})",
                error.classify(),
                error.line(),
                error.column()
            ))
        })?;
        if spec.version != SCAN_SPEC_VERSION {
            return Err(UdfError::User(format!(
                "unsupported MongoScanSpec version {}; refresh the Virtual Schema",
                spec.version
            )));
        }
        if spec.columns.is_empty() {
            return Err(UdfError::User("MongoScanSpec has no columns".into()));
        }
        if !spec.inference_fingerprint.is_empty()
            && (spec.inference_fingerprint.len() != 64
                || !spec
                    .inference_fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
        {
            return Err(UdfError::User(
                "MongoScanSpec has an invalid inference fingerprint".into(),
            ));
        }
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use crate::model::{BsonKind, ColumnSource, SqlType};

    use super::*;

    fn spec() -> MongoScanSpec {
        MongoScanSpec {
            version: SCAN_SPEC_VERSION,
            connection_name: "MONGO_CONN".into(),
            database: "demo".into(),
            collection: "people".into(),
            plan: MongoReadPlan::RootFind,
            columns: vec![ColumnSpec {
                source: ColumnSource::Field { name: "_id".into() },
                exasol_name: "mongo_id".into(),
                sql_type: SqlType::Varchar { size: 24 },
                bson_kind: Some(BsonKind::ObjectId),
            }],
            batch_size: 128,
            pushdown: MongoPushdown::default(),
            inference_fingerprint: String::new(),
        }
    }

    #[test]
    fn scan_spec_round_trips_without_credentials() {
        let spec = spec();
        let json = spec.to_json().unwrap();
        assert_eq!(MongoScanSpec::from_json(&json).unwrap(), spec);
        assert!(!json.contains("password"));
    }

    #[test]
    fn rejects_unknown_version() {
        let mut invalid = spec();
        invalid.version = 99;
        assert!(
            invalid
                .to_json()
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );

        let mut json: serde_json::Value = serde_json::from_str(&spec().to_json().unwrap()).unwrap();
        json["version"] = serde_json::json!(99);
        assert!(
            MongoScanSpec::from_json(&json.to_string())
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
    }

    #[test]
    fn malformed_spec_error_does_not_echo_input() {
        let secret_marker = "do-not-echo-this";
        let error = MongoScanSpec::from_json(&format!("{{{secret_marker}"))
            .unwrap_err()
            .to_string();
        assert!(!error.contains(secret_marker));
    }

    #[test]
    fn rejects_oversized_wire_input_before_parsing_it() {
        let input = "x".repeat(MAX_SCAN_SPEC_BYTES + 1);
        let error = MongoScanSpec::from_json(&input).unwrap_err().to_string();
        assert!(error.contains("maximum"));
        assert!(error.contains(&(MAX_SCAN_SPEC_BYTES + 1).to_string()));
    }

    #[test]
    fn rejects_empty_columns_and_oversized_output() {
        let mut empty = spec();
        empty.columns.clear();
        let json = serde_json::to_string(&empty).unwrap();
        assert!(
            MongoScanSpec::from_json(&json)
                .unwrap_err()
                .to_string()
                .contains("no columns")
        );

        let mut oversized = spec();
        oversized.database = "x".repeat(MAX_SCAN_SPEC_BYTES);
        assert!(
            oversized
                .to_json()
                .unwrap_err()
                .to_string()
                .contains("maximum")
        );

        let mut invalid_fingerprint = spec();
        invalid_fingerprint.inference_fingerprint = "not-a-fingerprint".into();
        let json = serde_json::to_string(&invalid_fingerprint).unwrap();
        assert!(MongoScanSpec::from_json(&json).is_err());
    }
}
