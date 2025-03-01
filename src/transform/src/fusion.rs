// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Transformations that fuse together others of their kind.

pub mod filter;
pub mod join;
pub mod map;
pub mod negate;
pub mod project;
pub mod reduce;
pub mod top_k;
pub mod union;

use mz_expr::MirRelationExpr;

use crate::TransformCtx;

/// Fuses multiple like operators together when possible.
#[derive(Debug)]
pub struct Fusion;

impl crate::Transform for Fusion {
    fn name(&self) -> &'static str {
        "Fusion"
    }

    #[mz_ore::instrument(
        target = "optimizer",
        level = "debug",
        fields(path.segment = "fusion")
    )]
    fn actually_perform_transform(
        &self,
        relation: &mut MirRelationExpr,
        _: &mut TransformCtx,
    ) -> Result<(), crate::TransformError> {
        use mz_expr::visit::Visit;
        relation.visit_mut_post(&mut Self::action)?;
        mz_repr::explain::trace_plan(&*relation);
        Ok(())
    }
}

impl Fusion {
    /// Apply fusion action for variants we know how to fuse.
    pub(crate) fn action(expr: &mut MirRelationExpr) {
        match expr {
            MirRelationExpr::Filter { .. } => filter::Filter::action(expr),
            MirRelationExpr::Map { .. } => map::Map::action(expr),
            MirRelationExpr::Project { .. } => project::Project::action(expr),
            MirRelationExpr::Negate { .. } => negate::Negate::action(expr),
            MirRelationExpr::TopK { .. } => top_k::TopK::action(expr),
            MirRelationExpr::Union { .. } => union::Union::action(expr),
            _ => {}
        }
    }
}
