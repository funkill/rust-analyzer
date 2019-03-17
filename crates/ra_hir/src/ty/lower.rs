//! Methods for lowering the HIR to types. There are two main cases here:
//!
//!  - Lowering a type reference like `&usize` or `Option<foo::bar::Baz>` to a
//!    type: The entry point for this is `Ty::from_hir`.
//!  - Building the type for an item: This happens through the `type_for_def` query.
//!
//! This usually involves resolving names, collecting generic arguments etc.

use std::sync::Arc;

use crate::{
    Function, Struct, StructField, Enum, EnumVariant, Path,
    ModuleDef, TypeAlias,
    Const, Static,
    HirDatabase,
    type_ref::TypeRef,
    name::KnownName,
    nameres::Namespace,
    resolve::{Resolver, Resolution},
    path::{ PathSegment, GenericArg},
    generics::GenericParams,
    adt::VariantDef,
};
use super::{Ty, primitive, FnSig, Substs};

impl Ty {
    pub(crate) fn from_hir(db: &impl HirDatabase, resolver: &Resolver, type_ref: &TypeRef) -> Self {
        match type_ref {
            TypeRef::Never => Ty::Never,
            TypeRef::Tuple(inner) => {
                let inner_tys =
                    inner.iter().map(|tr| Ty::from_hir(db, resolver, tr)).collect::<Vec<_>>();
                Ty::Tuple(inner_tys.into())
            }
            TypeRef::Path(path) => Ty::from_hir_path(db, resolver, path),
            TypeRef::RawPtr(inner, mutability) => {
                let inner_ty = Ty::from_hir(db, resolver, inner);
                Ty::RawPtr(Arc::new(inner_ty), *mutability)
            }
            TypeRef::Array(inner) => {
                let inner_ty = Ty::from_hir(db, resolver, inner);
                Ty::Array(Arc::new(inner_ty))
            }
            TypeRef::Slice(inner) => {
                let inner_ty = Ty::from_hir(db, resolver, inner);
                Ty::Slice(Arc::new(inner_ty))
            }
            TypeRef::Reference(inner, mutability) => {
                let inner_ty = Ty::from_hir(db, resolver, inner);
                Ty::Ref(Arc::new(inner_ty), *mutability)
            }
            TypeRef::Placeholder => Ty::Unknown,
            TypeRef::Fn(params) => {
                let inner_tys =
                    params.iter().map(|tr| Ty::from_hir(db, resolver, tr)).collect::<Vec<_>>();
                let sig = FnSig { params_and_return: inner_tys.into() };
                Ty::FnPtr(sig)
            }
            TypeRef::Error => Ty::Unknown,
        }
    }

    pub(crate) fn from_hir_path(db: &impl HirDatabase, resolver: &Resolver, path: &Path) -> Self {
        if let Some(name) = path.as_ident() {
            // FIXME handle primitive type names in resolver as well?
            if let Some(int_ty) = primitive::UncertainIntTy::from_name(name) {
                return Ty::Int(int_ty);
            } else if let Some(float_ty) = primitive::UncertainFloatTy::from_name(name) {
                return Ty::Float(float_ty);
            } else if let Some(known) = name.as_known_name() {
                match known {
                    KnownName::Bool => return Ty::Bool,
                    KnownName::Char => return Ty::Char,
                    KnownName::Str => return Ty::Str,
                    _ => {}
                }
            }
        }

        // Resolve the path (in type namespace)
        let resolution = resolver.resolve_path(db, path).take_types();

        let def = match resolution {
            Some(Resolution::Def(def)) => def,
            Some(Resolution::LocalBinding(..)) => {
                // this should never happen
                panic!("path resolved to local binding in type ns");
            }
            Some(Resolution::GenericParam(idx)) => {
                return Ty::Param {
                    idx,
                    // FIXME: maybe return name in resolution?
                    name: path
                        .as_ident()
                        .expect("generic param should be single-segment path")
                        .clone(),
                };
            }
            Some(Resolution::SelfType(impl_block)) => {
                return impl_block.target_ty(db);
            }
            None => return Ty::Unknown,
        };

        let typable: TypableDef = match def.into() {
            None => return Ty::Unknown,
            Some(it) => it,
        };
        let ty = db.type_for_def(typable, Namespace::Types);
        let substs = Ty::substs_from_path(db, resolver, path, typable);
        ty.subst(&substs)
    }

    pub(super) fn substs_from_path_segment(
        db: &impl HirDatabase,
        resolver: &Resolver,
        segment: &PathSegment,
        resolved: TypableDef,
    ) -> Substs {
        let mut substs = Vec::new();
        let def_generics = match resolved {
            TypableDef::Function(func) => func.generic_params(db),
            TypableDef::Struct(s) => s.generic_params(db),
            TypableDef::Enum(e) => e.generic_params(db),
            TypableDef::EnumVariant(var) => var.parent_enum(db).generic_params(db),
            TypableDef::TypeAlias(t) => t.generic_params(db),
            TypableDef::Const(_) | TypableDef::Static(_) => GenericParams::default().into(),
        };
        let parent_param_count = def_generics.count_parent_params();
        substs.extend((0..parent_param_count).map(|_| Ty::Unknown));
        if let Some(generic_args) = &segment.args_and_bindings {
            // if args are provided, it should be all of them, but we can't rely on that
            let param_count = def_generics.params.len();
            for arg in generic_args.args.iter().take(param_count) {
                match arg {
                    GenericArg::Type(type_ref) => {
                        let ty = Ty::from_hir(db, resolver, type_ref);
                        substs.push(ty);
                    }
                }
            }
        }
        // add placeholders for args that were not provided
        // FIXME: handle defaults
        let supplied_params = substs.len();
        for _ in supplied_params..def_generics.count_params_including_parent() {
            substs.push(Ty::Unknown);
        }
        assert_eq!(substs.len(), def_generics.count_params_including_parent());
        Substs(substs.into())
    }

    /// Collect generic arguments from a path into a `Substs`. See also
    /// `create_substs_for_ast_path` and `def_to_ty` in rustc.
    pub(super) fn substs_from_path(
        db: &impl HirDatabase,
        resolver: &Resolver,
        path: &Path,
        resolved: TypableDef,
    ) -> Substs {
        let last = path.segments.last().expect("path should have at least one segment");
        let segment = match resolved {
            TypableDef::Function(_)
            | TypableDef::Struct(_)
            | TypableDef::Enum(_)
            | TypableDef::Const(_)
            | TypableDef::Static(_)
            | TypableDef::TypeAlias(_) => last,
            TypableDef::EnumVariant(_) => {
                // the generic args for an enum variant may be either specified
                // on the segment referring to the enum, or on the segment
                // referring to the variant. So `Option::<T>::None` and
                // `Option::None::<T>` are both allowed (though the former is
                // preferred). See also `def_ids_for_path_segments` in rustc.
                let len = path.segments.len();
                let segment = if len >= 2 && path.segments[len - 2].args_and_bindings.is_some() {
                    // Option::<T>::None
                    &path.segments[len - 2]
                } else {
                    // Option::None::<T>
                    last
                };
                segment
            }
        };
        Ty::substs_from_path_segment(db, resolver, segment, resolved)
    }
}

/// Build the declared type of an item. This depends on the namespace; e.g. for
/// `struct Foo(usize)`, we have two types: The type of the struct itself, and
/// the constructor function `(usize) -> Foo` which lives in the values
/// namespace.
pub(crate) fn type_for_def(db: &impl HirDatabase, def: TypableDef, ns: Namespace) -> Ty {
    match (def, ns) {
        (TypableDef::Function(f), Namespace::Values) => type_for_fn(db, f),
        (TypableDef::Struct(s), Namespace::Types) => type_for_struct(db, s),
        (TypableDef::Struct(s), Namespace::Values) => type_for_struct_constructor(db, s),
        (TypableDef::Enum(e), Namespace::Types) => type_for_enum(db, e),
        (TypableDef::EnumVariant(v), Namespace::Values) => type_for_enum_variant_constructor(db, v),
        (TypableDef::TypeAlias(t), Namespace::Types) => type_for_type_alias(db, t),
        (TypableDef::Const(c), Namespace::Values) => type_for_const(db, c),
        (TypableDef::Static(c), Namespace::Values) => type_for_static(db, c),

        // 'error' cases:
        (TypableDef::Function(_), Namespace::Types) => Ty::Unknown,
        (TypableDef::Enum(_), Namespace::Values) => Ty::Unknown,
        (TypableDef::EnumVariant(_), Namespace::Types) => Ty::Unknown,
        (TypableDef::TypeAlias(_), Namespace::Values) => Ty::Unknown,
        (TypableDef::Const(_), Namespace::Types) => Ty::Unknown,
        (TypableDef::Static(_), Namespace::Types) => Ty::Unknown,
    }
}

/// Build the signature of a callable item (function, struct or enum variant).
pub(crate) fn callable_item_sig(db: &impl HirDatabase, def: CallableDef) -> FnSig {
    match def {
        CallableDef::Function(f) => fn_sig_for_fn(db, f),
        CallableDef::Struct(s) => fn_sig_for_struct_constructor(db, s),
        CallableDef::EnumVariant(e) => fn_sig_for_enum_variant_constructor(db, e),
    }
}

/// Build the type of a specific field of a struct or enum variant.
pub(crate) fn type_for_field(db: &impl HirDatabase, field: StructField) -> Ty {
    let parent_def = field.parent_def(db);
    let resolver = match parent_def {
        VariantDef::Struct(it) => it.resolver(db),
        VariantDef::EnumVariant(it) => it.parent_enum(db).resolver(db),
    };
    let var_data = parent_def.variant_data(db);
    let type_ref = &var_data.fields().unwrap()[field.id].type_ref;
    Ty::from_hir(db, &resolver, type_ref)
}

fn fn_sig_for_fn(db: &impl HirDatabase, def: Function) -> FnSig {
    let signature = def.signature(db);
    let resolver = def.resolver(db);
    let params =
        signature.params().iter().map(|tr| Ty::from_hir(db, &resolver, tr)).collect::<Vec<_>>();
    let ret = Ty::from_hir(db, &resolver, signature.ret_type());
    FnSig::from_params_and_return(params, ret)
}

/// Build the declared type of a function. This should not need to look at the
/// function body.
fn type_for_fn(db: &impl HirDatabase, def: Function) -> Ty {
    let generics = def.generic_params(db);
    let substs = make_substs(&generics);
    Ty::FnDef { def: def.into(), substs }
}

/// Build the declared type of a const.
fn type_for_const(db: &impl HirDatabase, def: Const) -> Ty {
    let signature = def.signature(db);
    let resolver = def.resolver(db);

    Ty::from_hir(db, &resolver, signature.type_ref())
}

/// Build the declared type of a static.
fn type_for_static(db: &impl HirDatabase, def: Static) -> Ty {
    let signature = def.signature(db);
    let resolver = def.resolver(db);

    Ty::from_hir(db, &resolver, signature.type_ref())
}

fn fn_sig_for_struct_constructor(db: &impl HirDatabase, def: Struct) -> FnSig {
    let var_data = def.variant_data(db);
    let fields = match var_data.fields() {
        Some(fields) => fields,
        None => panic!("fn_sig_for_struct_constructor called on unit struct"),
    };
    let resolver = def.resolver(db);
    let params = fields
        .iter()
        .map(|(_, field)| Ty::from_hir(db, &resolver, &field.type_ref))
        .collect::<Vec<_>>();
    let ret = type_for_struct(db, def);
    FnSig::from_params_and_return(params, ret)
}

/// Build the type of a tuple struct constructor.
fn type_for_struct_constructor(db: &impl HirDatabase, def: Struct) -> Ty {
    let var_data = def.variant_data(db);
    if var_data.fields().is_none() {
        return type_for_struct(db, def); // Unit struct
    }
    let generics = def.generic_params(db);
    let substs = make_substs(&generics);
    Ty::FnDef { def: def.into(), substs }
}

fn fn_sig_for_enum_variant_constructor(db: &impl HirDatabase, def: EnumVariant) -> FnSig {
    let var_data = def.variant_data(db);
    let fields = match var_data.fields() {
        Some(fields) => fields,
        None => panic!("fn_sig_for_enum_variant_constructor called for unit variant"),
    };
    let resolver = def.parent_enum(db).resolver(db);
    let params = fields
        .iter()
        .map(|(_, field)| Ty::from_hir(db, &resolver, &field.type_ref))
        .collect::<Vec<_>>();
    let generics = def.parent_enum(db).generic_params(db);
    let substs = make_substs(&generics);
    let ret = type_for_enum(db, def.parent_enum(db)).subst(&substs);
    FnSig::from_params_and_return(params, ret)
}

/// Build the type of a tuple enum variant constructor.
fn type_for_enum_variant_constructor(db: &impl HirDatabase, def: EnumVariant) -> Ty {
    let var_data = def.variant_data(db);
    if var_data.fields().is_none() {
        return type_for_enum(db, def.parent_enum(db)); // Unit variant
    }
    let generics = def.parent_enum(db).generic_params(db);
    let substs = make_substs(&generics);
    Ty::FnDef { def: def.into(), substs }
}

fn make_substs(generics: &GenericParams) -> Substs {
    Substs(
        generics
            .params_including_parent()
            .into_iter()
            .map(|p| Ty::Param { idx: p.idx, name: p.name.clone() })
            .collect::<Vec<_>>()
            .into(),
    )
}

fn type_for_struct(db: &impl HirDatabase, s: Struct) -> Ty {
    let generics = s.generic_params(db);
    Ty::Adt { def_id: s.into(), substs: make_substs(&generics) }
}

fn type_for_enum(db: &impl HirDatabase, s: Enum) -> Ty {
    let generics = s.generic_params(db);
    Ty::Adt { def_id: s.into(), substs: make_substs(&generics) }
}

fn type_for_type_alias(db: &impl HirDatabase, t: TypeAlias) -> Ty {
    let generics = t.generic_params(db);
    let resolver = t.resolver(db);
    let type_ref = t.type_ref(db);
    let substs = make_substs(&generics);
    let inner = Ty::from_hir(db, &resolver, &type_ref);
    inner.subst(&substs)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypableDef {
    Function(Function),
    Struct(Struct),
    Enum(Enum),
    EnumVariant(EnumVariant),
    TypeAlias(TypeAlias),
    Const(Const),
    Static(Static),
}
impl_froms!(TypableDef: Function, Struct, Enum, EnumVariant, TypeAlias, Const, Static);

impl From<ModuleDef> for Option<TypableDef> {
    fn from(def: ModuleDef) -> Option<TypableDef> {
        let res = match def {
            ModuleDef::Function(f) => f.into(),
            ModuleDef::Struct(s) => s.into(),
            ModuleDef::Enum(e) => e.into(),
            ModuleDef::EnumVariant(v) => v.into(),
            ModuleDef::TypeAlias(t) => t.into(),
            ModuleDef::Const(v) => v.into(),
            ModuleDef::Static(v) => v.into(),
            ModuleDef::Module(_) | ModuleDef::Trait(_) => return None,
        };
        Some(res)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallableDef {
    Function(Function),
    Struct(Struct),
    EnumVariant(EnumVariant),
}
impl_froms!(CallableDef: Function, Struct, EnumVariant);
