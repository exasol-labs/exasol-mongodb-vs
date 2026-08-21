use std::collections::{HashMap, HashSet};

use exasol_udf_sdk::error::UdfError;
use mongodb::bson::{Bson, DateTime, Document, doc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::model::{BsonKind, ColumnSource, ColumnSpec, PathSegment};
use crate::mongo_plan::VALUE_FIELD;

pub const AGGREGATE_COUNT_FIELD: &str = "__jt_count";

pub const CAPABILITIES: &[&str] = &[
    "AGGREGATE_SINGLE_GROUP",
    "SELECTLIST_PROJECTION",
    "SELECTLIST_EXPRESSIONS",
    "FILTER_EXPRESSIONS",
    "LITERAL_BOOL",
    "LITERAL_DOUBLE",
    "LITERAL_EXACTNUMERIC",
    "LITERAL_STRING",
    "LITERAL_TIMESTAMP",
    "FN_PRED_AND",
    "FN_PRED_OR",
    "FN_PRED_NOT",
    "FN_PRED_BETWEEN",
    "FN_PRED_EQUAL",
    "FN_PRED_NOTEQUAL",
    "FN_PRED_LESS",
    "FN_PRED_LESSEQUAL",
    "FN_PRED_IN_CONSTLIST",
    "FN_PRED_IS_NULL",
    "FN_PRED_IS_NOT_NULL",
    "FN_AGG_COUNT",
    "FN_AGG_COUNT_STAR",
    "LIMIT",
    "ORDER_BY_COLUMN",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MongoPushdown {
    /// Conservative root-document filter rendered with native dotted paths.
    /// Unlike `filter`, this need not be semantically complete because Exasol
    /// still evaluates the original predicate over every retained row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefilter: Option<FilterExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterExpr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order_by: Vec<SortKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregation: Option<MongoAggregation>,
}

impl MongoPushdown {
    pub fn is_empty(&self) -> bool {
        self.prefilter.is_none()
            && self.filter.is_none()
            && self.order_by.is_empty()
            && self.limit.is_none()
            && self.aggregation.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MongoAggregation {
    CountStar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FilterExpr {
    And {
        expressions: Vec<FilterExpr>,
    },
    Or {
        expressions: Vec<FilterExpr>,
    },
    Not {
        expression: Box<FilterExpr>,
    },
    Compare {
        op: CompareOp,
        column: String,
        literal: Literal,
    },
    In {
        column: String,
        literals: Vec<Literal>,
    },
    IsNull {
        column: String,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Literal {
    Boolean(bool),
    Double(String),
    ExactNumeric(String),
    String(String),
    Timestamp(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    pub column: String,
    pub ascending: bool,
    pub nulls_last: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    pub selected: Vec<String>,
    pub required: Vec<String>,
    pub filter: Option<FilterExpr>,
    pub order_by: Vec<SortKey>,
    pub limit: Option<u64>,
    pub aggregation: Option<SingleGroupAggregation>,
    pub mongo: MongoPushdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleGroupAggregation {
    pub expressions: Vec<AggregateExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateExpr {
    CountStar,
    CountColumn { column: String },
    Constant { literal: Literal },
}

pub fn plan(request: &Json, columns: &[ColumnSpec]) -> Result<QueryPlan, UdfError> {
    let pushdown = request
        .get("pushdownRequest")
        .ok_or_else(|| UdfError::User("pushdown request has no query body".into()))?;
    if pushdown.get("type").and_then(Json::as_str) != Some("select") {
        return Err(UdfError::User("only SELECT pushdown is supported".into()));
    }
    let known = columns
        .iter()
        .map(|column| (column.exasol_name.as_str(), column))
        .collect::<HashMap<_, _>>();
    if pushdown.get("aggregationType").is_some() {
        return plan_single_group(pushdown, columns, &known);
    }
    let output_types = pushdown
        .get("selectListDataTypes")
        .and_then(Json::as_array)
        .map_or(0, Vec::len);
    let residual_count_carrier = output_types == 0
        && pushdown
            .get("selectList")
            .and_then(Json::as_array)
            .is_none_or(Vec::is_empty);
    let selected = match pushdown.get("selectList") {
        None if residual_count_carrier => involved_columns(request)
            .and_then(|columns| columns.into_iter().next().map(|column| vec![column]))
            .unwrap_or_else(|| vec![columns[0].exasol_name.clone()]),
        None => involved_columns(request).unwrap_or_else(|| {
            columns
                .iter()
                .map(|column| column.exasol_name.clone())
                .collect()
        }),
        Some(Json::Array(items)) if items.is_empty() && residual_count_carrier => {
            involved_columns(request)
                .and_then(|columns| columns.into_iter().next().map(|column| vec![column]))
                .unwrap_or_else(|| vec![columns[0].exasol_name.clone()])
        }
        Some(Json::Array(items)) if items.is_empty() => {
            involved_columns(request).unwrap_or_else(|| {
                columns
                    .iter()
                    .map(|column| column.exasol_name.clone())
                    .collect()
            })
        }
        Some(Json::Array(items)) if items.iter().all(is_column) => items
            .iter()
            .map(column_name)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| UdfError::User("projection contains an invalid column".into()))?,
        Some(_) => {
            return Err(UdfError::User(
                "projection contains an expression that was not advertised".into(),
            ));
        }
    };
    validate_columns(&selected, &known)?;

    let filter = pushdown.get("filter").map(parse_filter).transpose()?;
    let order_by = parse_order_by(pushdown)?;
    validate_columns(
        &order_by
            .iter()
            .map(|key| key.column.clone())
            .collect::<Vec<_>>(),
        &known,
    )?;
    if let Some(filter) = &filter {
        validate_columns(&referenced_columns(filter), &known)?;
    }
    let limit = parse_limit(pushdown)?;

    let mut required = selected.clone();
    if let Some(filter) = &filter {
        required.extend(referenced_columns(filter));
    }
    required.extend(order_by.iter().map(|key| key.column.clone()));
    expand_branch_siblings(&mut required, columns, &known);

    let exact_filter = filter
        .as_ref()
        .filter(|expression| mongo_filter_exact(expression, &known))
        .cloned();
    let prefilter = filter
        .as_ref()
        .and_then(|expression| mongo_prefilter_candidate(expression, &known));
    let exact_order =
        if !order_by.is_empty() && order_by.iter().all(|key| mongo_sort_exact(key, &known)) {
            order_by.clone()
        } else {
            Vec::new()
        };
    let filter_fully_pushed = filter.is_none() || exact_filter.is_some();
    let order_fully_pushed = order_by.is_empty() || !exact_order.is_empty();
    let mongo_limit = limit.filter(|_| filter_fully_pushed && order_fully_pushed);

    Ok(QueryPlan {
        selected,
        required,
        filter: filter.clone(),
        order_by,
        limit,
        aggregation: None,
        mongo: MongoPushdown {
            prefilter,
            filter: exact_filter,
            order_by: exact_order,
            limit: mongo_limit,
            aggregation: None,
        },
    })
}

fn plan_single_group(
    pushdown: &Json,
    columns: &[ColumnSpec],
    known: &HashMap<&str, &ColumnSpec>,
) -> Result<QueryPlan, UdfError> {
    if pushdown.get("aggregationType").and_then(Json::as_str) != Some("single_group") {
        return Err(UdfError::User(
            "only single-group aggregation is advertised".into(),
        ));
    }
    if pushdown.get("groupBy").is_some() || pushdown.get("having").is_some() {
        return Err(UdfError::User(
            "GROUP BY and HAVING are not advertised".into(),
        ));
    }
    if pushdown.get("orderBy").is_some() {
        return Err(UdfError::User(
            "ORDER BY on aggregate expressions is not advertised".into(),
        ));
    }
    let items = pushdown
        .get("selectList")
        .and_then(Json::as_array)
        .ok_or_else(|| UdfError::User("aggregate query has no select list".into()))?;
    if items.is_empty() {
        return Err(UdfError::User("aggregate select list is empty".into()));
    }
    let expressions = items
        .iter()
        .map(parse_aggregate_projection)
        .collect::<Result<Vec<_>, _>>()?;
    if !expressions.iter().any(|expression| {
        matches!(
            expression,
            AggregateExpr::CountStar | AggregateExpr::CountColumn { .. }
        )
    }) {
        return Err(UdfError::User(
            "aggregate select list has no aggregate function".into(),
        ));
    }
    if let Some(output_types) = pushdown.get("selectListDataTypes").and_then(Json::as_array)
        && output_types.len() != expressions.len()
    {
        return Err(UdfError::User(
            "aggregate result type count does not match its select list".into(),
        ));
    }

    let filter = pushdown.get("filter").map(parse_filter).transpose()?;
    if let Some(filter) = &filter {
        validate_columns(&referenced_columns(filter), known)?;
    }
    let limit = parse_limit(pushdown)?;
    let mut required = expressions
        .iter()
        .filter_map(|expression| match expression {
            AggregateExpr::CountColumn { column } => Some(column.clone()),
            AggregateExpr::CountStar | AggregateExpr::Constant { .. } => None,
        })
        .collect::<Vec<_>>();
    if let Some(filter) = &filter {
        required.extend(referenced_columns(filter));
    }
    validate_columns(&required, known)?;
    if required.is_empty() {
        required.push(count_carrier(columns).exasol_name.clone());
    }
    expand_branch_siblings(&mut required, columns, known);

    let exact_filter = filter
        .as_ref()
        .filter(|expression| mongo_filter_exact(expression, known))
        .cloned();
    let prefilter = filter
        .as_ref()
        .and_then(|expression| mongo_prefilter_candidate(expression, known));
    let filter_fully_pushed = filter.is_none() || exact_filter.is_some();
    let count_star_exact = expressions.iter().all(|expression| {
        matches!(
            expression,
            AggregateExpr::CountStar | AggregateExpr::Constant { .. }
        )
    });
    let mongo_aggregation =
        (count_star_exact && filter_fully_pushed).then_some(MongoAggregation::CountStar);

    Ok(QueryPlan {
        selected: Vec::new(),
        required,
        filter: filter.clone(),
        order_by: Vec::new(),
        limit,
        aggregation: Some(SingleGroupAggregation { expressions }),
        mongo: MongoPushdown {
            prefilter,
            filter: exact_filter,
            order_by: Vec::new(),
            limit: None,
            aggregation: mongo_aggregation,
        },
    })
}

fn parse_aggregate_projection(node: &Json) -> Result<AggregateExpr, UdfError> {
    if node
        .get("type")
        .and_then(Json::as_str)
        .is_some_and(|kind| kind.starts_with("literal_"))
    {
        return parse_literal(node).map(|literal| AggregateExpr::Constant { literal });
    }
    if node.get("type").and_then(Json::as_str) != Some("function_aggregate")
        || !node
            .get("name")
            .and_then(Json::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("count"))
    {
        return Err(UdfError::User(
            "aggregate select list contains a function that was not advertised".into(),
        ));
    }
    if node.get("distinct").and_then(Json::as_bool) == Some(true) {
        return Err(UdfError::User("COUNT DISTINCT is not advertised".into()));
    }
    let arguments = node.get("arguments").and_then(Json::as_array);
    match arguments {
        None => Ok(AggregateExpr::CountStar),
        Some(arguments) if arguments.is_empty() => Ok(AggregateExpr::CountStar),
        Some(arguments) if arguments.len() == 1 => arguments[0]
            .get("name")
            .and_then(Json::as_str)
            .filter(|_| is_column(&arguments[0]))
            .map(|column| AggregateExpr::CountColumn {
                column: column.into(),
            })
            .ok_or_else(|| UdfError::User("COUNT requires a physical column".into())),
        Some(_) => Err(UdfError::User("COUNT tuple is not advertised".into())),
    }
}

fn count_carrier(columns: &[ColumnSpec]) -> &ColumnSpec {
    columns
        .iter()
        .find(|column| {
            matches!(
                column.source,
                ColumnSource::RowId | ColumnSource::ParentId | ColumnSource::Position
            )
        })
        .unwrap_or(&columns[0])
}

fn expand_branch_siblings(
    required: &mut Vec<String>,
    columns: &[ColumnSpec],
    known: &HashMap<&str, &ColumnSpec>,
) {
    let branch_sources = required
        .iter()
        .filter_map(|name| known.get(name.as_str()))
        .filter_map(|column| branch_source(&column.source))
        .collect::<Vec<_>>();
    required.extend(
        columns
            .iter()
            .filter(|column| {
                branch_source(&column.source).is_some_and(|source| branch_sources.contains(&source))
            })
            .map(|column| column.exasol_name.clone()),
    );
    deduplicate(required);
}

pub fn render_outer_sql(
    udf_select: &str,
    query: &QueryPlan,
    all_columns: &[ColumnSpec],
) -> Result<String, UdfError> {
    let known = all_columns
        .iter()
        .map(|column| (column.exasol_name.as_str(), column))
        .collect::<HashMap<_, _>>();
    if let Some(aggregation) = &query.aggregation {
        let remotely_aggregated = query.mongo.aggregation.is_some();
        let projection = aggregation
            .expressions
            .iter()
            .map(|expression| match expression {
                AggregateExpr::Constant { literal } => render_untyped_literal(literal),
                AggregateExpr::CountStar if remotely_aggregated => {
                    Ok(quote_ident(AGGREGATE_COUNT_FIELD))
                }
                AggregateExpr::CountStar => Ok("COUNT(*)".into()),
                AggregateExpr::CountColumn { column } => {
                    if remotely_aggregated {
                        Err(UdfError::User(
                            "remote COUNT plan contains a column count".into(),
                        ))
                    } else {
                        Ok(format!("COUNT({})", quote_ident(column)))
                    }
                }
            })
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let mut sql = format!("SELECT {projection} FROM ({udf_select}) \"MONGO_PUSHDOWN\"");
        if !remotely_aggregated && let Some(filter) = &query.filter {
            sql.push_str(" WHERE ");
            sql.push_str(&render_filter(filter, &known)?);
        }
        if let Some(limit) = query.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        return Ok(sql);
    }
    let projection = query
        .selected
        .iter()
        .map(|name| quote_ident(name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("SELECT {projection} FROM ({udf_select}) \"MONGO_PUSHDOWN\"");
    if let Some(filter) = &query.filter {
        sql.push_str(" WHERE ");
        sql.push_str(&render_filter(filter, &known)?);
    }
    if !query.order_by.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(
            &query
                .order_by
                .iter()
                .map(|key| {
                    format!(
                        "{} {} NULLS {}",
                        quote_ident(&key.column),
                        if key.ascending { "ASC" } else { "DESC" },
                        if key.nulls_last { "LAST" } else { "FIRST" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if let Some(limit) = query.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    Ok(sql)
}

pub fn mongo_stages(pushdown: &MongoPushdown, columns: &[ColumnSpec]) -> Vec<Document> {
    let known = columns
        .iter()
        .map(|column| (column.exasol_name.as_str(), column))
        .collect::<HashMap<_, _>>();
    let mut stages = Vec::new();
    if let Some(filter) = &pushdown.filter
        && let Some(expression) = mongo_expression(filter, &known)
    {
        stages.push(doc! {"$match": {"$expr": expression}});
    }
    if pushdown.aggregation == Some(MongoAggregation::CountStar) {
        stages.push(doc! {"$count": AGGREGATE_COUNT_FIELD});
        return stages;
    }
    if !pushdown.order_by.is_empty() {
        let mut set = Document::new();
        let mut sort = Document::new();
        for (index, key) in pushdown.order_by.iter().enumerate() {
            let Some(column) = known.get(key.column.as_str()) else {
                return Vec::new();
            };
            let value = mongo_column_value(column);
            let null_rank = format!("__jt_null_{index}");
            let sort_value = format!("__jt_sort_{index}");
            set.insert(
                &null_rank,
                doc! {"$cond": [mongo_present(column, value.clone()), if key.nulls_last { 0 } else { 1 }, if key.nulls_last { 1 } else { 0 }]},
            );
            set.insert(&sort_value, value);
            sort.insert(null_rank, 1);
            sort.insert(sort_value, if key.ascending { 1 } else { -1 });
        }
        stages.push(doc! {"$set": set});
        stages.push(doc! {"$sort": sort});
    }
    if let Some(limit) = pushdown.limit
        && let Ok(limit) = i64::try_from(limit)
    {
        stages.push(doc! {"$limit": limit});
    }
    stages
}

/// Build a conservative root-document prefilter using MongoDB's native dotted
/// field paths. The regular typed `$expr` filter is still evaluated after the
/// table path has been traversed and arrays have been unwound. Consequently,
/// this stage only needs to be a necessary condition: it may retain extra root
/// documents, but it must never discard a relational row that the final filter
/// would accept.
///
/// A native path prefilter lets MongoDB use ordinary and multikey indexes for
/// nested fields. Literal field names containing `.` or starting with `$` stay
/// on the `$getField` path because dotted query syntax would change their
/// meaning. Direct nested-array path segments are also declined because MongoDB
/// dot notation does not encode those structural levels unambiguously.
pub(crate) fn mongo_path_prefilter(
    pushdown: &MongoPushdown,
    columns: &[ColumnSpec],
    table_path: &[PathSegment],
) -> Option<Document> {
    // Older serialized plans have no dedicated prefilter. Falling back to the
    // exact filter preserves their dotted-path optimization.
    let filter = pushdown.prefilter.as_ref().or(pushdown.filter.as_ref())?;
    let known = columns
        .iter()
        .map(|column| (column.exasol_name.as_str(), column))
        .collect::<HashMap<_, _>>();
    path_prefilter_expression(filter, &known, table_path)
}

fn path_prefilter_expression(
    expr: &FilterExpr,
    known: &HashMap<&str, &ColumnSpec>,
    table_path: &[PathSegment],
) -> Option<Document> {
    match expr {
        // Every retained conjunct is a necessary condition. Unsupported
        // conjuncts can therefore be omitted without introducing false
        // negatives.
        FilterExpr::And { expressions } => combine_path_prefilters(
            "$and",
            expressions
                .iter()
                .filter_map(|expr| path_prefilter_expression(expr, known, table_path))
                .collect(),
        ),
        // Every OR branch must have a necessary condition. Dropping one branch
        // would incorrectly discard documents that satisfy only that branch.
        FilterExpr::Or { expressions } => {
            let compiled = expressions
                .iter()
                .map(|expr| path_prefilter_expression(expr, known, table_path))
                .collect::<Option<Vec<_>>>()?;
            combine_path_prefilters("$or", compiled)
        }
        // Negation over an array path is not a safe root prefilter: another
        // element can satisfy the positive expression while the emitted row
        // satisfies its negation. The post-unwind typed filter handles it.
        FilterExpr::Not { .. } => None,
        FilterExpr::Compare {
            op,
            column,
            literal,
        } => {
            if *op == CompareOp::NotEqual {
                return None;
            }
            let spec = known.get(column.as_str())?;
            let path = mongo_dotted_path(table_path, spec)?;
            if spec.bson_kind == Some(BsonKind::String) {
                if *op != CompareOp::Equal {
                    return None;
                }
                let Literal::String(literal) = literal else {
                    return None;
                };
                return typed_path_predicate(path, spec, "$eq", Bson::String(literal.clone()));
            }
            if !comparison_exact(spec, *op) {
                return None;
            }
            let literal = literal_bson(spec, literal)?;
            let operator = match op {
                CompareOp::Equal => "$eq",
                CompareOp::Less => "$lt",
                CompareOp::LessEqual => "$lte",
                CompareOp::Greater => "$gt",
                CompareOp::GreaterEqual => "$gte",
                CompareOp::NotEqual => unreachable!(),
            };
            typed_path_predicate(path, spec, operator, literal)
        }
        FilterExpr::In { column, literals } => {
            let spec = known.get(column.as_str())?;
            let path = mongo_dotted_path(table_path, spec)?;
            if spec.bson_kind == Some(BsonKind::String) {
                let literals = literals
                    .iter()
                    .map(|literal| match literal {
                        Literal::String(value) => Some(Bson::String(value.clone())),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                return Some(doc! {
                    path: {"$type": "string", "$in": Bson::Array(literals)}
                });
            }
            if !comparison_exact(spec, CompareOp::Equal) {
                return None;
            }
            let literals = literals
                .iter()
                .map(|literal| literal_bson(spec, literal))
                .collect::<Option<Vec<_>>>()?;
            typed_path_predicate(path, spec, "$in", Bson::Array(literals))
        }
        FilterExpr::IsNull {
            column,
            negated: true,
        } => {
            let spec = known.get(column.as_str())?;
            let path = mongo_dotted_path(table_path, spec)?;
            let types = scalar_type_names(spec)?;
            Some(doc! {path: {"$type": type_selector(types)}})
        }
        FilterExpr::IsNull { .. } => None,
    }
}

fn combine_path_prefilters(operator: &str, expressions: Vec<Document>) -> Option<Document> {
    match expressions.len() {
        0 => None,
        1 => expressions.into_iter().next(),
        _ => Some(doc! {operator: expressions}),
    }
}

fn mongo_dotted_path(table_path: &[PathSegment], spec: &ColumnSpec) -> Option<String> {
    let mut segments = table_path
        .iter()
        .map(|segment| {
            if segment.direct || !mongo_path_segment_safe(&segment.name) {
                None
            } else {
                Some(segment.name.as_str())
            }
        })
        .collect::<Option<Vec<_>>>()?;
    match &spec.source {
        ColumnSource::Field { name }
        | ColumnSource::NullMask { name }
        | ColumnSource::EmptyStringMask { name } => {
            if !mongo_path_segment_safe(name) {
                return None;
            }
            segments.push(name);
        }
        ColumnSource::Value
        | ColumnSource::ValueNullMask
        | ColumnSource::ValueEmptyStringMask
        | ColumnSource::ValueObjectMarker => {}
        _ => return None,
    }
    (!segments.is_empty()).then(|| segments.join("."))
}

fn mongo_path_segment_safe(segment: &str) -> bool {
    !segment.is_empty() && !segment.contains('.') && !segment.starts_with('$')
}

fn typed_path_predicate(
    path: String,
    spec: &ColumnSpec,
    operator: &str,
    literal: Bson,
) -> Option<Document> {
    match &spec.source {
        ColumnSource::Field { .. } | ColumnSource::Value => {
            let types = scalar_type_names(spec)?;
            Some(doc! {path: {"$type": type_selector(types), operator: literal}})
        }
        ColumnSource::NullMask { .. } | ColumnSource::ValueNullMask
            if literal == Bson::Boolean(true) && operator == "$eq" =>
        {
            Some(doc! {path: {"$type": "null"}})
        }
        ColumnSource::EmptyStringMask { .. } | ColumnSource::ValueEmptyStringMask
            if literal == Bson::Boolean(true) && operator == "$eq" =>
        {
            Some(doc! {path: {"$type": "string", "$eq": ""}})
        }
        _ => None,
    }
}

fn type_selector(mut types: Vec<Bson>) -> Bson {
    if types.len() == 1 {
        types.pop().expect("one type")
    } else {
        Bson::Array(types)
    }
}

fn parse_filter(node: &Json) -> Result<FilterExpr, UdfError> {
    match node.get("type").and_then(Json::as_str) {
        Some("predicate_and" | "predicate_or") => {
            let is_or = node.get("type").and_then(Json::as_str) == Some("predicate_or");
            let name = if is_or { "OR" } else { "AND" };
            let expressions = node
                .get("expressions")
                .and_then(Json::as_array)
                .ok_or_else(|| UdfError::User(format!("{name} predicate has no expressions")))?
                .iter()
                .map(parse_filter)
                .collect::<Result<Vec<_>, _>>()?;
            if expressions.is_empty() {
                return Err(UdfError::User(format!("{name} predicate is empty")));
            }
            if is_or {
                Ok(FilterExpr::Or { expressions })
            } else {
                Ok(FilterExpr::And { expressions })
            }
        }
        Some("predicate_not") => Ok(FilterExpr::Not {
            expression: Box::new(
                node.get("expression")
                    .ok_or_else(|| UdfError::User("NOT predicate has no expression".into()))
                    .and_then(parse_filter)?,
            ),
        }),
        Some(
            "predicate_equal" | "predicate_notequal" | "predicate_less" | "predicate_lessequal",
        ) => parse_comparison(node),
        Some("predicate_between") => parse_between(node),
        Some("predicate_in_constlist") => {
            let column = node
                .get("expression")
                .and_then(column_name)
                .ok_or_else(|| UdfError::User("IN predicate requires a physical column".into()))?;
            let literals = node
                .get("arguments")
                .and_then(Json::as_array)
                .ok_or_else(|| UdfError::User("IN predicate has no constant list".into()))?
                .iter()
                .map(parse_literal)
                .collect::<Result<Vec<_>, _>>()?;
            if literals.is_empty() {
                return Err(UdfError::User(
                    "IN predicate has an empty constant list".into(),
                ));
            }
            Ok(FilterExpr::In { column, literals })
        }
        Some("predicate_is_null" | "predicate_is_not_null") => Ok(FilterExpr::IsNull {
            column: node
                .get("expression")
                .and_then(column_name)
                .ok_or_else(|| {
                    UdfError::User("NULL predicate requires a physical column".into())
                })?,
            negated: node.get("type").and_then(Json::as_str) == Some("predicate_is_not_null"),
        }),
        _ => Err(UdfError::User(
            "filter contains an operation that was not advertised".into(),
        )),
    }
}

fn parse_between(node: &Json) -> Result<FilterExpr, UdfError> {
    let column = node
        .get("expression")
        .and_then(column_name)
        .ok_or_else(|| UdfError::User("BETWEEN requires a physical column".into()))?;
    let lower = node
        .get("left")
        .ok_or_else(|| UdfError::User("BETWEEN has no lower bound".into()))
        .and_then(parse_literal)?;
    let upper = node
        .get("right")
        .ok_or_else(|| UdfError::User("BETWEEN has no upper bound".into()))
        .and_then(parse_literal)?;
    Ok(FilterExpr::And {
        expressions: vec![
            FilterExpr::Compare {
                op: CompareOp::GreaterEqual,
                column: column.clone(),
                literal: lower,
            },
            FilterExpr::Compare {
                op: CompareOp::LessEqual,
                column,
                literal: upper,
            },
        ],
    })
}

fn parse_comparison(node: &Json) -> Result<FilterExpr, UdfError> {
    let left = node
        .get("left")
        .ok_or_else(|| UdfError::User("comparison has no left operand".into()))?;
    let right = node
        .get("right")
        .ok_or_else(|| UdfError::User("comparison has no right operand".into()))?;
    let (column, literal, column_left) = if let Some(column) = column_name(left) {
        (column, parse_literal(right)?, true)
    } else if let Some(column) = column_name(right) {
        (column, parse_literal(left)?, false)
    } else {
        return Err(UdfError::User(
            "comparison requires one physical column and one literal".into(),
        ));
    };
    let mut op = match node.get("type").and_then(Json::as_str) {
        Some("predicate_equal") => CompareOp::Equal,
        Some("predicate_notequal") => CompareOp::NotEqual,
        Some("predicate_less") => CompareOp::Less,
        Some("predicate_lessequal") => CompareOp::LessEqual,
        _ => unreachable!(),
    };
    if !column_left {
        op = match op {
            CompareOp::Less => CompareOp::Greater,
            CompareOp::LessEqual => CompareOp::GreaterEqual,
            other => other,
        };
    }
    Ok(FilterExpr::Compare {
        op,
        column,
        literal,
    })
}

fn parse_literal(node: &Json) -> Result<Literal, UdfError> {
    let value = node
        .get("value")
        .ok_or_else(|| UdfError::User("literal has no value".into()))?;
    match node.get("type").and_then(Json::as_str) {
        Some("literal_bool") => value.as_bool().map(Literal::Boolean),
        Some("literal_double") => Some(Literal::Double(json_scalar(value)?)),
        Some("literal_exactnumeric") => Some(Literal::ExactNumeric(json_scalar(value)?)),
        Some("literal_string") => value.as_str().map(|value| Literal::String(value.into())),
        Some("literal_timestamp" | "literal_timestamp_utc") => {
            value.as_str().map(|value| Literal::Timestamp(value.into()))
        }
        _ => None,
    }
    .ok_or_else(|| UdfError::User("filter contains an invalid or unadvertised literal".into()))
}

fn parse_order_by(pushdown: &Json) -> Result<Vec<SortKey>, UdfError> {
    let Some(elements) = pushdown.get("orderBy") else {
        return Ok(Vec::new());
    };
    elements
        .as_array()
        .ok_or_else(|| UdfError::User("ORDER BY must be an array".into()))?
        .iter()
        .map(|element| {
            Ok(SortKey {
                column: element
                    .get("expression")
                    .and_then(column_name)
                    .ok_or_else(|| {
                        UdfError::User("ORDER BY requires a bare physical column".into())
                    })?,
                ascending: element
                    .get("isAscending")
                    .and_then(Json::as_bool)
                    .ok_or_else(|| UdfError::User("ORDER BY has no direction".into()))?,
                nulls_last: element
                    .get("nullsLast")
                    .and_then(Json::as_bool)
                    .ok_or_else(|| UdfError::User("ORDER BY has no NULL placement".into()))?,
            })
        })
        .collect()
}

fn parse_limit(pushdown: &Json) -> Result<Option<u64>, UdfError> {
    let Some(limit) = pushdown.get("limit") else {
        return Ok(None);
    };
    let value = limit
        .get("numElements")
        .and_then(Json::as_u64)
        .ok_or_else(|| UdfError::User("LIMIT has no non-negative row count".into()))?;
    if limit.get("offset").and_then(Json::as_u64).unwrap_or(0) != 0 {
        return Err(UdfError::User("LIMIT offset was not advertised".into()));
    }
    Ok(Some(value))
}

fn mongo_filter_exact(expr: &FilterExpr, known: &HashMap<&str, &ColumnSpec>) -> bool {
    match expr {
        FilterExpr::And { expressions } | FilterExpr::Or { expressions } => expressions
            .iter()
            .all(|expr| mongo_filter_exact(expr, known)),
        FilterExpr::Not { expression } => mongo_filter_exact(expression, known),
        FilterExpr::Compare {
            op,
            column,
            literal,
        } => known.get(column.as_str()).is_some_and(|spec| {
            literal_bson(spec, literal).is_some() && comparison_exact(spec, *op)
        }),
        FilterExpr::In { column, literals } => known.get(column.as_str()).is_some_and(|spec| {
            comparison_exact(spec, CompareOp::Equal)
                && literals
                    .iter()
                    .all(|literal| literal_bson(spec, literal).is_some())
        }),
        FilterExpr::IsNull { column, .. } => known.get(column.as_str()).is_some_and(|spec| {
            matches!(
                spec.source,
                ColumnSource::Field { .. }
                    | ColumnSource::Value
                    | ColumnSource::NullMask { .. }
                    | ColumnSource::ValueNullMask
                    | ColumnSource::EmptyStringMask { .. }
                    | ColumnSource::ValueEmptyStringMask
            )
        }),
    }
}

fn mongo_prefilter_candidate(
    expr: &FilterExpr,
    known: &HashMap<&str, &ColumnSpec>,
) -> Option<FilterExpr> {
    match expr {
        FilterExpr::And { expressions } => {
            let expressions = expressions
                .iter()
                .filter_map(|expression| mongo_prefilter_candidate(expression, known))
                .collect::<Vec<_>>();
            match expressions.len() {
                0 => None,
                1 => expressions.into_iter().next(),
                _ => Some(FilterExpr::And { expressions }),
            }
        }
        FilterExpr::Or { expressions } => Some(FilterExpr::Or {
            expressions: expressions
                .iter()
                .map(|expression| mongo_prefilter_candidate(expression, known))
                .collect::<Option<Vec<_>>>()?,
        }),
        FilterExpr::Not { .. } => None,
        FilterExpr::Compare {
            op,
            column,
            literal,
        } => {
            let spec = known.get(column.as_str())?;
            let supported = if spec.bson_kind == Some(BsonKind::String) {
                *op == CompareOp::Equal && matches!(literal, Literal::String(_))
            } else {
                *op != CompareOp::NotEqual
                    && comparison_exact(spec, *op)
                    && literal_bson(spec, literal).is_some()
            };
            supported.then(|| expr.clone())
        }
        FilterExpr::In { column, literals } => {
            let spec = known.get(column.as_str())?;
            let supported = if spec.bson_kind == Some(BsonKind::String) {
                literals
                    .iter()
                    .all(|literal| matches!(literal, Literal::String(_)))
            } else {
                comparison_exact(spec, CompareOp::Equal)
                    && literals
                        .iter()
                        .all(|literal| literal_bson(spec, literal).is_some())
            };
            supported.then(|| expr.clone())
        }
        FilterExpr::IsNull {
            column,
            negated: true,
        } => scalar_type_names(known.get(column.as_str())?).map(|_| expr.clone()),
        FilterExpr::IsNull { .. } => None,
    }
}

fn comparison_exact(spec: &ColumnSpec, op: CompareOp) -> bool {
    match (&spec.source, spec.bson_kind) {
        (ColumnSource::NullMask { .. } | ColumnSource::ValueNullMask, None) => {
            matches!(op, CompareOp::Equal | CompareOp::NotEqual)
        }
        (ColumnSource::EmptyStringMask { .. } | ColumnSource::ValueEmptyStringMask, None) => {
            matches!(op, CompareOp::Equal | CompareOp::NotEqual)
        }
        (
            ColumnSource::Field { .. } | ColumnSource::Value,
            Some(
                BsonKind::Int32
                | BsonKind::Int64
                | BsonKind::Integer
                | BsonKind::Boolean
                | BsonKind::DateTime
                | BsonKind::ObjectId,
            ),
        ) => true,
        _ => false,
    }
}

fn mongo_sort_exact(key: &SortKey, known: &HashMap<&str, &ColumnSpec>) -> bool {
    known.get(key.column.as_str()).is_some_and(|spec| {
        matches!(
            spec.source,
            ColumnSource::Field { .. } | ColumnSource::Value
        ) && matches!(
            spec.bson_kind,
            Some(
                BsonKind::Int32
                    | BsonKind::Int64
                    | BsonKind::Integer
                    | BsonKind::Boolean
                    | BsonKind::DateTime
            )
        )
    })
}

fn mongo_expression(expr: &FilterExpr, known: &HashMap<&str, &ColumnSpec>) -> Option<Bson> {
    match expr {
        FilterExpr::And { expressions } => Some(Bson::Document(
            doc! {"$and": expressions.iter().map(|expr| mongo_expression(expr, known)).collect::<Option<Vec<_>>>()?},
        )),
        FilterExpr::Or { expressions } => Some(Bson::Document(
            doc! {"$or": expressions.iter().map(|expr| mongo_expression(expr, known)).collect::<Option<Vec<_>>>()?},
        )),
        FilterExpr::Not { expression } => mongo_expression_negated(expression, known),
        FilterExpr::Compare {
            op,
            column,
            literal,
        } => {
            let spec = known.get(column.as_str())?;
            let value = mongo_column_value(spec);
            let literal = literal_bson(spec, literal)?;
            let operator = match op {
                CompareOp::Equal => "$eq",
                CompareOp::NotEqual => "$ne",
                CompareOp::Less => "$lt",
                CompareOp::LessEqual => "$lte",
                CompareOp::Greater => "$gt",
                CompareOp::GreaterEqual => "$gte",
            };
            Some(Bson::Document(
                doc! {"$and": [mongo_present(spec, value.clone()), {operator: [value, literal]}]},
            ))
        }
        FilterExpr::In { column, literals } => {
            let spec = known.get(column.as_str())?;
            let value = mongo_column_value(spec);
            let literals = literals
                .iter()
                .map(|literal| literal_bson(spec, literal))
                .collect::<Option<Vec<_>>>()?;
            Some(Bson::Document(
                doc! {"$and": [mongo_present(spec, value.clone()), {"$in": [value, literals]}]},
            ))
        }
        FilterExpr::IsNull { column, negated } => {
            let spec = known.get(column.as_str())?;
            if matches!(
                spec.source,
                ColumnSource::NullMask { .. }
                    | ColumnSource::ValueNullMask
                    | ColumnSource::EmptyStringMask { .. }
                    | ColumnSource::ValueEmptyStringMask
            ) {
                return Some(Bson::Boolean(*negated));
            }
            let present = mongo_present(spec, mongo_column_value(spec));
            Some(if *negated {
                present
            } else {
                Bson::Document(doc! {"$not": [present]})
            })
        }
    }
}

/// Compile SQL NOT by pushing negation to leaves. Wrapping the existing Mongo
/// expression in `$not` would turn SQL UNKNOWN (missing/null/wrong BSON branch)
/// into true. Keeping each leaf's presence guard preserves WHERE's three-valued
/// logic while De Morgan handles nested boolean expressions.
fn mongo_expression_negated(expr: &FilterExpr, known: &HashMap<&str, &ColumnSpec>) -> Option<Bson> {
    match expr {
        FilterExpr::And { expressions } => Some(Bson::Document(
            doc! {"$or": expressions.iter().map(|expr| mongo_expression_negated(expr, known)).collect::<Option<Vec<_>>>()?},
        )),
        FilterExpr::Or { expressions } => Some(Bson::Document(
            doc! {"$and": expressions.iter().map(|expr| mongo_expression_negated(expr, known)).collect::<Option<Vec<_>>>()?},
        )),
        FilterExpr::Not { expression } => mongo_expression(expression, known),
        FilterExpr::Compare {
            op,
            column,
            literal,
        } => mongo_expression(
            &FilterExpr::Compare {
                op: match op {
                    CompareOp::Equal => CompareOp::NotEqual,
                    CompareOp::NotEqual => CompareOp::Equal,
                    CompareOp::Less => CompareOp::GreaterEqual,
                    CompareOp::LessEqual => CompareOp::Greater,
                    CompareOp::Greater => CompareOp::LessEqual,
                    CompareOp::GreaterEqual => CompareOp::Less,
                },
                column: column.clone(),
                literal: literal.clone(),
            },
            known,
        ),
        FilterExpr::In { column, literals } => {
            let spec = known.get(column.as_str())?;
            let value = mongo_column_value(spec);
            let literals = literals
                .iter()
                .map(|literal| literal_bson(spec, literal))
                .collect::<Option<Vec<_>>>()?;
            Some(Bson::Document(doc! {
                "$and": [
                    mongo_present(spec, value.clone()),
                    {"$not": [{"$in": [value, literals]}]}
                ]
            }))
        }
        FilterExpr::IsNull { column, negated } => mongo_expression(
            &FilterExpr::IsNull {
                column: column.clone(),
                negated: !negated,
            },
            known,
        ),
    }
}

fn mongo_column_value(spec: &ColumnSpec) -> Bson {
    match &spec.source {
        ColumnSource::Field { name }
        | ColumnSource::ObjectLink { name }
        | ColumnSource::ArrayLength { name } => Bson::Document(
            doc! {"$getField": {"field": {"$literal": name}, "input": format!("${VALUE_FIELD}")}},
        ),
        ColumnSource::Value => Bson::String(format!("${VALUE_FIELD}")),
        ColumnSource::ValueArrayLength => Bson::Document(
            doc! {"$cond": [{"$isArray": format!("${VALUE_FIELD}")}, {"$size": format!("${VALUE_FIELD}")}, Bson::Null]},
        ),
        ColumnSource::NullMask { name } => Bson::Document(
            doc! {"$eq": [{"$type": {"$getField": {"field": {"$literal": name}, "input": format!("${VALUE_FIELD}")}}}, "null"]},
        ),
        ColumnSource::ValueNullMask => {
            Bson::Document(doc! {"$eq": [{"$type": format!("${VALUE_FIELD}")}, "null"]})
        }
        ColumnSource::EmptyStringMask { name } => Bson::Document(
            doc! {"$eq": [{"$getField": {"field": {"$literal": name}, "input": format!("${VALUE_FIELD}")}}, ""]},
        ),
        ColumnSource::ValueEmptyStringMask => {
            Bson::Document(doc! {"$eq": [format!("${VALUE_FIELD}"), ""]})
        }
        _ => Bson::Null,
    }
}

fn mongo_present(spec: &ColumnSpec, value: Bson) -> Bson {
    match &spec.source {
        ColumnSource::NullMask { .. }
        | ColumnSource::ValueNullMask
        | ColumnSource::EmptyStringMask { .. }
        | ColumnSource::ValueEmptyStringMask => Bson::Boolean(true),
        ColumnSource::Field { .. } | ColumnSource::Value => {
            let types = scalar_type_names(spec).unwrap_or_default();
            let type_expr = Bson::Document(doc! {"$type": value.clone()});
            let mut checks = vec![Bson::Document(doc! {"$in": [type_expr, types]})];
            if spec.bson_kind == Some(BsonKind::String) {
                checks.push(Bson::Document(doc! {"$ne": [value, ""]}));
            }
            Bson::Document(doc! {"$and": checks})
        }
        _ => Bson::Boolean(true),
    }
}

fn scalar_type_names(spec: &ColumnSpec) -> Option<Vec<Bson>> {
    Some(match spec.bson_kind? {
        BsonKind::Int32 => vec!["int".into()],
        BsonKind::Int64 => vec!["long".into()],
        BsonKind::Integer => vec!["int".into(), "long".into()],
        BsonKind::Double => vec!["double".into()],
        BsonKind::Boolean => vec!["bool".into()],
        BsonKind::DateTime => vec!["date".into()],
        BsonKind::ObjectId => vec!["objectId".into()],
        BsonKind::String => vec!["string".into()],
        _ => return None,
    })
}

fn literal_bson(spec: &ColumnSpec, literal: &Literal) -> Option<Bson> {
    match (spec.bson_kind, literal) {
        (
            Some(BsonKind::Int32 | BsonKind::Int64 | BsonKind::Integer),
            Literal::ExactNumeric(value),
        ) => value.parse::<i64>().ok().map(Bson::Int64),
        (Some(BsonKind::Double), Literal::Double(value) | Literal::ExactNumeric(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(Bson::Double),
        (Some(BsonKind::Boolean), Literal::Boolean(value)) => Some(Bson::Boolean(*value)),
        (Some(BsonKind::String), Literal::String(value)) => Some(Bson::String(value.clone())),
        (Some(BsonKind::ObjectId), Literal::String(value)) => {
            mongodb::bson::oid::ObjectId::parse_str(value)
                .ok()
                .map(Bson::ObjectId)
        }
        (Some(BsonKind::DateTime), Literal::Timestamp(value)) => {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|value| {
                    Bson::DateTime(DateTime::from_millis(value.and_utc().timestamp_millis()))
                })
        }
        (None, Literal::Boolean(value))
            if matches!(
                spec.source,
                ColumnSource::NullMask { .. }
                    | ColumnSource::ValueNullMask
                    | ColumnSource::EmptyStringMask { .. }
                    | ColumnSource::ValueEmptyStringMask
            ) =>
        {
            Some(Bson::Boolean(*value))
        }
        _ => None,
    }
}

fn render_filter(
    expr: &FilterExpr,
    known: &HashMap<&str, &ColumnSpec>,
) -> Result<String, UdfError> {
    Ok(match expr {
        FilterExpr::And { expressions } => format!(
            "({})",
            expressions
                .iter()
                .map(|expr| render_filter(expr, known))
                .collect::<Result<Vec<_>, _>>()?
                .join(" AND ")
        ),
        FilterExpr::Or { expressions } => format!(
            "({})",
            expressions
                .iter()
                .map(|expr| render_filter(expr, known))
                .collect::<Result<Vec<_>, _>>()?
                .join(" OR ")
        ),
        FilterExpr::Not { expression } => {
            format!("(NOT {})", render_filter(expression, known)?)
        }
        FilterExpr::Compare {
            op,
            column,
            literal,
        } => {
            let spec = known
                .get(column.as_str())
                .ok_or_else(|| UdfError::User("filter references an unknown column".into()))?;
            format!(
                "({} {} {})",
                quote_ident(column),
                match op {
                    CompareOp::Equal => "=",
                    CompareOp::NotEqual => "<>",
                    CompareOp::Less => "<",
                    CompareOp::LessEqual => "<=",
                    CompareOp::Greater => ">",
                    CompareOp::GreaterEqual => ">=",
                },
                render_literal(literal, spec)?
            )
        }
        FilterExpr::In { column, literals } => {
            let spec = known
                .get(column.as_str())
                .ok_or_else(|| UdfError::User("filter references an unknown column".into()))?;
            format!(
                "({} IN ({}))",
                quote_ident(column),
                literals
                    .iter()
                    .map(|literal| render_literal(literal, spec))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ")
            )
        }
        FilterExpr::IsNull { column, negated } => format!(
            "({} IS {}NULL)",
            quote_ident(column),
            if *negated { "NOT " } else { "" }
        ),
    })
}

fn render_literal(literal: &Literal, _spec: &ColumnSpec) -> Result<String, UdfError> {
    render_untyped_literal(literal)
}

fn render_untyped_literal(literal: &Literal) -> Result<String, UdfError> {
    Ok(match literal {
        Literal::Boolean(value) => {
            if *value {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        Literal::Double(value) | Literal::ExactNumeric(value) => {
            if !value.bytes().all(|byte| {
                byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E')
            }) {
                return Err(UdfError::User("numeric literal is invalid".into()));
            }
            value.clone()
        }
        Literal::String(value) => quote_string(value),
        Literal::Timestamp(value) => format!("TIMESTAMP {}", quote_string(value)),
    })
}

fn referenced_columns(expr: &FilterExpr) -> Vec<String> {
    let mut result = Vec::new();
    match expr {
        FilterExpr::And { expressions } | FilterExpr::Or { expressions } => expressions
            .iter()
            .for_each(|expr| result.extend(referenced_columns(expr))),
        FilterExpr::Not { expression } => result.extend(referenced_columns(expression)),
        FilterExpr::Compare { column, .. }
        | FilterExpr::In { column, .. }
        | FilterExpr::IsNull { column, .. } => result.push(column.clone()),
    }
    deduplicate(&mut result);
    result
}

fn validate_columns(
    columns: &[String],
    known: &HashMap<&str, &ColumnSpec>,
) -> Result<(), UdfError> {
    if let Some(name) = columns
        .iter()
        .find(|name| !known.contains_key(name.as_str()))
    {
        return Err(UdfError::User(format!(
            "pushdown references unknown column '{name}'"
        )));
    }
    Ok(())
}

fn deduplicate(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn involved_columns(request: &Json) -> Option<Vec<String>> {
    let columns = request
        .get("involvedTables")?
        .as_array()?
        .first()?
        .get("columns")?
        .as_array()?
        .iter()
        .map(|column| column.get("name")?.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    (!columns.is_empty()).then_some(columns)
}

fn branch_source(source: &ColumnSource) -> Option<(bool, &str)> {
    match source {
        ColumnSource::Field { name }
        | ColumnSource::ObjectLink { name }
        | ColumnSource::ArrayLength { name } => Some((false, name)),
        ColumnSource::Value | ColumnSource::ValueObjectMarker | ColumnSource::ValueArrayLength => {
            Some((true, ""))
        }
        _ => None,
    }
}

fn is_column(value: &Json) -> bool {
    value.get("type").and_then(Json::as_str) == Some("column")
}
fn column_name(value: &Json) -> Option<String> {
    is_column(value)
        .then(|| value.get("name")?.as_str().map(str::to_owned))
        .flatten()
}
fn json_scalar(value: &Json) -> Result<String, UdfError> {
    match value {
        Json::Number(value) => Ok(value.to_string()),
        Json::String(value) => Ok(value.clone()),
        _ => Err(UdfError::User("numeric literal is not scalar".into())),
    }
}
fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SqlType;

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
            ColumnSpec {
                source: ColumnSource::NullMask {
                    name: "name".into(),
                },
                exasol_name: "name|n".into(),
                sql_type: SqlType::Boolean,
                bson_kind: None,
            },
        ]
    }

    fn extended_columns() -> Vec<ColumnSpec> {
        let mut values = columns();
        values.extend([
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
            ColumnSpec {
                source: ColumnSource::EmptyStringMask {
                    name: "name".into(),
                },
                exasol_name: "name|empty".into(),
                sql_type: SqlType::Boolean,
                bson_kind: None,
            },
            ColumnSpec {
                source: ColumnSource::RowId,
                exasol_name: "_id".into(),
                sql_type: SqlType::Varchar { size: 64 },
                bson_kind: None,
            },
        ]);
        values
    }

    fn count_request(arguments: Vec<Json>) -> Json {
        serde_json::json!({
            "pushdownRequest": {
                "type": "select",
                "aggregationType": "single_group",
                "selectList": [{
                    "type": "function_aggregate",
                    "name": "count",
                    "arguments": arguments,
                    "distinct": false
                }],
                "selectListDataTypes": [{"type":"decimal", "precision":18, "scale":0}]
            }
        })
    }

    #[test]
    fn advertises_and_plans_remote_single_group_count_star() {
        for capability in [
            "AGGREGATE_SINGLE_GROUP",
            "SELECTLIST_EXPRESSIONS",
            "FN_AGG_COUNT",
            "FN_AGG_COUNT_STAR",
        ] {
            assert!(CAPABILITIES.contains(&capability));
        }

        let query = plan(&count_request(vec![]), &extended_columns()).unwrap();
        assert_eq!(
            query.aggregation,
            Some(SingleGroupAggregation {
                expressions: vec![AggregateExpr::CountStar]
            })
        );
        assert_eq!(query.required, ["_id"]);
        assert_eq!(query.mongo.aggregation, Some(MongoAggregation::CountStar));
        assert_eq!(
            render_outer_sql("SELECT scan", &query, &extended_columns()).unwrap(),
            "SELECT \"__jt_count\" FROM (SELECT scan) \"MONGO_PUSHDOWN\""
        );
        let stages = mongo_stages(&query.mongo, &extended_columns());
        assert_eq!(stages, [doc! {"$count": AGGREGATE_COUNT_FIELD}]);
    }

    #[test]
    fn constants_alongside_count_star_keep_remote_count_pushdown() {
        let mut labelled = count_request(vec![]);
        labelled["pushdownRequest"]["selectList"] = serde_json::json!([
            {"type":"literal_string", "value":"mongodb"},
            {"type":"function_aggregate", "name":"count", "arguments":[], "distinct":false},
            {"type":"literal_exactnumeric", "value":1}
        ]);
        labelled["pushdownRequest"]["selectListDataTypes"] = serde_json::json!([
            {"type":"varchar", "size":8},
            {"type":"decimal", "precision":18, "scale":0},
            {"type":"decimal", "precision":1, "scale":0}
        ]);

        let query = plan(&labelled, &extended_columns()).unwrap();
        assert_eq!(query.mongo.aggregation, Some(MongoAggregation::CountStar));
        assert_eq!(
            query.aggregation,
            Some(SingleGroupAggregation {
                expressions: vec![
                    AggregateExpr::Constant {
                        literal: Literal::String("mongodb".into())
                    },
                    AggregateExpr::CountStar,
                    AggregateExpr::Constant {
                        literal: Literal::ExactNumeric("1".into())
                    }
                ]
            })
        );
        assert_eq!(
            render_outer_sql("SELECT scan", &query, &extended_columns()).unwrap(),
            "SELECT 'mongodb', \"__jt_count\", 1 FROM (SELECT scan) \"MONGO_PUSHDOWN\""
        );
        assert_eq!(
            mongo_stages(&query.mongo, &extended_columns()),
            [doc! {"$count": AGGREGATE_COUNT_FIELD}]
        );

        let mut column_count = labelled;
        column_count["pushdownRequest"]["selectList"][1]["arguments"] =
            serde_json::json!([{"type":"column", "name":"name"}]);
        let query = plan(&column_count, &extended_columns()).unwrap();
        assert_eq!(query.mongo.aggregation, None);
        assert_eq!(
            render_outer_sql("SELECT scan", &query, &extended_columns()).unwrap(),
            "SELECT 'mongodb', COUNT(\"name\"), 1 FROM (SELECT scan) \"MONGO_PUSHDOWN\""
        );
    }

    #[test]
    fn count_star_pushes_only_an_exact_filter_before_count() {
        let mut exact = count_request(vec![]);
        exact["pushdownRequest"]["filter"] = serde_json::json!({
            "type":"predicate_between",
            "expression":{"type":"column","name":"age"},
            "left":{"type":"literal_exactnumeric","value":18},
            "right":{"type":"literal_exactnumeric","value":65}
        });
        let exact = plan(&exact, &extended_columns()).unwrap();
        assert_eq!(exact.mongo.aggregation, Some(MongoAggregation::CountStar));
        let stages = mongo_stages(&exact.mongo, &extended_columns());
        assert_eq!(stages.len(), 2);
        assert!(stages[0].contains_key("$match"));
        assert_eq!(stages[1], doc! {"$count": AGGREGATE_COUNT_FIELD});
        assert!(
            !render_outer_sql("SELECT scan", &exact, &extended_columns())
                .unwrap()
                .contains("WHERE")
        );

        let mut inexact = count_request(vec![]);
        inexact["pushdownRequest"]["filter"] = serde_json::json!({
            "type":"predicate_equal",
            "left":{"type":"column","name":"name"},
            "right":{"type":"literal_string","value":"Ada"}
        });
        let inexact = plan(&inexact, &extended_columns()).unwrap();
        assert_eq!(inexact.mongo.aggregation, None);
        assert!(inexact.mongo.prefilter.is_some());
        assert!(inexact.mongo.filter.is_none());
        assert_eq!(
            render_outer_sql("SELECT scan", &inexact, &extended_columns()).unwrap(),
            "SELECT COUNT(*) FROM (SELECT scan) \"MONGO_PUSHDOWN\" WHERE (\"name\" = 'Ada')"
        );
    }

    #[test]
    fn count_column_uses_the_exact_exasol_fallback() {
        let request = count_request(vec![serde_json::json!({"type":"column","name":"name"})]);
        let query = plan(&request, &extended_columns()).unwrap();
        assert_eq!(query.mongo.aggregation, None);
        assert_eq!(
            query.aggregation,
            Some(SingleGroupAggregation {
                expressions: vec![AggregateExpr::CountColumn {
                    column: "name".into()
                }]
            })
        );
        assert_eq!(query.required, ["name"]);
        assert_eq!(
            render_outer_sql("SELECT scan", &query, &extended_columns()).unwrap(),
            "SELECT COUNT(\"name\") FROM (SELECT scan) \"MONGO_PUSHDOWN\""
        );
    }

    #[test]
    fn rejects_unadvertised_aggregate_shapes() {
        let mut grouped = count_request(vec![]);
        grouped["pushdownRequest"]["aggregationType"] = Json::String("group_by".into());
        let mut distinct = count_request(vec![]);
        distinct["pushdownRequest"]["selectList"][0]["distinct"] = Json::Bool(true);
        let mut tuple = count_request(vec![]);
        tuple["pushdownRequest"]["selectList"][0]["arguments"] = serde_json::json!([
            {"type":"column","name":"age"},
            {"type":"column","name":"name"}
        ]);
        let mut sum = count_request(vec![]);
        sum["pushdownRequest"]["selectList"][0]["name"] = Json::String("sum".into());

        for request in [grouped, distinct, tuple, sum] {
            assert!(plan(&request, &extended_columns()).is_err());
        }
    }

    #[test]
    fn plans_projection_filter_limit_and_exact_numeric_pushdown() {
        let request = serde_json::json!({"pushdownRequest": {"type":"select", "selectList":[{"type":"column","name":"name"}], "filter":{"type":"predicate_less","left":{"type":"column","name":"age"},"right":{"type":"literal_exactnumeric","value":40}}, "limit":{"numElements":5}}});
        let plan = plan(&request, &columns()).unwrap();
        assert_eq!(plan.selected, ["name"]);
        assert_eq!(plan.required, ["name", "age"]);
        assert!(plan.mongo.filter.is_some());
        assert_eq!(plan.mongo.limit, Some(5));
        let sql = render_outer_sql("SELECT scan", &plan, &columns()).unwrap();
        assert_eq!(
            sql,
            "SELECT \"name\" FROM (SELECT scan) \"MONGO_PUSHDOWN\" WHERE (\"age\" < 40) LIMIT 5"
        );
    }

    #[test]
    fn between_lowers_to_an_exact_inclusive_mongo_range() {
        assert!(CAPABILITIES.contains(&"FN_PRED_BETWEEN"));
        let request = serde_json::json!({
            "pushdownRequest": {
                "type": "select",
                "selectList": [{"type": "column", "name": "name"}],
                "filter": {
                    "type": "predicate_between",
                    "expression": {"type": "column", "name": "age"},
                    "left": {"type": "literal_exactnumeric", "value": 18},
                    "right": {"type": "literal_exactnumeric", "value": 65}
                },
                "limit": {"numElements": 5}
            }
        });

        let query = plan(&request, &columns()).unwrap();
        assert_eq!(query.required, ["name", "age"]);
        assert!(query.mongo.filter.is_some());
        assert_eq!(query.mongo.limit, Some(5));
        let mongo = serde_json::to_string(&mongo_stages(&query.mongo, &columns())).unwrap();
        assert!(mongo.contains("$gte"));
        assert!(mongo.contains("$lte"));
        let outer = render_outer_sql("SELECT scan", &query, &columns()).unwrap();
        assert!(outer.contains("(\"age\" >= 18)"));
        assert!(outer.contains("(\"age\" <= 65)"));
    }

    #[test]
    fn between_is_all_or_nothing_and_rejects_invalid_shapes() {
        let inexact = serde_json::json!({
            "pushdownRequest": {
                "type": "select",
                "filter": {
                    "type": "predicate_between",
                    "expression": {"type": "column", "name": "score"},
                    "left": {"type": "literal_double", "value": "1.5"},
                    "right": {"type": "literal_double", "value": "2.5"}
                },
                "limit": {"numElements": 1}
            }
        });
        let query = plan(&inexact, &extended_columns()).unwrap();
        assert!(query.mongo.filter.is_none());
        assert!(query.mongo.limit.is_none());

        for invalid in [
            serde_json::json!({"type":"predicate_between","expression":{"type":"literal_exactnumeric","value":1},"left":{"type":"literal_exactnumeric","value":0},"right":{"type":"literal_exactnumeric","value":2}}),
            serde_json::json!({"type":"predicate_between","expression":{"type":"column","name":"age"},"right":{"type":"literal_exactnumeric","value":2}}),
            serde_json::json!({"type":"predicate_between","expression":{"type":"column","name":"age"},"left":{"type":"literal_exactnumeric","value":0}}),
        ] {
            let request = serde_json::json!({"pushdownRequest":{"type":"select","filter":invalid}});
            assert!(plan(&request, &columns()).is_err());
        }
    }

    #[test]
    fn builds_native_dotted_path_prefilter_for_nested_where_identifier() {
        let request = serde_json::json!({
            "pushdownRequest": {
                "type": "select",
                "filter": {
                    "type": "predicate_less",
                    "left": {"type": "column", "name": "age"},
                    "right": {"type": "literal_exactnumeric", "value": 40}
                }
            }
        });
        let query = plan(&request, &columns()).unwrap();
        let path = vec![PathSegment {
            name: "profile".into(),
            kind: crate::model::PathKind::Object,
            direct: false,
        }];

        assert_eq!(
            mongo_path_prefilter(&query.mongo, &columns(), &path),
            Some(doc! {
                "profile.age": {"$type": "int", "$lt": Bson::Int64(40)}
            })
        );
    }

    #[test]
    fn dotted_path_prefilter_is_conservative_for_boolean_composition() {
        let positive = FilterExpr::Compare {
            op: CompareOp::GreaterEqual,
            column: "age".into(),
            literal: Literal::ExactNumeric("18".into()),
        };
        let negated = FilterExpr::Not {
            expression: Box::new(positive.clone()),
        };
        let path = [PathSegment {
            name: "items".into(),
            kind: crate::model::PathKind::Array,
            direct: false,
        }];

        let conjunction = MongoPushdown {
            filter: Some(FilterExpr::And {
                expressions: vec![positive.clone(), negated.clone()],
            }),
            ..MongoPushdown::default()
        };
        assert_eq!(
            mongo_path_prefilter(&conjunction, &columns(), &path),
            Some(doc! {
                "items.age": {"$type": "int", "$gte": Bson::Int64(18)}
            })
        );

        let disjunction = MongoPushdown {
            filter: Some(FilterExpr::Or {
                expressions: vec![positive, negated],
            }),
            ..MongoPushdown::default()
        };
        assert_eq!(mongo_path_prefilter(&disjunction, &columns(), &path), None);
    }

    #[test]
    fn dotted_path_prefilter_declines_ambiguous_literal_field_names_and_nested_arrays() {
        let pushdown = MongoPushdown {
            filter: Some(FilterExpr::Compare {
                op: CompareOp::Equal,
                column: "age".into(),
                literal: Literal::ExactNumeric("18".into()),
            }),
            ..MongoPushdown::default()
        };
        for path in [
            vec![PathSegment {
                name: "literal.name".into(),
                kind: crate::model::PathKind::Object,
                direct: false,
            }],
            vec![
                PathSegment {
                    name: "matrix".into(),
                    kind: crate::model::PathKind::Array,
                    direct: false,
                },
                PathSegment {
                    name: String::new(),
                    kind: crate::model::PathKind::Array,
                    direct: true,
                },
            ],
        ] {
            assert_eq!(mongo_path_prefilter(&pushdown, &columns(), &path), None);
        }
    }

    #[test]
    fn or_and_not_are_exact_all_or_nothing_boolean_pushdowns() {
        assert!(CAPABILITIES.contains(&"FN_PRED_OR"));
        assert!(CAPABILITIES.contains(&"FN_PRED_NOT"));
        let exact = serde_json::json!({
            "pushdownRequest": {
                "type":"select",
                "selectList":[{"type":"column","name":"age"}],
                "filter": {
                    "type":"predicate_not",
                    "expression": {
                        "type":"predicate_or",
                        "expressions":[
                            {"type":"predicate_equal","left":{"type":"column","name":"age"},"right":{"type":"literal_exactnumeric","value":18}},
                            {"type":"predicate_in_constlist","expression":{"type":"column","name":"age"},"arguments":[{"type":"literal_exactnumeric","value":21},{"type":"literal_exactnumeric","value":65}]}
                        ]
                    }
                },
                "limit":{"numElements":2}
            }
        });
        let query = plan(&exact, &extended_columns()).unwrap();
        assert!(query.mongo.filter.is_some());
        assert_eq!(query.mongo.limit, Some(2));
        let mongo =
            serde_json::to_string(&mongo_stages(&query.mongo, &extended_columns())).unwrap();
        // NOT(OR(...)) is lowered with De Morgan. Each negated leaf retains a
        // presence/type guard, so missing/null remains SQL UNKNOWN, not true.
        assert!(mongo.contains("$and"));
        assert!(mongo.contains("$ne"));
        assert!(mongo.contains("$not"));
        assert!(mongo.contains("$type"));
        let outer = render_outer_sql("SELECT scan", &query, &extended_columns()).unwrap();
        assert!(outer.contains("NOT"));
        assert!(outer.contains(" OR "));

        let inexact = serde_json::json!({
            "pushdownRequest": {
                "type":"select",
                "filter": {
                    "type":"predicate_or",
                    "expressions":[
                        {"type":"predicate_equal","left":{"type":"column","name":"age"},"right":{"type":"literal_exactnumeric","value":18}},
                        {"type":"predicate_equal","left":{"type":"column","name":"name"},"right":{"type":"literal_string","value":"Ada"}}
                    ]
                },
                "limit":{"numElements":1}
            }
        });
        let query = plan(&inexact, &extended_columns()).unwrap();
        assert!(query.mongo.filter.is_none());
        assert!(query.mongo.limit.is_none());
        assert!(
            render_outer_sql("SELECT scan", &query, &extended_columns())
                .unwrap()
                .contains(" OR ")
        );
    }

    #[test]
    fn rejects_malformed_or_and_not_predicates() {
        for filter in [
            serde_json::json!({"type":"predicate_or","expressions":[]}),
            serde_json::json!({"type":"predicate_or"}),
            serde_json::json!({"type":"predicate_not"}),
        ] {
            let request = serde_json::json!({"pushdownRequest":{"type":"select","filter":filter}});
            assert!(plan(&request, &extended_columns()).is_err());
        }
    }

    #[test]
    fn selecting_one_array_variant_keeps_all_value_branches_for_validation() {
        let columns = vec![
            ColumnSpec {
                source: ColumnSource::Value,
                exasol_name: "_value".into(),
                sql_type: SqlType::Decimal {
                    precision: 10,
                    scale: 0,
                },
                bson_kind: Some(BsonKind::Int32),
            },
            ColumnSpec {
                source: ColumnSource::Value,
                exasol_name: "_value|string".into(),
                sql_type: SqlType::Varchar { size: 20 },
                bson_kind: Some(BsonKind::String),
            },
            ColumnSpec {
                source: ColumnSource::ValueObjectMarker,
                exasol_name: "_value|object".into(),
                sql_type: SqlType::Boolean,
                bson_kind: None,
            },
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
        let request = serde_json::json!({
            "pushdownRequest": {
                "type": "select",
                "selectList": [{"type": "column", "name": "_value|string"}]
            }
        });

        let query = plan(&request, &columns).unwrap();
        assert_eq!(query.selected, ["_value|string"]);
        assert_eq!(
            query.required,
            ["_value|string", "_value", "_value|object", "_value|array"]
        );
    }

    #[test]
    fn selecting_scalar_or_object_variant_keeps_both_branches_for_validation() {
        let columns = vec![
            ColumnSpec {
                source: ColumnSource::Field {
                    name: "payload".into(),
                },
                exasol_name: "payload".into(),
                sql_type: SqlType::Varchar { size: 20 },
                bson_kind: Some(BsonKind::String),
            },
            ColumnSpec {
                source: ColumnSource::ObjectLink {
                    name: "payload".into(),
                },
                exasol_name: "payload|object".into(),
                sql_type: SqlType::Varchar { size: 64 },
                bson_kind: None,
            },
        ];

        for (selected, expected) in [
            ("payload", vec!["payload", "payload|object"]),
            ("payload|object", vec!["payload|object", "payload"]),
        ] {
            let request = serde_json::json!({
                "pushdownRequest": {
                    "type": "select",
                    "selectList": [{"type": "column", "name": selected}]
                }
            });
            let query = plan(&request, &columns).unwrap();
            assert_eq!(query.selected, [selected]);
            assert_eq!(query.required, expected);
        }
    }

    #[test]
    fn string_equality_gets_a_dotted_prefilter_but_stays_in_exasol() {
        let request = serde_json::json!({"pushdownRequest": {"type":"select", "filter":{"type":"predicate_equal","left":{"type":"column","name":"name"},"right":{"type":"literal_string","value":"O'Reilly"}}, "limit":{"numElements":2}}});
        let query = plan(&request, &columns()).unwrap();
        assert!(query.mongo.prefilter.is_some());
        assert!(query.mongo.filter.is_none());
        assert!(query.mongo.limit.is_none());
        let path = [PathSegment {
            name: "items".into(),
            kind: crate::model::PathKind::Array,
            direct: false,
        }];
        assert_eq!(
            mongo_path_prefilter(&query.mongo, &columns(), &path),
            Some(doc! {
                "items.name": {
                    "$type": "string",
                    "$eq": "O'Reilly"
                }
            })
        );
        assert!(mongo_stages(&query.mongo, &columns()).is_empty());
        assert!(
            render_outer_sql("SELECT scan", &query, &columns())
                .unwrap()
                .contains("'O''Reilly'")
        );

        let range = serde_json::json!({"pushdownRequest": {
            "type":"select",
            "filter":{
                "type":"predicate_less",
                "left":{"type":"column","name":"name"},
                "right":{"type":"literal_string","value":"Z"}
            }
        }});
        let range = plan(&range, &columns()).unwrap();
        assert!(range.mongo.prefilter.is_none());
        assert!(range.mongo.filter.is_none());
    }

    #[test]
    fn string_in_prefilter_preserves_exact_values_and_trailing_spaces() {
        let request = serde_json::json!({"pushdownRequest": {
            "type":"select",
            "filter":{
                "type":"predicate_in_constlist",
                "expression":{"type":"column","name":"name"},
                "arguments":[
                    {"type":"literal_string","value":"SKU-1"},
                    {"type":"literal_string","value":"SKU-2 "}
                ]
            }
        }});
        let query = plan(&request, &columns()).unwrap();
        let prefilter = mongo_path_prefilter(&query.mongo, &columns(), &[]).unwrap();
        assert_eq!(
            prefilter,
            doc! {
                "name": {"$type": "string", "$in": ["SKU-1", "SKU-2 "]}
            }
        );
        assert!(query.mongo.filter.is_none());
    }

    #[test]
    fn eligible_topn_builds_null_rank_sort_and_limit() {
        let request = serde_json::json!({"pushdownRequest": {"type":"select", "selectList":[{"type":"column","name":"age"}], "orderBy":[{"type":"order_by_element","expression":{"type":"column","name":"age"},"isAscending":false,"nullsLast":true}], "limit":{"numElements":3}}});
        let plan = plan(&request, &columns()).unwrap();
        assert_eq!(plan.mongo.order_by.len(), 1);
        let stages = mongo_stages(&plan.mongo, &columns());
        assert!(stages.iter().any(|stage| stage.contains_key("$sort")));
        assert!(stages.iter().any(|stage| stage.contains_key("$limit")));
    }

    #[test]
    fn parser_is_all_or_nothing_and_rejects_unadvertised_shapes() {
        let or = serde_json::json!({"pushdownRequest":{"type":"select","filter":{"type":"predicate_or","expressions":[]}}});
        assert!(plan(&or, &columns()).is_err());
        let offset = serde_json::json!({"pushdownRequest":{"type":"select","limit":{"numElements":1,"offset":1}}});
        assert!(plan(&offset, &columns()).is_err());
    }

    #[test]
    fn parses_and_compiles_comparisons_in_inverted_and_mask_forms() {
        let request = serde_json::json!({
            "pushdownRequest": {
                "type":"select",
                "selectList":[{"type":"column","name":"age"}],
                "filter":{"type":"predicate_and","expressions":[
                    {"type":"predicate_less","left":{"type":"literal_exactnumeric","value":1},"right":{"type":"column","name":"age"}},
                    {"type":"predicate_notequal","left":{"type":"column","name":"active"},"right":{"type":"literal_bool","value":false}},
                    {"type":"predicate_in_constlist","expression":{"type":"column","name":"age"},"arguments":[{"type":"literal_exactnumeric","value":2},{"type":"literal_exactnumeric","value":"3"}]},
                    {"type":"predicate_equal","left":{"type":"column","name":"name|n"},"right":{"type":"literal_bool","value":true}},
                    {"type":"predicate_equal","left":{"type":"column","name":"name|empty"},"right":{"type":"literal_bool","value":false}}
                ]}
            }
        });
        let query = plan(&request, &extended_columns()).unwrap();
        assert!(query.mongo.filter.is_some());
        let json = serde_json::to_string(&mongo_stages(&query.mongo, &extended_columns())).unwrap();
        assert!(json.contains("$gt"));
        assert!(json.contains("$in"));
        assert!(json.contains("name"));
    }

    #[test]
    fn null_checks_distinguish_scalar_presence_from_non_null_masks() {
        for (column, negated, expected) in [
            ("name", false, "$not"),
            ("name", true, "$type"),
            ("name|n", false, "false"),
            ("name|empty", true, "true"),
        ] {
            let request = serde_json::json!({"pushdownRequest":{"type":"select","selectList":[{"type":"column","name":"age"}],"filter":{"type":if negated {"predicate_is_not_null"} else {"predicate_is_null"},"expression":{"type":"column","name":column}}}});
            let query = plan(&request, &extended_columns()).unwrap();
            let json =
                serde_json::to_string(&mongo_stages(&query.mongo, &extended_columns())).unwrap();
            assert!(json.contains(expected), "{column}: {json}");
        }
        let structural = serde_json::json!({"pushdownRequest":{"type":"select","filter":{"type":"predicate_is_null","expression":{"type":"column","name":"_id"}},"limit":{"numElements":1}}});
        let query = plan(&structural, &extended_columns()).unwrap();
        assert!(query.mongo.filter.is_none());
        assert!(query.mongo.limit.is_none());
    }

    #[test]
    fn converts_object_id_and_timestamp_but_declines_double_literals() {
        let oid = "0123456789abcdef01234567";
        let request = serde_json::json!({"pushdownRequest":{"type":"select","filter":{"type":"predicate_and","expressions":[
        {"type":"predicate_equal","left":{"type":"column","name":"mongo_id"},"right":{"type":"literal_string","value":oid}},
                {"type":"predicate_less","left":{"type":"column","name":"created"},"right":{"type":"literal_timestamp","value":"2026-08-09 10:11:12.123"}}
            ]}}});
        let query = plan(&request, &extended_columns()).unwrap();
        let json = serde_json::to_string(&mongo_stages(&query.mongo, &extended_columns())).unwrap();
        assert!(json.contains("$oid"));
        assert!(json.contains("$date"));

        let double = serde_json::json!({"pushdownRequest":{"type":"select","filter":{"type":"predicate_equal","left":{"type":"column","name":"score"},"right":{"type":"literal_double","value":"1.25"}},"limit":{"numElements":1}}});
        let double = plan(&double, &extended_columns()).unwrap();
        assert!(double.mongo.filter.is_none());
        assert!(double.mongo.limit.is_none());
    }

    #[test]
    fn residual_projection_forms_use_involved_columns_or_one_count_carrier() {
        let involved = serde_json::json!([{"name":"T","columns":[{"name":"name"},{"name":"age"}]}]);
        let full = serde_json::json!({"involvedTables":involved,"pushdownRequest":{"type":"select","selectListDataTypes":[{"type":"varchar"},{"type":"decimal"}]}});
        assert_eq!(plan(&full, &columns()).unwrap().selected, ["name", "age"]);

        let count = serde_json::json!({"involvedTables":[{"name":"T","columns":[{"name":"age"},{"name":"name"}]}],"pushdownRequest":{"type":"select"}});
        assert_eq!(plan(&count, &columns()).unwrap().selected, ["age"]);
    }

    #[test]
    fn outer_renderer_preserves_order_nulls_limit_and_declined_filter() {
        let request = serde_json::json!({"pushdownRequest":{"type":"select","selectList":[{"type":"column","name":"name"}],"filter":{"type":"predicate_in_constlist","expression":{"type":"column","name":"name"},"arguments":[{"type":"literal_string","value":"a"},{"type":"literal_string","value":"b"}]},"orderBy":[{"expression":{"type":"column","name":"name"},"isAscending":true,"nullsLast":false}],"limit":{"numElements":4}}});
        let query = plan(&request, &columns()).unwrap();
        assert!(query.mongo.prefilter.is_some());
        assert!(query.mongo.filter.is_none());
        assert!(query.mongo.order_by.is_empty());
        assert!(query.mongo.limit.is_none());
        let sql = render_outer_sql("SELECT scan", &query, &columns()).unwrap();
        assert!(sql.contains("IN ('a', 'b')"));
        assert!(sql.contains("ORDER BY \"name\" ASC NULLS FIRST LIMIT 4"));
    }

    #[test]
    fn malformed_advertised_nodes_fail_without_partial_translation() {
        let cases = [
            serde_json::json!({}),
            serde_json::json!({"pushdownRequest":{"type":"delete"}}),
            serde_json::json!({"pushdownRequest":{"type":"select","selectList":[{"type":"literal_string","value":"x"}]}}),
            serde_json::json!({"pushdownRequest":{"type":"select","filter":{"type":"predicate_and","expressions":[]}}}),
            serde_json::json!({"pushdownRequest":{"type":"select","filter":{"type":"predicate_equal","left":{"type":"column","name":"missing"},"right":{"type":"literal_exactnumeric","value":1}}}}),
            serde_json::json!({"pushdownRequest":{"type":"select","filter":{"type":"predicate_in_constlist","expression":{"type":"column","name":"age"},"arguments":[]}}}),
            serde_json::json!({"pushdownRequest":{"type":"select","orderBy":[{"expression":{"type":"column","name":"age"}}]}}),
            serde_json::json!({"pushdownRequest":{"type":"select","limit":{"numElements":-1}}}),
        ];
        for case in cases {
            assert!(plan(&case, &columns()).is_err(), "{case}");
        }
    }
}
