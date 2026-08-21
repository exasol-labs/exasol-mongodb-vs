#![no_main]

use libfuzzer_sys::fuzz_target;
use mongodb_vs::{
    model::{BsonKind, ColumnSource, ColumnSpec, SqlType},
    pushdown::{mongo_stages, plan, render_outer_sql},
};

fn columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec {
            source: ColumnSource::RowId,
            exasol_name: "_id".into(),
            sql_type: SqlType::Varchar { size: 64 },
            bson_kind: None,
        },
        ColumnSpec {
            source: ColumnSource::Field { name: "age".into() },
            exasol_name: "age".into(),
            sql_type: SqlType::Decimal {
                precision: 10,
                scale: 0,
            },
            bson_kind: Some(BsonKind::Int32),
        },
        ColumnSpec {
            source: ColumnSource::Field {
                name: "name".into(),
            },
            exasol_name: "name".into(),
            sql_type: SqlType::Varchar { size: 200 },
            bson_kind: Some(BsonKind::String),
        },
        ColumnSpec {
            source: ColumnSource::NullMask {
                name: "name".into(),
            },
            exasol_name: "name|n".into(),
            sql_type: SqlType::Boolean,
            bson_kind: None,
        },
        ColumnSpec {
            source: ColumnSource::EmptyStringMask {
                name: "name".into(),
            },
            exasol_name: "name|empty".into(),
            sql_type: SqlType::Boolean,
            bson_kind: None,
        },
        ColumnSpec {
            source: ColumnSource::Field {
                name: "active".into(),
            },
            exasol_name: "active".into(),
            sql_type: SqlType::Boolean,
            bson_kind: Some(BsonKind::Boolean),
        },
        ColumnSpec {
            source: ColumnSource::Field {
                name: "score".into(),
            },
            exasol_name: "score".into(),
            sql_type: SqlType::Double,
            bson_kind: Some(BsonKind::Double),
        },
        ColumnSpec {
            source: ColumnSource::Field {
                name: "created".into(),
            },
            exasol_name: "created".into(),
            sql_type: SqlType::Timestamp,
            bson_kind: Some(BsonKind::DateTime),
        },
        ColumnSpec {
            source: ColumnSource::Field { name: "_id".into() },
            exasol_name: "mongo_id".into(),
            sql_type: SqlType::Varchar { size: 24 },
            bson_kind: Some(BsonKind::ObjectId),
        },
    ]
}

fuzz_target!(|data: &[u8]| {
    let Ok(request) = serde_json::from_slice(data) else {
        return;
    };
    let columns = columns();
    let Ok(query) = plan(&request, &columns) else {
        return;
    };

    render_outer_sql("SELECT scan", &query, &columns)
        .expect("a successful pushdown plan must render as SQL");
    serde_json::to_vec(&mongo_stages(&query.mongo, &columns))
        .expect("a successful pushdown plan must render as BSON pipeline JSON");
});
