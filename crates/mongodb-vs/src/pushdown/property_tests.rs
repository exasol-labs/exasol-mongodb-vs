use std::collections::HashMap;

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::model::{BsonKind, ColumnSource, ColumnSpec, SqlType};

fn columns() -> Vec<ColumnSpec> {
    vec![
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
            sql_type: SqlType::Varchar { size: 100 },
            bson_kind: Some(BsonKind::String),
        },
    ]
}

fn exact_integer_filter() -> impl Strategy<Value = FilterExpr> {
    (any::<i64>(), 0u8..6).prop_map(|(value, operation)| FilterExpr::Compare {
        op: match operation {
            0 => CompareOp::Equal,
            1 => CompareOp::NotEqual,
            2 => CompareOp::Less,
            3 => CompareOp::LessEqual,
            4 => CompareOp::Greater,
            _ => CompareOp::GreaterEqual,
        },
        column: "age".into(),
        literal: Literal::ExactNumeric(value.to_string()),
    })
}

fn inexact_string_filter() -> impl Strategy<Value = FilterExpr> {
    vec(any::<char>(), 0..40).prop_map(|characters| FilterExpr::Compare {
        op: CompareOp::Less,
        column: "name".into(),
        literal: Literal::String(characters.into_iter().collect()),
    })
}

fn safe_integer_prefilter() -> impl Strategy<Value = FilterExpr> {
    (any::<i64>(), 0u8..5).prop_map(|(value, operation)| FilterExpr::Compare {
        op: match operation {
            0 => CompareOp::Equal,
            1 => CompareOp::Less,
            2 => CompareOp::LessEqual,
            3 => CompareOp::Greater,
            _ => CompareOp::GreaterEqual,
        },
        column: "age".into(),
        literal: Literal::ExactNumeric(value.to_string()),
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn exact_filter_composition_is_all_or_nothing(
        exact in vec(exact_integer_filter(), 1..12),
        inexact in inexact_string_filter(),
    ) {
        let available_columns = columns();
        let known = available_columns
            .iter()
            .map(|column| (column.exasol_name.as_str(), column))
            .collect::<HashMap<_, _>>();
        let exact_and = FilterExpr::And { expressions: exact.clone() };
        let exact_or = FilterExpr::Or { expressions: exact.clone() };
        let exact_not = FilterExpr::Not { expression: Box::new(exact[0].clone()) };

        prop_assert!(mongo_filter_exact(&exact_and, &known));
        prop_assert!(mongo_filter_exact(&exact_or, &known));
        prop_assert!(mongo_filter_exact(&exact_not, &known));

        let mixed_and = FilterExpr::And {
            expressions: vec![exact[0].clone(), inexact.clone()],
        };
        let mixed_or = FilterExpr::Or {
            expressions: vec![exact[0].clone(), inexact],
        };
        prop_assert!(!mongo_filter_exact(&mixed_and, &known));
        prop_assert!(!mongo_filter_exact(&mixed_or, &known));
    }

    #[test]
    fn prefilter_composition_never_weakens_or_or_not_unsafely(
        exact in safe_integer_prefilter(),
        inexact in inexact_string_filter(),
    ) {
        let available_columns = columns();
        let known = available_columns
            .iter()
            .map(|column| (column.exasol_name.as_str(), column))
            .collect::<HashMap<_, _>>();
        let conjunction = FilterExpr::And {
            expressions: vec![exact.clone(), inexact.clone()],
        };
        let disjunction = FilterExpr::Or {
            expressions: vec![exact.clone(), inexact.clone()],
        };
        let negation = FilterExpr::Not { expression: Box::new(exact.clone()) };

        prop_assert_eq!(mongo_prefilter_candidate(&conjunction, &known), Some(exact));
        prop_assert_eq!(mongo_prefilter_candidate(&disjunction, &known), None);
        prop_assert_eq!(mongo_prefilter_candidate(&negation, &known), None);
    }

    #[test]
    fn quoted_sql_literals_cannot_terminate_their_literal(
        value in vec(any::<char>(), 0..100).prop_map(|chars| chars.into_iter().collect::<String>()),
    ) {
        let quoted = quote_string(&value);
        prop_assert!(quoted.starts_with('\''));
        prop_assert!(quoted.ends_with('\''));
        prop_assert_eq!(&quoted[1..quoted.len() - 1], value.replace('\'', "''"));
    }
}
