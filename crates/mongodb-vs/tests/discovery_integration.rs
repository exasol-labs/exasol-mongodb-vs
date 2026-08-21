use exasol_udf_sdk::connect_back::ConnectionObject;
use mongodb::{Client, bson::doc};
use mongodb_vs::discovery::{EvidenceStatus, InferenceConfig, infer};

fn connection(uri: String) -> ConnectionObject {
    ConnectionObject {
        kind: String::new(),
        address: uri,
        user: String::new(),
        password: String::new(),
    }
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
            vec!["PEOPLE", "PEOPLE_items_arr", "PEOPLE_profile"]
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
