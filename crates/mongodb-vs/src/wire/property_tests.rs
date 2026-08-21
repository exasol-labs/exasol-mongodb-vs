use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::model::{BsonKind, ColumnSource, ColumnSpec, SqlType};

fn short_string() -> impl Strategy<Value = String> {
    vec(any::<char>(), 0..48).prop_map(|characters| characters.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn valid_scan_specs_round_trip_for_arbitrary_names(
        connection_name in short_string(),
        database in short_string(),
        collection in short_string(),
        column_name in short_string().prop_filter("column name must not be empty", |name| !name.is_empty()),
        source_name in short_string(),
        batch_size in any::<u32>(),
        fingerprint in prop::option::of("[0-9a-f]{64}"),
    ) {
        let value = MongoScanSpec {
            version: SCAN_SPEC_VERSION,
            connection_name,
            database,
            collection,
            plan: MongoReadPlan::RootFind,
            columns: vec![ColumnSpec {
                source: ColumnSource::Field { name: source_name },
                exasol_name: column_name,
                sql_type: SqlType::Varchar { size: 128 },
                bson_kind: Some(BsonKind::String),
            }],
            batch_size,
            pushdown: MongoPushdown::default(),
            inference_fingerprint: fingerprint.unwrap_or_default(),
        };

        let json = value.to_json().unwrap();
        prop_assert_eq!(MongoScanSpec::from_json(&json).unwrap(), value);
    }

    #[test]
    fn malformed_wire_errors_never_echo_arbitrary_input(
        payload in "[A-Za-z0-9]{1,64}",
    ) {
        let marker = format!("sensitive_marker_{payload}_must_not_leak");
        let input = format!("{{invalid-{marker}");
        let error = MongoScanSpec::from_json(&input).unwrap_err().to_string();
        prop_assert!(!error.contains(&marker));
    }
}
