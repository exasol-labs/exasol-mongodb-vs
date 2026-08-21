use exasol_udf_sdk::connect_back::ConnectionObject;
use futures_util::TryStreamExt;
use mongodb::{
    Client,
    bson::{Bson, Document, doc},
};
use mongodb_vs::discovery::{EvidenceStatus, InferenceConfig, infer};
use mongodb_vs::model::{BsonKind, ColumnSource, ColumnSpec, SqlType};
use mongodb_vs::mongo_plan::MongoReadPlan;
use mongodb_vs::pushdown::plan;

fn connection(uri: String) -> ConnectionObject {
    ConnectionObject {
        kind: String::new(),
        address: uri,
        user: String::new(),
        password: String::new(),
    }
}

fn contains_stage(value: &Bson, expected: &str) -> bool {
    match value {
        Bson::Document(document) => {
            document.get_str("stage") == Ok(expected)
                || document
                    .values()
                    .any(|value| contains_stage(value, expected))
        }
        Bson::Array(values) => values.iter().any(|value| contains_stage(value, expected)),
        _ => false,
    }
}

fn assert_indexable_finite_double_match(stage: &Document, operator: &str, expected: f64) {
    let predicate = stage
        .get_document("$match")
        .unwrap()
        .get_document("score")
        .unwrap();
    assert_eq!(predicate.get_str("$type"), Ok("double"));
    assert_eq!(predicate.get_f64(operator), Ok(expected));
    let excluded = predicate.get_array("$nin").unwrap();
    assert!(
        excluded
            .iter()
            .any(|value| value.as_f64().is_some_and(f64::is_nan))
    );
    assert!(excluded.contains(&Bson::Double(f64::INFINITY)));
    assert!(excluded.contains(&Bson::Double(f64::NEG_INFINITY)));
}

#[test]
#[ignore = "started by scripts/run_mongodb_integration.sh"]
fn discovers_validator_indexes_samples_and_permission_gaps() {
    let root_uri = std::env::var("MONGODB_INTEGRATION_ROOT_URI").expect("root URI");
    let limited_uri = std::env::var("MONGODB_INTEGRATION_LIMITED_URI").expect("limited URI");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let config = InferenceConfig::default();
        let first = infer(
            &connection(root_uri.clone()),
            "inference",
            "people",
            &config,
        )
        .await
        .unwrap();
        let second = infer(
            &connection(root_uri.clone()),
            "inference",
            "people",
            &config,
        )
        .await
        .unwrap();
        assert_eq!(first.manifest, second.manifest);
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.report, second.report);
        assert_eq!(first.report.metadata_status, EvidenceStatus::Available);
        assert_eq!(first.report.index_status, EvidenceStatus::Available);
        assert_eq!(first.report.sample_status, EvidenceStatus::Available);
        assert!(!first.report.complete);
        assert!(first.report.indexes.iter().any(|index| {
            index.name == "email_unique" && index.unique && index.keys[0].path == "email"
        }));
        assert!(first.report.indexes.iter().any(|index| {
            index.name == "name_text"
                && index.keys
                    == [mongodb_vs::discovery::IndexKeyEvidence {
                        path: "name".into(),
                        kind: "text".into(),
                    }]
        }));
        assert!(first.report.indexes.iter().all(|index| {
            index
                .keys
                .iter()
                .all(|key| key.path != "_fts" && key.path != "_ftsx")
        }));
        assert!(first.report.paths.iter().any(|path| {
            path.path == ["account", "id"]
                && path.declared == ["string"]
                && !path.required
                && path.indexed_by == ["account_type_partial"]
        }));
        assert!(first.report.paths.iter().any(|path| {
            path.path == ["active"] && path.declared == ["boolean"] && !path.required
        }));
        assert!(
            !first
                .report
                .paths
                .iter()
                .any(|path| path.path == ["unsafe"])
        );
        assert!(first.report.warnings.iter().any(|warning| {
            warning.contains("partial index 'unsafe_or_partial'")
                && warning.contains("predicate '$or'")
        }));
        assert!(first.manifest.tables.iter().any(|table| {
            table.table_name == "PEOPLE_account"
                && table
                    .columns
                    .iter()
                    .any(|column| column.name == "id" && !column.is_required)
        }));
        assert!(first.report.paths.iter().any(|path| {
            path.path == ["age"]
                && path.declared == ["null", "int32", "int64"]
                && path.observed.contains(&"string".into())
        }));
        assert_eq!(
            first
                .manifest
                .tables
                .iter()
                .map(|table| table.table_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "PEOPLE",
                "PEOPLE_account",
                "PEOPLE_items_arr",
                "PEOPLE_profile",
            ]
        );

        let limited = infer(&connection(limited_uri), "inference", "people", &config)
            .await
            .unwrap();
        assert_eq!(
            limited.report.metadata_status,
            EvidenceStatus::NotAuthorized
        );
        assert_eq!(limited.report.index_status, EvidenceStatus::NotAuthorized);
        assert_eq!(limited.report.sample_status, EvidenceStatus::Available);
        assert!(
            limited
                .report
                .warnings
                .iter()
                .any(|warning| warning.contains("metadata is not authorized"))
        );
        assert!(
            limited
                .manifest
                .tables
                .iter()
                .any(|table| table.table_name == "PEOPLE")
        );

        let deterministic_config = InferenceConfig {
            sample_size: 5,
            ..InferenceConfig::default()
        };
        let expected = infer(
            &connection(root_uri.clone()),
            "inference",
            "deterministic_samples",
            &deterministic_config,
        )
        .await
        .unwrap();
        for _ in 0..20 {
            let actual = infer(
                &connection(root_uri.clone()),
                "inference",
                "deterministic_samples",
                &deterministic_config,
            )
            .await
            .unwrap();
            assert_eq!(actual.manifest, expected.manifest);
            assert_eq!(actual.fingerprint, expected.fingerprint);
            assert_eq!(actual.report, expected.report);
        }

        let client = Client::with_uri_str(&root_uri).await.unwrap();
        let double_columns = vec![ColumnSpec {
            source: ColumnSource::Field {
                name: "score".into(),
            },
            exasol_name: "score|double".into(),
            sql_type: SqlType::Double,
            bson_kind: Some(BsonKind::Double),
        }];
        let request = serde_json::json!({"pushdownRequest": {
            "type": "select",
            "selectList": [{"type": "column", "name": "score|double"}],
            "filter": {
                "type": "predicate_less",
                "left": {"type": "column", "name": "score|double"},
                "right": {"type": "literal_exactnumeric", "value": 150}
            }
        }});
        let query = plan(&request, &double_columns).unwrap();
        let pipeline = MongoReadPlan::RootFind
            .pipeline_with(&query.mongo, &double_columns)
            .unwrap();
        assert_indexable_finite_double_match(pipeline.first().unwrap(), "$lt", 150.0);

        let database = client.database("inference");
        let double_collection = database.collection::<Document>("double_pushdown");
        let pushed_rows = double_collection
            .aggregate(pipeline.clone())
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let mongo_count = double_collection
            .count_documents(doc! {"score": {
                "$type": "double",
                "$lt": 150.0,
                "$nin": [f64::NAN, f64::INFINITY, f64::NEG_INFINITY],
            }})
            .await
            .unwrap();
        assert_eq!(pushed_rows.len() as u64, mongo_count);
        assert_eq!(mongo_count, 150);

        let explain_pipeline = Bson::Array(pipeline.into_iter().map(Bson::Document).collect());
        let explain = database
            .run_command(doc! {
                "explain": {
                    "aggregate": "double_pushdown",
                    "pipeline": explain_pipeline,
                    "cursor": {},
                },
                "verbosity": "executionStats",
            })
            .await
            .unwrap();
        assert!(
            contains_stage(&Bson::Document(explain.clone()), "IXSCAN"),
            "double prefilter did not use its index: {explain:?}"
        );

        client
            .database("inference")
            .collection("deterministic_samples")
            .insert_one(doc! {"_id": -1, "new_branch": true})
            .await
            .unwrap();
        let changed = infer(
            &connection(root_uri),
            "inference",
            "deterministic_samples",
            &deterministic_config,
        )
        .await
        .unwrap();
        assert_ne!(changed.manifest, expected.manifest);
        assert_ne!(changed.fingerprint, expected.fingerprint);
    });
}
