use std::collections::HashMap;

use cairo_lang_defs::ids::ModuleFileId;
use cairo_lang_syntax::node::ids::SyntaxStablePtrId;
use itertools::zip_eq;

pub use self::demand::Demand;
use crate::diagnostic::LoweringDiagnosticKind::*;
use crate::diagnostic::LoweringDiagnostics;
use crate::{
    BlockId, FlatBlockEnd, FlatLowered, Statement, StatementMatchEnum, StatementMatchExtern,
    VariableId,
};

mod demand;

/// Borrowchecking uses sequential flow block. Every branching statement splits the block.
/// When lowering uses Goto, the lowering will also use these kind of blocks, and we won't have
/// this indirection.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RealBlock(BlockId, usize);

/// Information about the callsite of the current block. When lowering uses Goto, this won't be
/// necessary.
#[derive(Clone)]
pub struct CallsiteInfo<'a> {
    return_site: RealBlock,
    call_output_vars: Vec<VariableId>,
    parent: Option<&'a CallsiteInfo<'a>>,
}

pub struct BorrowChecker<'a> {
    diagnostics: &'a mut LoweringDiagnostics,
    lowered: &'a FlatLowered,
    cache: HashMap<RealBlock, Demand>,
    // TODO(spapini): Remove when variables carry their stable pointers.
    dummy_stable_ptr: SyntaxStablePtrId,
}

impl<'a> BorrowChecker<'a> {
    /// Computes the variables [Demand] from the beginning of a [RealBlock], while outputting borrow
    /// checking diagnostics.
    pub fn get_demand(
        &mut self,
        callsite_info: Option<CallsiteInfo<'_>>,
        block: RealBlock,
    ) -> Demand {
        if let Some(cached_result) = self.cache.get(&block) {
            return cached_result.clone();
        }
        let RealBlock(block_id, stmt_offset) = block;

        // Find real block ending.
        // This indirection and find_block_forwards_demand() will removed when lowering is using
        // Gotos.
        let (real_block_end_offset, mut demand) = self
            .get_demand_from_next_split(block_id, stmt_offset, callsite_info.clone())
            .unwrap_or_else(|| {
                // No branching statement was found, and the RealBlock continues until BlockEnd.
                let demand =
                    self.get_block_end_demand(&self.lowered.blocks[block_id].end, callsite_info);
                (self.lowered.blocks[block_id].statements.len(), demand)
            });

        // Go through statements backwards, and update demand.
        for stmt in self.lowered.blocks[block_id].statements[stmt_offset..real_block_end_offset]
            .iter()
            .rev()
        {
            demand.variables_introduced(self, &stmt.outputs());
            demand.variables_used(self, &stmt.inputs());
        }

        if stmt_offset == 0 {
            // Update block inputs.
            demand.variables_introduced(self, &self.lowered.blocks[block_id].inputs);
        }

        // Cache result.
        self.cache.insert(block, demand.clone());
        demand
    }

    /// Computes the variables [Demand] from a [FlatBlockEnd], while outputting borrow checking
    /// diagnostics.
    fn get_block_end_demand(
        &mut self,
        block_end: &FlatBlockEnd,
        callsite_info: Option<CallsiteInfo<'_>>,
    ) -> Demand {
        let demand = match block_end {
            FlatBlockEnd::Callsite(vars) => {
                let callsite_info = callsite_info.unwrap();
                let mut demand =
                    self.get_demand(callsite_info.parent.cloned(), callsite_info.return_site);
                for (var, callsite_var) in zip_eq(vars, &callsite_info.call_output_vars) {
                    if demand.vars.swap_remove(callsite_var) {
                        demand.vars.insert(*var);
                    }
                }
                demand
            }
            FlatBlockEnd::Return(vars) => Demand { vars: vars.iter().copied().collect() },
            FlatBlockEnd::Unreachable => Demand::default(),
        };
        demand
    }

    // Note: When lowering uses Gotos, this will be merged with get_block_end_demand().
    /// Computes the variables [Demand] from the next branching statement in a block.
    /// A [RealBlock] ends in either a branching statement (e.g. match) or a [FlatBlockEnd].
    /// If such a statement was found, returns its index and the [Demand] from that point.
    /// Otherwise, returns None.
    fn get_demand_from_next_split(
        &mut self,
        block_id: BlockId,
        stmt_offset: usize,
        callsite_info: Option<CallsiteInfo<'_>>,
    ) -> Option<(usize, Demand)> {
        for (i, stmt) in self.lowered.blocks[block_id].statements[stmt_offset..].iter().enumerate()
        {
            // Closure that creates a new CallsiteInfo struct for a branching statement.
            // Will be removed when lowering uses Gotos.
            let new_callsite = |outputs: &Vec<VariableId>| -> Option<CallsiteInfo<'_>> {
                Some(CallsiteInfo {
                    return_site: RealBlock(block_id, stmt_offset + i + 1),
                    call_output_vars: outputs.clone(),
                    parent: callsite_info.as_ref(),
                })
            };

            let demand = match stmt {
                Statement::CallBlock(stmt) => {
                    self.get_demand(new_callsite(&stmt.outputs), RealBlock(stmt.block, 0))
                }
                Statement::MatchExtern(StatementMatchExtern { arms, outputs, .. })
                | Statement::MatchEnum(StatementMatchEnum { arms, outputs, .. }) => {
                    let arm_demands = arms
                        .iter()
                        .map(|(_, arm_block)| {
                            self.get_demand(new_callsite(outputs), RealBlock(*arm_block, 0))
                        })
                        .collect();
                    self.merge_demands(arm_demands)
                }
                Statement::Literal(_)
                | Statement::Call(_)
                | Statement::StructConstruct(_)
                | Statement::StructDestructure(_)
                | Statement::EnumConstruct(_) => continue,
            };
            return Some((stmt_offset + i, demand));
        }
        None
    }

    /// Merges [Demand]s from multiple branches into one, reporting diagnostics in the way.
    fn merge_demands(&mut self, arm_demands: Vec<Demand>) -> Demand {
        // Union demands.
        let mut demand = Demand::default();
        for arm_demand in &arm_demands {
            demand.vars.extend(arm_demand.vars.iter().copied());
        }
        // Check each var.
        for var in demand.vars.iter() {
            for arm_demand in &arm_demands {
                if !arm_demand.vars.contains(var) {
                    // Variable demanded only on some branches. It should be dropped in other.
                    // If it's not drop, that is an issue.
                    // Currently disabled, since Drop is not properly implemented everywhere.
                    #[allow(clippy::overly_complex_bool_expr)]
                    if false && !self.lowered.variables[*var].droppable {
                        self.diagnostics.report(self.dummy_stable_ptr, VariableNotDropped);
                    }
                    // Report only once per variable.
                    break;
                }
            }
        }
        demand
    }
}

/// Report borrow checking diagnostics.
pub fn borrow_check(
    module_file_id: ModuleFileId,
    dummy_stable_ptr: SyntaxStablePtrId,
    lowered: &mut FlatLowered,
) {
    let mut diagnostics = LoweringDiagnostics::new(module_file_id);
    diagnostics.diagnostics.extend(std::mem::take(&mut lowered.diagnostics));

    if let Ok(root) = lowered.root {
        let mut checker = BorrowChecker {
            diagnostics: &mut diagnostics,
            lowered,
            cache: Default::default(),
            dummy_stable_ptr,
        };
        let root_demand = checker.get_demand(None, RealBlock(root, 0));
        assert!(root_demand.vars.is_empty(), "Undefined variable should not happen at this stage");
    }

    lowered.diagnostics = diagnostics.build();
}
