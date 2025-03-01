use cairo_lang_defs::ids::ModuleItemId;
use cairo_lang_utils::extract_matches;
use pretty_assertions::assert_eq;
use test_log::test;

use crate::db::SemanticGroup;
use crate::test_utils::{setup_test_module, SemanticDatabaseForTesting};

// TODO(ilya): enable test once impls are enabled.
#[test]
fn test_impl() {
    let mut db_val = SemanticDatabaseForTesting::default();
    let db = &mut db_val;
    let (test_module, diagnostics) = setup_test_module(
        db,
        indoc::indoc! {"
            #[ABI]
            trait IContract {
                fn foo(a: felt);
            }


            #[Contract]
            impl Contract of IContract {
                fn foo(a: felt) {
                }
            }
        "},
    )
    .split();

    assert!(diagnostics.is_empty());

    let impl_id = extract_matches!(
        db.module_item_by_name(test_module.module_id, "Contract".into()).unwrap().unwrap(),
        ModuleItemId::Impl
    );

    assert_eq!(format!("{:?}", db.impl_generic_params(impl_id).unwrap()), "[]");

    let func_ids = db.impl_functions(impl_id).unwrap();
    assert_eq!(format!("{:?}", db.impl_functions(impl_id).unwrap()), "[ImplFunctionId(0)]");

    assert_eq!(
        format!("{:?}", db.impl_function_signature(func_ids[0]).unwrap()),
        "Signature { params: [Parameter { id: ParamId(1), name: \"a\", ty: TypeId(1), mutability: \
         Immutable }], return_type: TypeId(0), implicits: [], panicable: true }"
    );

    assert_eq!(format!("{:?}", db.impl_trait(impl_id).unwrap()), "ConcreteTraitId(0)");
}
