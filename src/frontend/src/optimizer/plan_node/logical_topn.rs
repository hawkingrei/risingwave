// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt;

use fixedbitset::FixedBitSet;

use super::{ColPrunable, PlanBase, PlanNode, PlanRef, PlanTreeNodeUnary, ToBatch, ToStream};
use crate::optimizer::plan_node::LogicalProject;
use crate::optimizer::property::{FieldOrder, Order};
use crate::utils::ColIndexMapping;

/// `LogicalTopN` sorts the input data and fetches up to `limit` rows from `offset`
#[derive(Debug, Clone)]
pub struct LogicalTopN {
    pub base: PlanBase,
    input: PlanRef,
    limit: usize,
    offset: usize,
    order: Order,
}

impl LogicalTopN {
    fn new(input: PlanRef, limit: usize, offset: usize, order: Order) -> Self {
        let ctx = input.ctx();
        let schema = input.schema().clone();
        let pk_indices = input.pk_indices().to_vec();
        let base = PlanBase::new_logical(ctx, schema, pk_indices);
        LogicalTopN {
            base,
            input,
            limit,
            offset,
            order,
        }
    }

    /// the function will check if the cond is bool expression
    pub fn create(input: PlanRef, limit: usize, offset: usize, order: Order) -> PlanRef {
        Self::new(input, limit, offset, order).into()
    }
}

impl PlanTreeNodeUnary for LogicalTopN {
    fn input(&self) -> PlanRef {
        self.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        Self::new(input, self.limit, self.offset, self.order.clone())
    }

    #[must_use]
    fn rewrite_with_input(
        &self,
        input: PlanRef,
        input_col_change: ColIndexMapping,
    ) -> (Self, ColIndexMapping) {
        (
            Self::new(
                input,
                self.limit,
                self.offset,
                input_col_change
                    .rewrite_required_order(&self.order)
                    .unwrap(),
            ),
            input_col_change,
        )
    }
}
impl_plan_tree_node_for_unary! {LogicalTopN}
impl fmt::Display for LogicalTopN {
    fn fmt(&self, _f: &mut fmt::Formatter) -> fmt::Result {
        todo!()
    }
}

impl ColPrunable for LogicalTopN {
    fn prune_col(&self, required_cols: &FixedBitSet) -> PlanRef {
        self.must_contain_columns(required_cols);

        let mut input_required_cols = required_cols.clone();
        self.order
            .field_order
            .iter()
            .for_each(|fo| input_required_cols.insert(fo.index));

        let mapping = ColIndexMapping::with_remaining_columns(&input_required_cols);
        let new_order = Order {
            field_order: self
                .order
                .field_order
                .iter()
                .map(|fo| FieldOrder {
                    index: mapping.map(fo.index),
                    direct: fo.direct,
                })
                .collect(),
        };
        let new_input = self.input.prune_col(required_cols);
        let top_n = Self::new(new_input, self.limit, self.offset, new_order).into();

        if *required_cols == input_required_cols {
            top_n
        } else {
            let mut remaining_columns = FixedBitSet::with_capacity(top_n.schema().fields().len());
            remaining_columns.extend(required_cols.ones().map(|i| mapping.map(i)));
            LogicalProject::with_mapping(
                top_n,
                ColIndexMapping::with_remaining_columns(&remaining_columns),
            )
        }
    }
}

impl ToBatch for LogicalTopN {
    fn to_batch(&self) -> PlanRef {
        todo!()
    }
}

impl ToStream for LogicalTopN {
    fn to_stream(&self) -> PlanRef {
        todo!()
    }

    fn logical_rewrite_for_stream(&self) -> (PlanRef, ColIndexMapping) {
        let (input, input_col_change) = self.input.logical_rewrite_for_stream();
        let (top_n, out_col_change) = self.rewrite_with_input(input, input_col_change);
        (top_n.into(), out_col_change)
    }
}
