use mongodb::bson::{Bson, Document, doc};
use serde::{Deserialize, Serialize};

use crate::model::{ColumnSpec, PathKind, PathSegment, TableModel};
use crate::pushdown::{MongoPushdown, mongo_path_prefilter, mongo_stages};

pub const ROOT_ID_FIELD: &str = "__jt_root_id";
pub const VALUE_FIELD: &str = "__jt_value";
pub const POSITION_PREFIX: &str = "__jt_pos_";

/// A deliberately small read IR. It is safe to serialize across the UDF wire
/// and is compiled to driver documents only inside the scan layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MongoReadPlan {
    RootFind,
    Nested {
        path: Vec<PathSegment>,
        table_kind: PathKind,
    },
}

impl MongoReadPlan {
    pub fn for_table(table: &TableModel) -> Self {
        if table.path.is_empty() {
            Self::RootFind
        } else {
            Self::Nested {
                path: table.path.clone(),
                table_kind: table.kind,
            }
        }
    }

    pub fn pipeline(&self) -> Option<Vec<Document>> {
        self.pipeline_with(&MongoPushdown::default(), &[])
    }

    pub fn pipeline_with(
        &self,
        pushdown: &MongoPushdown,
        columns: &[ColumnSpec],
    ) -> Option<Vec<Document>> {
        let path = self.path();
        let prefilter = mongo_path_prefilter(pushdown, columns, path);
        if matches!(self, Self::RootFind)
            && prefilter.is_none()
            && pushdown.filter.is_none()
            && pushdown.order_by.is_empty()
            && pushdown.limit.is_none()
            && pushdown.aggregation.is_none()
        {
            return None;
        }
        let mut pipeline = Vec::new();
        if let Some(prefilter) = prefilter {
            pipeline.push(doc! {"$match": prefilter});
        }
        pipeline.push(doc! {
            "$project": {
                ROOT_ID_FIELD: "$_id",
                VALUE_FIELD: "$$ROOT",
            }
        });
        let (path, table_kind) = match self {
            Self::RootFind => (&[][..], PathKind::Object),
            Self::Nested { path, table_kind } => (path.as_slice(), *table_kind),
        };
        for (index, segment) in path.iter().enumerate() {
            if !segment.direct {
                let guarded_get = doc! {
                    "$cond": [
                        {"$eq": [{"$type": format!("${VALUE_FIELD}")}, "object"]},
                        {"$getField": {
                            "field": {"$literal": segment.name.clone()},
                            "input": format!("${VALUE_FIELD}"),
                        }},
                        "$$REMOVE",
                    ]
                };
                pipeline.push(doc! {"$set": {VALUE_FIELD: guarded_get}});
            }
            if segment.kind == PathKind::Array {
                // A polymorphic path may contain an advertised scalar sibling.
                // MongoDB's $unwind otherwise emits that scalar as one row with
                // a null array index, which is neither an element nor a valid
                // json-tables position.
                pipeline.push(doc! {
                    "$match": {
                        "$expr": {"$eq": [{"$type": format!("${VALUE_FIELD}")}, "array"]}
                    }
                });
                pipeline.push(doc! {
                    "$unwind": {
                        "path": format!("${VALUE_FIELD}"),
                        "includeArrayIndex": format!("{POSITION_PREFIX}{index}"),
                        "preserveNullAndEmptyArrays": false,
                    }
                });
            }
        }
        if table_kind == PathKind::Object && !path.is_empty() {
            pipeline.push(doc! {
                "$match": {
                    "$expr": {"$eq": [{"$type": format!("${VALUE_FIELD}")}, "object"]}
                }
            });
        }
        pipeline.extend(mongo_stages(pushdown, columns));
        Some(pipeline)
    }

    pub fn path(&self) -> &[PathSegment] {
        match self {
            Self::RootFind => &[],
            Self::Nested { path, .. } => path,
        }
    }

    pub fn array_depth(&self) -> usize {
        self.path()
            .iter()
            .filter(|segment| segment.kind == PathKind::Array)
            .count()
    }
}

pub fn position_field(path_index: usize) -> String {
    format!("{POSITION_PREFIX}{path_index}")
}

pub fn projected_value(document: &Document) -> Option<&Bson> {
    document.get(VALUE_FIELD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BsonKind, ColumnSource, SqlType};
    use crate::pushdown::{
        AGGREGATE_COUNT_FIELD, CompareOp, FilterExpr, Literal, MongoAggregation,
    };

    #[test]
    fn count_pipeline_runs_after_root_or_nested_row_expansion() {
        let pushdown = MongoPushdown {
            aggregation: Some(MongoAggregation::CountStar),
            ..MongoPushdown::default()
        };
        let root = MongoReadPlan::RootFind
            .pipeline_with(&pushdown, &[])
            .unwrap();
        assert_eq!(root.last(), Some(&doc! {"$count": AGGREGATE_COUNT_FIELD}));

        let nested = MongoReadPlan::Nested {
            path: vec![PathSegment {
                name: "items".into(),
                kind: PathKind::Array,
                direct: false,
            }],
            table_kind: PathKind::Array,
        }
        .pipeline_with(&pushdown, &[])
        .unwrap();
        assert!(nested.iter().any(|stage| stage.contains_key("$unwind")));
        assert_eq!(nested.last(), Some(&doc! {"$count": AGGREGATE_COUNT_FIELD}));
    }

    #[test]
    fn nested_plan_uses_get_field_and_one_unwind_per_array_segment() {
        let plan = MongoReadPlan::Nested {
            path: vec![
                PathSegment {
                    name: "a.b$".into(),
                    kind: PathKind::Object,
                    direct: false,
                },
                PathSegment {
                    name: "items[]".into(),
                    kind: PathKind::Array,
                    direct: false,
                },
            ],
            table_kind: PathKind::Array,
        };
        let pipeline = plan.pipeline().unwrap();
        let json = serde_json::to_string(&pipeline).unwrap();
        assert!(json.contains("$getField"));
        assert!(json.contains("a.b$"));
        assert_eq!(
            pipeline
                .iter()
                .filter(|stage| stage.contains_key("$unwind"))
                .count(),
            1
        );
        assert_eq!(plan.array_depth(), 1);
    }

    #[test]
    fn nested_filter_uses_dotted_match_before_traversal_and_typed_match_afterwards() {
        let plan = MongoReadPlan::Nested {
            path: vec![PathSegment {
                name: "items".into(),
                kind: PathKind::Array,
                direct: false,
            }],
            table_kind: PathKind::Array,
        };
        let columns = [ColumnSpec {
            source: ColumnSource::Field {
                name: "quantity".into(),
            },
            exasol_name: "quantity".into(),
            sql_type: SqlType::Decimal {
                precision: 10,
                scale: 0,
            },
            bson_kind: Some(BsonKind::Int32),
        }];
        let pushdown = MongoPushdown {
            filter: Some(FilterExpr::Compare {
                op: CompareOp::Greater,
                column: "quantity".into(),
                literal: Literal::ExactNumeric("2".into()),
            }),
            ..MongoPushdown::default()
        };

        let pipeline = plan.pipeline_with(&pushdown, &columns).unwrap();
        assert_eq!(
            pipeline.first(),
            Some(&doc! {
                "$match": {
                    "items.quantity": {"$type": "int", "$gt": Bson::Int64(2)}
                }
            })
        );
        let unwind = pipeline
            .iter()
            .position(|stage| stage.contains_key("$unwind"))
            .unwrap();
        assert!(pipeline[unwind + 1].contains_key("$match"));
        assert!(
            serde_json::to_string(&pipeline[unwind + 1])
                .unwrap()
                .contains("$getField")
        );
    }

    #[test]
    fn nested_string_equality_only_prefilters_before_traversal() {
        let plan = MongoReadPlan::Nested {
            path: vec![PathSegment {
                name: "items".into(),
                kind: PathKind::Array,
                direct: false,
            }],
            table_kind: PathKind::Array,
        };
        let columns = [ColumnSpec {
            source: ColumnSource::Field { name: "sku".into() },
            exasol_name: "sku".into(),
            sql_type: SqlType::Varchar { size: 100 },
            bson_kind: Some(BsonKind::String),
        }];
        let pushdown = MongoPushdown {
            prefilter: Some(FilterExpr::Compare {
                op: CompareOp::Equal,
                column: "sku".into(),
                literal: Literal::String("SKU[1] ".into()),
            }),
            ..MongoPushdown::default()
        };

        let pipeline = plan.pipeline_with(&pushdown, &columns).unwrap();
        let first = serde_json::to_string(&pipeline[0]).unwrap();
        assert!(first.contains("items.sku"));
        assert!(first.contains(r"^SKU\\[1\\] *$"));
        let unwind = pipeline
            .iter()
            .position(|stage| stage.contains_key("$unwind"))
            .unwrap();
        assert!(
            pipeline[unwind + 1..]
                .iter()
                .all(|stage| !stage.contains_key("$match"))
        );

        let unsupported_range = MongoPushdown {
            prefilter: Some(FilterExpr::Compare {
                op: CompareOp::Less,
                column: "sku".into(),
                literal: Literal::String("Z".into()),
            }),
            ..MongoPushdown::default()
        };
        assert!(
            MongoReadPlan::RootFind
                .pipeline_with(&unsupported_range, &columns)
                .is_none()
        );
    }

    #[test]
    fn root_and_object_plans_have_expected_shape() {
        let root = MongoReadPlan::RootFind;
        assert!(root.pipeline().is_none());
        assert!(root.path().is_empty());
        assert_eq!(root.array_depth(), 0);
        assert_eq!(position_field(3), "__jt_pos_3");

        let table = TableModel {
            table_name: "CHILD".into(),
            kind: PathKind::Object,
            path: vec![PathSegment {
                name: "child".into(),
                kind: PathKind::Object,
                direct: false,
            }],
            columns: vec![],
        };
        let plan = MongoReadPlan::for_table(&table);
        let pipeline = plan.pipeline().unwrap();
        assert!(pipeline.last().unwrap().contains_key("$match"));
        let projected = doc! { VALUE_FIELD: 42 };
        assert_eq!(projected_value(&projected), Some(&Bson::Int32(42)));
    }

    #[test]
    fn direct_nested_arrays_unwind_without_inventing_a_field() {
        let plan = MongoReadPlan::Nested {
            path: vec![
                PathSegment {
                    name: "matrix".into(),
                    kind: PathKind::Array,
                    direct: false,
                },
                PathSegment {
                    name: String::new(),
                    kind: PathKind::Array,
                    direct: true,
                },
            ],
            table_kind: PathKind::Array,
        };
        let pipeline = plan.pipeline().unwrap();
        let json = serde_json::to_string(&pipeline).unwrap();
        assert_eq!(json.matches("$getField").count(), 1);
        assert_eq!(json.matches("$unwind").count(), 2);
        assert_eq!(json.matches("\"array\"").count(), 2);
        for (index, stage) in pipeline.iter().enumerate() {
            if stage.contains_key("$unwind") {
                assert!(index > 0 && pipeline[index - 1].contains_key("$match"));
            }
        }
        assert_eq!(plan.array_depth(), 2);
    }
}
