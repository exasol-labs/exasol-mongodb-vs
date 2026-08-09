//! MongoDB Virtual Schema.
//!
//! A single shared library exports exactly two Exasol entry points:
//! `MONGODB_ADAPTER` implements the Virtual Schema JSON protocol and
//! `MONGODB_SCAN` streams explicit JSON-table manifests through a dynamic
//! `EMITS` clause.

use exasol_udf_macros::exasol_udf;
use exasol_udf_sdk::context::UdfContext;
use exasol_udf_sdk::error::UdfError;

pub mod adapter;
pub mod connection;
pub mod discovery;
pub mod model;
pub mod mongo_plan;
pub mod pushdown;
pub mod scan;
pub mod wire;

/// Virtual Schema adapter entry point. Exasol invokes `adapter::adapter_call`
/// through the single-call adapter hook; this function body is unreachable.
#[exasol_udf(vs_adapter(adapter::adapter_call))]
fn mongodb_adapter(_ctx: &mut dyn UdfContext) -> Result<(), UdfError> {
    Ok(())
}

/// Scalar-emitting data-plane entry point. Dynamic output columns are supplied
/// by the adapter's generated `EMITS (...)` clause.
#[exasol_udf(name = "MONGODB_SCAN", input(spec: String))]
fn mongodb_scan(ctx: &mut dyn UdfContext) -> Result<(), UdfError> {
    scan::run_scan(ctx)
}
