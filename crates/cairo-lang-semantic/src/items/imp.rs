use std::collections::HashSet;
use std::sync::Arc;
use std::vec;

use cairo_lang_defs::ids::{
    GenericFunctionId, GenericParamId, ImplFunctionId, ImplFunctionLongId, ImplId,
    LanguageElementId, ModuleId,
};
use cairo_lang_diagnostics::{
    skip_diagnostic, Diagnostics, DiagnosticsBuilder, Maybe, ToMaybe, ToOption,
};
use cairo_lang_proc_macros::DebugWithDb;
use cairo_lang_syntax as syntax;
use cairo_lang_syntax::node::ast::{self, Item, MaybeImplBody, OptionReturnTypeClause};
use cairo_lang_syntax::node::db::SyntaxGroup;
use cairo_lang_syntax::node::ids::SyntaxStablePtrId;
use cairo_lang_syntax::node::TypedSyntaxNode;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_lang_utils::unordered_hash_map::UnorderedHashMap;
use cairo_lang_utils::{define_short_id, extract_matches, try_extract_matches, OptionHelper};
use itertools::izip;

use super::attribute::{ast_attributes_to_semantic, Attribute};
use super::enm::SemanticEnumEx;
use super::function_with_body::{FunctionBody, FunctionBodyData, FunctionWithBodyDeclarationData};
use super::generics::semantic_generic_params;
use super::strct::SemanticStructEx;
use crate::corelib::{copy_trait, drop_trait, never_ty};
use crate::db::SemanticGroup;
use crate::diagnostic::SemanticDiagnosticKind::{self, *};
use crate::diagnostic::{NotFoundItemType, SemanticDiagnostics};
use crate::expr::compute::{compute_expr_block_semantic, ComputationContext, Environment};
use crate::resolve_path::{ResolvedConcreteItem, ResolvedGenericItem, ResolvedLookback, Resolver};
use crate::{
    semantic, ConcreteTraitId, ConcreteTraitLongId, Expr, FunctionId, GenericArgumentId,
    Mutability, SemanticDiagnostic, TypeId, TypeLongId,
};

#[cfg(test)]
#[path = "imp_test.rs"]
mod test;

#[derive(Clone, Debug, Hash, PartialEq, Eq, DebugWithDb)]
#[debug_db(dyn SemanticGroup + 'static)]
pub struct ConcreteImplLongId {
    pub impl_id: ImplId,
    pub generic_args: Vec<GenericArgumentId>,
}
define_short_id!(ConcreteImplId, ConcreteImplLongId, SemanticGroup, lookup_intern_concrete_impl);

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb)]
#[debug_db(dyn SemanticGroup + 'static)]
pub struct ImplDeclarationData {
    diagnostics: Diagnostics<SemanticDiagnostic>,
    generic_params: Vec<GenericParamId>,
    /// The concrete trait this impl implements, or Err if cannot be resolved.
    concrete_trait: Maybe<ConcreteTraitId>,
    attributes: Vec<Attribute>,
    resolved_lookback: Arc<ResolvedLookback>,
}

impl ImplDeclarationData {
    /// Returns Maybe::Err if a cycle is detected here.
    // TODO(orizi): Remove this function when cycle validation is not required through a type's
    // field.
    pub fn check_no_cycle(&self) -> Maybe<()> {
        self.concrete_trait?;
        Ok(())
    }
}

/// Query implementation of [crate::db::SemanticGroup::impl_semantic_declaration_diagnostics].
pub fn impl_semantic_declaration_diagnostics(
    db: &dyn SemanticGroup,
    impl_id: ImplId,
) -> Diagnostics<SemanticDiagnostic> {
    db.priv_impl_declaration_data(impl_id).map(|data| data.diagnostics).unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_generic_params].
pub fn impl_generic_params(db: &dyn SemanticGroup, impl_id: ImplId) -> Maybe<Vec<GenericParamId>> {
    Ok(db.priv_impl_declaration_data(impl_id)?.generic_params)
}

/// Query implementation of [crate::db::SemanticGroup::impl_resolved_lookback].
pub fn impl_resolved_lookback(
    db: &dyn SemanticGroup,
    impl_id: ImplId,
) -> Maybe<Arc<ResolvedLookback>> {
    Ok(db.priv_impl_declaration_data(impl_id)?.resolved_lookback)
}

/// Query implementation of [crate::db::SemanticGroup::impl_trait].
pub fn impl_trait(db: &dyn SemanticGroup, impl_id: ImplId) -> Maybe<ConcreteTraitId> {
    db.priv_impl_declaration_data(impl_id)?.concrete_trait
}

/// Cycle handling for [crate::db::SemanticGroup::priv_impl_declaration_data].
pub fn priv_impl_declaration_data_cycle(
    db: &dyn SemanticGroup,
    _cycle: &[String],
    impl_id: &ImplId,
) -> Maybe<ImplDeclarationData> {
    priv_impl_declaration_data_inner(db, *impl_id, false)
}

/// Query implementation of [crate::db::SemanticGroup::priv_impl_declaration_data].
pub fn priv_impl_declaration_data(
    db: &dyn SemanticGroup,
    impl_id: ImplId,
) -> Maybe<ImplDeclarationData> {
    priv_impl_declaration_data_inner(db, impl_id, true)
}

/// Shared code for the query and cycle handling.
/// The cycle handling logic needs to pass resolve_trait=false to prevent the cycle.
pub fn priv_impl_declaration_data_inner(
    db: &dyn SemanticGroup,
    impl_id: ImplId,
    resolve_trait: bool,
) -> Maybe<ImplDeclarationData> {
    let module_file_id = impl_id.module_file(db.upcast());
    let mut diagnostics = SemanticDiagnostics::new(module_file_id);

    // TODO(spapini): when code changes in a file, all the AST items change (as they contain a path
    // to the green root that changes. Once ASTs are rooted on items, use a selector that picks only
    // the item instead of all the module data.
    let module_impls = db.module_impls(module_file_id.0)?;
    let syntax_db = db.upcast();
    let impl_ast = module_impls.get(&impl_id).to_maybe()?;

    // Generic params.
    let generic_params = semantic_generic_params(
        db,
        &mut diagnostics,
        module_file_id,
        &impl_ast.generic_params(syntax_db),
    );

    let trait_path_syntax = impl_ast.trait_path(syntax_db);
    let mut resolver = Resolver::new(db, module_file_id, &generic_params);

    let concrete_trait = if resolve_trait {
        resolver
            .resolve_concrete_path(&mut diagnostics, &trait_path_syntax, NotFoundItemType::Trait)
            .ok()
            .and_then(|concrete_item| {
                try_extract_matches!(concrete_item, ResolvedConcreteItem::Trait)
            })
    } else {
        None
    }
    .ok_or_else(|| diagnostics.report(&trait_path_syntax, NotATrait));

    let attributes = ast_attributes_to_semantic(syntax_db, impl_ast.attributes(syntax_db));
    let resolved_lookback = Arc::new(resolver.lookback);
    Ok(ImplDeclarationData {
        diagnostics: diagnostics.build(),
        generic_params,
        concrete_trait,
        attributes,
        resolved_lookback,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb)]
#[debug_db(dyn SemanticGroup + 'static)]
pub struct ImplDefinitionData {
    diagnostics: Diagnostics<SemanticDiagnostic>,
    function_asts: OrderedHashMap<ImplFunctionId, ast::ItemFreeFunction>,
}

/// Query implementation of [crate::db::SemanticGroup::impl_semantic_definition_diagnostics].
pub fn impl_semantic_definition_diagnostics(
    db: &dyn SemanticGroup,
    impl_id: ImplId,
) -> Diagnostics<SemanticDiagnostic> {
    let mut diagnostics = DiagnosticsBuilder::default();

    let Ok(data) = db.priv_impl_definition_data(impl_id) else {
        return Diagnostics::default();
    };

    diagnostics.extend(data.diagnostics);
    for impl_function_id in data.function_asts.keys() {
        diagnostics.extend(db.impl_function_declaration_diagnostics(*impl_function_id));
        diagnostics.extend(db.impl_function_body_diagnostics(*impl_function_id));
    }

    diagnostics.build()
}

/// An helper function to report diagnostics in priv_impl_definition_data.
fn report_invalid_in_impl<Terminal: syntax::node::Terminal>(
    syntax_db: &dyn SyntaxGroup,
    diagnostics: &mut SemanticDiagnostics,
    kw_terminal: Terminal,
) {
    diagnostics.report_by_ptr(
        kw_terminal.as_syntax_node().stable_ptr(),
        InvalidImplItem { item_kw: kw_terminal.text(syntax_db) },
    );
}

/// Query implementation of [crate::db::SemanticGroup::priv_impl_definition_data].
pub fn priv_impl_definition_data(
    db: &dyn SemanticGroup,
    impl_id: ImplId,
) -> Maybe<ImplDefinitionData> {
    let module_file_id = impl_id.module_file(db.upcast());
    let mut diagnostics = SemanticDiagnostics::new(module_file_id);

    let declaration_data = db.priv_impl_declaration_data(impl_id)?;
    let concrete_trait = declaration_data.concrete_trait?;

    let module_impls = db.module_impls(module_file_id.0)?;
    let impl_ast = module_impls.get(&impl_id).to_maybe()?;
    let syntax_db = db.upcast();

    let lookup_context = ImplLookupContext {
        module_id: module_file_id.0,
        extra_modules: vec![],
        generic_params: declaration_data.generic_params,
    };
    check_special_impls(
        db,
        &mut diagnostics,
        lookup_context,
        concrete_trait,
        impl_ast.stable_ptr().untyped(),
    )
    // Ignore the result.
    .ok();

    let mut function_asts = OrderedHashMap::default();

    if let MaybeImplBody::Some(body) = impl_ast.body(syntax_db) {
        for item in body.items(syntax_db).elements(syntax_db) {
            match item {
                Item::Constant(constant) => report_invalid_in_impl(
                    syntax_db,
                    &mut diagnostics,
                    constant.const_kw(syntax_db),
                ),
                Item::Module(module) => {
                    report_invalid_in_impl(syntax_db, &mut diagnostics, module.module_kw(syntax_db))
                }

                Item::Use(use_item) => {
                    report_invalid_in_impl(syntax_db, &mut diagnostics, use_item.use_kw(syntax_db))
                }
                Item::ExternFunction(extern_func) => report_invalid_in_impl(
                    syntax_db,
                    &mut diagnostics,
                    extern_func.extern_kw(syntax_db),
                ),
                Item::ExternType(extern_type) => report_invalid_in_impl(
                    syntax_db,
                    &mut diagnostics,
                    extern_type.extern_kw(syntax_db),
                ),
                Item::Trait(trt) => {
                    report_invalid_in_impl(syntax_db, &mut diagnostics, trt.trait_kw(syntax_db))
                }
                Item::Impl(imp) => {
                    report_invalid_in_impl(syntax_db, &mut diagnostics, imp.impl_kw(syntax_db))
                }
                Item::Struct(strct) => {
                    report_invalid_in_impl(syntax_db, &mut diagnostics, strct.struct_kw(syntax_db))
                }
                Item::Enum(enm) => {
                    report_invalid_in_impl(syntax_db, &mut diagnostics, enm.enum_kw(syntax_db))
                }
                Item::TypeAlias(ty) => {
                    report_invalid_in_impl(syntax_db, &mut diagnostics, ty.type_kw(syntax_db))
                }
                Item::FreeFunction(func) => {
                    let impl_function_id = db.intern_impl_function(ImplFunctionLongId(
                        module_file_id,
                        func.stable_ptr(),
                    ));
                    function_asts.insert(impl_function_id, func);
                }
            }
        }
    }

    Ok(ImplDefinitionData { diagnostics: diagnostics.build(), function_asts })
}

/// Query implementation of [crate::db::SemanticGroup::impl_functions].
pub fn impl_functions(db: &dyn SemanticGroup, impl_id: ImplId) -> Maybe<Vec<ImplFunctionId>> {
    Ok(db.priv_impl_definition_data(impl_id)?.function_asts.keys().copied().collect())
}

/// Handle special cases such as Copy and Drop checking.
fn check_special_impls(
    db: &dyn SemanticGroup,
    diagnostics: &mut SemanticDiagnostics,
    lookup_context: ImplLookupContext,
    concrete_trait: ConcreteTraitId,
    stable_ptr: SyntaxStablePtrId,
) -> Maybe<()> {
    let ConcreteTraitLongId { trait_id, generic_args } =
        db.lookup_intern_concrete_trait(concrete_trait);
    let copy = copy_trait(db);
    let drop = drop_trait(db);

    if trait_id == copy {
        let tys = get_inner_types(db, extract_matches!(generic_args[0], GenericArgumentId::Type))?;
        if !tys
            .into_iter()
            .filter_map(|ty| db.type_info(lookup_context.clone(), ty).to_option())
            .all(|info| info.duplicatable)
        {
            return Err(diagnostics.report_by_ptr(stable_ptr, InvalidCopyTraitImpl));
        }
    }
    if trait_id == drop {
        let tys = get_inner_types(db, extract_matches!(generic_args[0], GenericArgumentId::Type))?;
        if !tys
            .into_iter()
            .filter_map(|ty| db.type_info(lookup_context.clone(), ty).to_option())
            .all(|info| info.droppable)
        {
            return Err(diagnostics.report_by_ptr(stable_ptr, InvalidDropTraitImpl));
        }
    }

    Ok(())
}

/// Retrieves all the inner types (members of a struct / tuple or variants of an enum).
/// These are the types that are required to implement some trait,
/// in order for the original type to be able to implement this trait.
fn get_inner_types(db: &dyn SemanticGroup, ty: TypeId) -> Maybe<Vec<TypeId>> {
    Ok(match db.lookup_intern_type(ty) {
        TypeLongId::Concrete(concrete_type_id) => {
            // Look for Copy and Drop trait in the defining module.
            match concrete_type_id {
                crate::ConcreteTypeId::Struct(concrete_struct_id) => db
                    .concrete_struct_members(concrete_struct_id)?
                    .values()
                    .map(|member| member.ty)
                    .collect(),
                crate::ConcreteTypeId::Enum(concrete_enum_id) => db
                    .concrete_enum_variants(concrete_enum_id)?
                    .into_iter()
                    .map(|variant| variant.ty)
                    .collect(),
                crate::ConcreteTypeId::Extern(_) => vec![],
            }
        }
        TypeLongId::Tuple(tys) => tys,
        TypeLongId::GenericParameter(_) => {
            return Err(skip_diagnostic());
        }
        TypeLongId::Missing(diag_added) => {
            return Err(diag_added);
        }
    })
}

/// Query implementation of [crate::db::SemanticGroup::find_impls_at_module].
pub fn find_impls_at_module(
    db: &dyn SemanticGroup,
    module_id: ModuleId,
    concrete_trait_id: ConcreteTraitId,
) -> Maybe<Vec<ConcreteImplId>> {
    let mut res = Vec::new();
    let impls = db.module_impls(module_id)?;
    // TODO(spapini): Index better.
    for impl_id in impls.keys().copied() {
        let Ok(imp_data)= db.priv_impl_declaration_data(impl_id) else {continue};
        if !imp_data.generic_params.is_empty() {
            // TODO(spapini): Infer generics and substitute.
            continue;
        }

        if imp_data.concrete_trait == Ok(concrete_trait_id) {
            let concrete_impl_id =
                db.intern_concrete_impl(ConcreteImplLongId { impl_id, generic_args: vec![] });
            res.push(concrete_impl_id);
        }
    }
    Ok(res)
}

#[allow(dead_code)]
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ImplLookupContext {
    pub module_id: ModuleId,
    pub extra_modules: Vec<ModuleId>,
    pub generic_params: Vec<GenericParamId>,
}

/// Finds all the implementations for a concrete trait, in a specific lookup context.
pub fn find_impls_at_context(
    db: &dyn SemanticGroup,
    lookup_context: &ImplLookupContext,
    concrete_trait_id: ConcreteTraitId,
) -> Maybe<Vec<ConcreteImplId>> {
    let mut res = Vec::new();
    // TODO(spapini): Lookup in generic_params once impl generic params are supported.
    res.extend(find_impls_at_module(db, lookup_context.module_id, concrete_trait_id)?);
    for module_id in &lookup_context.extra_modules {
        if let Ok(imps) = find_impls_at_module(db, *module_id, concrete_trait_id) {
            res.extend(imps);
        }
    }
    for submodule in db.module_submodules_ids(lookup_context.module_id)? {
        res.extend(find_impls_at_module(db, ModuleId::Submodule(submodule), concrete_trait_id)?);
    }
    for use_id in db.module_uses_ids(lookup_context.module_id)? {
        if let Ok(ResolvedGenericItem::Module(submodule)) = db.use_resolved_item(use_id) {
            res.extend(find_impls_at_module(db, submodule, concrete_trait_id)?);
        }
    }
    Ok(res)
}

// === Declaration ===

/// Query implementation of [crate::db::SemanticGroup::impl_function_signature].
pub fn impl_function_signature(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Maybe<semantic::Signature> {
    Ok(db.priv_impl_function_declaration_data(impl_function_id)?.signature)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_declaration_implicits].
pub fn impl_function_declaration_implicits(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Maybe<Vec<TypeId>> {
    Ok(db.priv_impl_function_declaration_data(impl_function_id)?.signature.implicits)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_generic_params].
pub fn impl_function_generic_params(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Maybe<Vec<GenericParamId>> {
    Ok(db.priv_impl_function_declaration_data(impl_function_id)?.generic_params)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_declaration_diagnostics].
pub fn impl_function_declaration_diagnostics(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Diagnostics<SemanticDiagnostic> {
    db.priv_impl_function_declaration_data(impl_function_id)
        .map(|data| data.diagnostics)
        .unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_resolved_lookback].
pub fn impl_function_resolved_lookback(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Maybe<Arc<ResolvedLookback>> {
    Ok(db.priv_impl_function_declaration_data(impl_function_id)?.resolved_lookback)
}

/// Query implementation of [crate::db::SemanticGroup::priv_impl_function_declaration_data].
pub fn priv_impl_function_declaration_data(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Maybe<FunctionWithBodyDeclarationData> {
    let module_file_id = impl_function_id.module_file(db.upcast());
    let mut diagnostics = SemanticDiagnostics::new(module_file_id);
    let impl_id = impl_function_id.impl_id(db.upcast());
    let data = db.priv_impl_definition_data(impl_id)?;
    let function_syntax = &data.function_asts[impl_function_id];
    let syntax_db = db.upcast();
    let declaration = function_syntax.declaration(syntax_db);
    let generic_params = semantic_generic_params(
        db,
        &mut diagnostics,
        module_file_id,
        &declaration.generic_params(syntax_db),
    );
    let mut resolver = Resolver::new(db, module_file_id, &generic_params);

    let signature_syntax = declaration.signature(syntax_db);

    let mut environment = Environment::default();
    let signature = semantic::Signature::from_ast(
        &mut diagnostics,
        db,
        &mut resolver,
        &signature_syntax,
        GenericFunctionId::ImplFunction(impl_function_id),
        &mut environment,
    );

    validate_impl_function_signature(
        db,
        &mut diagnostics,
        impl_id,
        impl_function_id,
        &signature_syntax,
        &signature,
        function_syntax,
    );

    let attributes = ast_attributes_to_semantic(syntax_db, function_syntax.attributes(syntax_db));
    let resolved_lookback = Arc::new(resolver.lookback);

    Ok(FunctionWithBodyDeclarationData {
        diagnostics: diagnostics.build(),
        signature,
        generic_params,
        environment,
        attributes,
        resolved_lookback,
    })
}

fn validate_impl_function_signature(
    db: &dyn SemanticGroup,
    diagnostics: &mut SemanticDiagnostics,
    impl_id: ImplId,
    impl_function_id: ImplFunctionId,
    signature_syntax: &ast::FunctionSignature,
    signature: &semantic::Signature,
    function_syntax: &ast::ItemFreeFunction,
) {
    let syntax_db = db.upcast();
    let Ok(declaraton_data) = db.priv_impl_declaration_data(impl_id) else {
        return;
    };
    let Ok(concrete_trait) = declaraton_data.concrete_trait else {
        return;
    };
    let concrete_trait_long_id = db.lookup_intern_concrete_trait(concrete_trait);
    let trait_id = concrete_trait_long_id.trait_id;
    let Ok(trait_functions) = db.trait_functions(trait_id) else {
        return;
    };
    let function_name = db.lookup_intern_impl_function(impl_function_id).name(db.upcast());
    let Some(trait_function_id) = trait_functions.get(&function_name).on_none(|| {
        diagnostics.report(function_syntax, FunctionNotMemberOfTrait { impl_id, impl_function_id, trait_id });
    }) else {
        return;
    };
    let Ok(trait_signature) = db.trait_function_signature(*trait_function_id) else {
        return;
    };
    if signature.params.len() != trait_signature.params.len() {
        diagnostics.report(
            &signature_syntax.parameters(syntax_db),
            WrongNumberOfParameters {
                impl_id,
                impl_function_id,
                trait_id,
                expected: trait_signature.params.len(),
                actual: signature.params.len(),
            },
        );
    }
    for (idx, (param, trait_param)) in
        izip!(signature.params.iter(), trait_signature.params.iter()).enumerate()
    {
        let expected_ty = trait_param.ty;
        let actual_ty = param.ty;

        if expected_ty != actual_ty {
            diagnostics.report(
                &signature_syntax.parameters(syntax_db).elements(syntax_db)[idx]
                    .type_clause(syntax_db)
                    .ty(syntax_db),
                WrongParameterType { impl_id, impl_function_id, trait_id, expected_ty, actual_ty },
            );
        }

        if trait_param.mutability != param.mutability {
            if trait_param.mutability == Mutability::Reference {
                diagnostics.report(
                    &signature_syntax.parameters(syntax_db).elements(syntax_db)[idx]
                        .modifiers(syntax_db),
                    ParamaterShouldBeReference { impl_id, impl_function_id, trait_id },
                );
            }

            if param.mutability == Mutability::Reference {
                diagnostics.report(
                    &signature_syntax.parameters(syntax_db).elements(syntax_db)[idx]
                        .modifiers(syntax_db),
                    ParamaterShouldNotBeReference { impl_id, impl_function_id, trait_id },
                );
            }
        }
    }

    if !trait_signature.panicable && signature.panicable {
        diagnostics.report(signature_syntax, PassPanicAsNopanic { impl_function_id, trait_id });
    }

    let expected_ty = trait_signature.return_type;
    let actual_ty = signature.return_type;
    if expected_ty != actual_ty {
        let location_ptr = match signature_syntax.ret_ty(syntax_db) {
            OptionReturnTypeClause::ReturnTypeClause(ret_ty) => {
                ret_ty.ty(syntax_db).as_syntax_node()
            }
            OptionReturnTypeClause::Empty(_) => {
                function_syntax.body(syntax_db).lbrace(syntax_db).as_syntax_node()
            }
        }
        .stable_ptr();
        diagnostics.report_by_ptr(
            location_ptr,
            WrongReturnTypeForImpl { impl_id, impl_function_id, trait_id, expected_ty, actual_ty },
        );
    }
}

// === Body ===

// --- Selectors ---

/// Query implementation of [crate::db::SemanticGroup::impl_function_body_diagnostics].
pub fn impl_function_body_diagnostics(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Diagnostics<SemanticDiagnostic> {
    db.priv_impl_function_body_data(impl_function_id)
        .map(|data| data.diagnostics)
        .unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_body].
pub fn impl_function_body(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Maybe<Arc<FunctionBody>> {
    Ok(db.priv_impl_function_body_data(impl_function_id)?.body)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_body_resolved_lookback].
pub fn impl_function_body_resolved_lookback(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Maybe<Arc<ResolvedLookback>> {
    Ok(db.priv_impl_function_body_data(impl_function_id)?.resolved_lookback)
}

// --- Computation ---

/// Query implementation of [crate::db::SemanticGroup::priv_impl_function_body_data].
pub fn priv_impl_function_body_data(
    db: &dyn SemanticGroup,
    impl_function_id: ImplFunctionId,
) -> Maybe<FunctionBodyData> {
    let defs_db = db.upcast();
    let module_file_id = impl_function_id.module_file(defs_db);
    let mut diagnostics = SemanticDiagnostics::new(module_file_id);
    let impl_id = impl_function_id.impl_id(defs_db);
    let data = db.priv_impl_definition_data(impl_id)?;
    let function_syntax = &data.function_asts[impl_function_id];
    // Compute declaration semantic.
    let declaration = db.priv_impl_function_declaration_data(impl_function_id)?;
    let resolver = Resolver::new(db, module_file_id, &declaration.generic_params);
    let environment = declaration.environment;
    // Compute body semantic expr.
    let mut ctx = ComputationContext::new(
        db,
        &mut diagnostics,
        resolver,
        &declaration.signature,
        environment,
    );
    let function_body = function_syntax.body(db.upcast());
    let expr = compute_expr_block_semantic(&mut ctx, &function_body)?;
    let expr_ty = expr.ty();
    let return_type = declaration.signature.return_type;
    if expr_ty != return_type
        && !expr_ty.is_missing(db)
        && !return_type.is_missing(db)
        && expr_ty != never_ty(db)
    {
        ctx.diagnostics.report(
            &function_body,
            SemanticDiagnosticKind::WrongReturnType {
                expected_ty: return_type,
                actual_ty: expr_ty,
            },
        );
    }
    let body_expr = ctx.exprs.alloc(expr);
    let ComputationContext { exprs, statements, resolver, .. } = ctx;

    let direct_callees: HashSet<FunctionId> = exprs
        .iter()
        .filter_map(|(_id, expr)| try_extract_matches!(expr, Expr::FunctionCall))
        .map(|f| f.function)
        .collect();

    let expr_lookup: UnorderedHashMap<_, _> =
        exprs.iter().map(|(expr_id, expr)| (expr.stable_ptr(), expr_id)).collect();
    let resolved_lookback = Arc::new(resolver.lookback);
    Ok(FunctionBodyData {
        diagnostics: diagnostics.build(),
        expr_lookup,
        resolved_lookback,
        body: Arc::new(FunctionBody {
            exprs,
            statements,
            body_expr,
            direct_callees: direct_callees.into_iter().collect(),
        }),
    })
}
