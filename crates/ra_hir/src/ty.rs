//! The type system. We currently use this to infer types for completion.
//!
//! For type inference, compare the implementations in rustc (the various
//! check_* methods in librustc_typeck/check/mod.rs are a good entry point) and
//! IntelliJ-Rust (org.rust.lang.core.types.infer). Our entry point for
//! inference here is the `infer` function, which infers the types of all
//! expressions in a given function.
//!
//! The central struct here is `Ty`, which represents a type. During inference,
//! it can contain type 'variables' which represent currently unknown types; as
//! we walk through the expressions, we might determine that certain variables
//! need to be equal to each other, or to certain types. To record this, we use
//! the union-find implementation from the `ena` crate, which is extracted from
//! rustc.

mod autoderef;
pub(crate) mod primitive;
#[cfg(test)]
mod tests;
pub(crate) mod method_resolution;

use std::borrow::Cow;
use std::ops::Index;
use std::sync::Arc;
use std::{fmt, mem};

use log;
use ena::unify::{InPlaceUnificationTable, UnifyKey, UnifyValue, NoError};
use ra_arena::map::ArenaMap;
use join_to_string::join;
use rustc_hash::FxHashMap;

use crate::{
    Def, DefId, Module, Function, Struct, StructField, Enum, EnumVariant, Path, Name, ImplBlock,
    FnSignature, FnScopes, ModuleDef,
    db::HirDatabase,
    type_ref::{TypeRef, Mutability},
    name::KnownName,
    expr::{Body, Expr, BindingAnnotation, Literal, ExprId, Pat, PatId, UnaryOp, BinaryOp, Statement, FieldPat},
    generics::GenericParams,
    path::GenericArg,
};

/// The ID of a type variable.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct TypeVarId(u32);

impl UnifyKey for TypeVarId {
    type Value = TypeVarValue;

    fn index(&self) -> u32 {
        self.0
    }

    fn from_index(i: u32) -> Self {
        TypeVarId(i)
    }

    fn tag() -> &'static str {
        "TypeVarId"
    }
}

/// The value of a type variable: either we already know the type, or we don't
/// know it yet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeVarValue {
    Known(Ty),
    Unknown,
}

impl TypeVarValue {
    fn known(&self) -> Option<&Ty> {
        match self {
            TypeVarValue::Known(ty) => Some(ty),
            TypeVarValue::Unknown => None,
        }
    }
}

impl UnifyValue for TypeVarValue {
    type Error = NoError;

    fn unify_values(value1: &Self, value2: &Self) -> Result<Self, NoError> {
        match (value1, value2) {
            // We should never equate two type variables, both of which have
            // known types. Instead, we recursively equate those types.
            (TypeVarValue::Known(t1), TypeVarValue::Known(t2)) => panic!(
                "equating two type variables, both of which have known types: {:?} and {:?}",
                t1, t2
            ),

            // If one side is known, prefer that one.
            (TypeVarValue::Known(..), TypeVarValue::Unknown) => Ok(value1.clone()),
            (TypeVarValue::Unknown, TypeVarValue::Known(..)) => Ok(value2.clone()),

            (TypeVarValue::Unknown, TypeVarValue::Unknown) => Ok(TypeVarValue::Unknown),
        }
    }
}

/// The kinds of placeholders we need during type inference. There's separate
/// values for general types, and for integer and float variables. The latter
/// two are used for inference of literal values (e.g. `100` could be one of
/// several integer types).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum InferTy {
    TypeVar(TypeVarId),
    IntVar(TypeVarId),
    FloatVar(TypeVarId),
}

impl InferTy {
    fn to_inner(self) -> TypeVarId {
        match self {
            InferTy::TypeVar(ty) | InferTy::IntVar(ty) | InferTy::FloatVar(ty) => ty,
        }
    }

    fn fallback_value(self) -> Ty {
        match self {
            InferTy::TypeVar(..) => Ty::Unknown,
            InferTy::IntVar(..) => {
                Ty::Int(primitive::UncertainIntTy::Signed(primitive::IntTy::I32))
            }
            InferTy::FloatVar(..) => {
                Ty::Float(primitive::UncertainFloatTy::Known(primitive::FloatTy::F64))
            }
        }
    }
}

/// When inferring an expression, we propagate downward whatever type hint we
/// are able in the form of an `Expectation`.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Expectation {
    ty: Ty,
    // TODO: In some cases, we need to be aware whether the expectation is that
    // the type match exactly what we passed, or whether it just needs to be
    // coercible to the expected type. See Expectation::rvalue_hint in rustc.
}

impl Expectation {
    /// The expectation that the type of the expression needs to equal the given
    /// type.
    fn has_type(ty: Ty) -> Self {
        Expectation { ty }
    }

    /// This expresses no expectation on the type.
    fn none() -> Self {
        Expectation { ty: Ty::Unknown }
    }
}

/// A list of substitutions for generic parameters.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Substs(Arc<[Ty]>);

impl Substs {
    pub fn empty() -> Substs {
        Substs(Arc::new([]))
    }
}

/// A type. This is based on the `TyKind` enum in rustc (librustc/ty/sty.rs).
///
/// This should be cheap to clone.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ty {
    /// The primitive boolean type. Written as `bool`.
    Bool,

    /// The primitive character type; holds a Unicode scalar value
    /// (a non-surrogate code point). Written as `char`.
    Char,

    /// A primitive integer type. For example, `i32`.
    Int(primitive::UncertainIntTy),

    /// A primitive floating-point type. For example, `f64`.
    Float(primitive::UncertainFloatTy),

    /// Structures, enumerations and unions.
    Adt {
        /// The DefId of the struct/enum.
        def_id: DefId,
        /// The name, for displaying.
        name: Name,
        /// Substitutions for the generic parameters of the type.
        substs: Substs,
    },

    /// The pointee of a string slice. Written as `str`.
    Str,

    /// The pointee of an array slice.  Written as `[T]`.
    Slice(Arc<Ty>),

    // An array with the given length. Written as `[T; n]`.
    Array(Arc<Ty>),

    /// A raw pointer. Written as `*mut T` or `*const T`
    RawPtr(Arc<Ty>, Mutability),

    /// A reference; a pointer with an associated lifetime. Written as
    /// `&'a mut T` or `&'a T`.
    Ref(Arc<Ty>, Mutability),

    /// A pointer to a function.  Written as `fn() -> i32`.
    ///
    /// For example the type of `bar` here:
    ///
    /// ```rust
    /// fn foo() -> i32 { 1 }
    /// let bar: fn() -> i32 = foo;
    /// ```
    FnPtr(Arc<FnSig>),

    // rustc has a separate type for each function, which just coerces to the
    // above function pointer type. Once we implement generics, we will probably
    // need this as well.

    // A trait, defined with `dyn Trait`.
    // Dynamic(),

    // The anonymous type of a closure. Used to represent the type of
    // `|a| a`.
    // Closure(DefId, ClosureSubsts<'tcx>),

    // The anonymous type of a generator. Used to represent the type of
    // `|a| yield a`.
    // Generator(DefId, GeneratorSubsts<'tcx>, hir::GeneratorMovability),

    // A type representing the types stored inside a generator.
    // This should only appear in GeneratorInteriors.
    // GeneratorWitness(Binder<&'tcx List<Ty<'tcx>>>),
    /// The never type `!`.
    Never,

    /// A tuple type.  For example, `(i32, bool)`.
    Tuple(Arc<[Ty]>),

    // The projection of an associated type.  For example,
    // `<T as Trait<..>>::N`.pub
    // Projection(ProjectionTy),

    // Opaque (`impl Trait`) type found in a return type.
    // Opaque(DefId, Substs),
    /// A type parameter; for example, `T` in `fn f<T>(x: T) {}
    Param {
        /// The index of the parameter (starting with parameters from the
        /// surrounding impl, then the current function).
        idx: u32,
        /// The name of the parameter, for displaying.
        name: Name,
    },

    /// A type variable used during type checking. Not to be confused with a
    /// type parameter.
    Infer(InferTy),

    /// A placeholder for a type which could not be computed; this is propagated
    /// to avoid useless error messages. Doubles as a placeholder where type
    /// variables are inserted before type checking, since we want to try to
    /// infer a better type here anyway -- for the IDE use case, we want to try
    /// to infer as much as possible even in the presence of type errors.
    Unknown,
}

/// A function signature.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FnSig {
    input: Vec<Ty>,
    output: Ty,
}

impl Ty {
    pub(crate) fn from_hir(
        db: &impl HirDatabase,
        // TODO: the next three parameters basically describe the scope for name
        // resolution; this should be refactored into something like a general
        // resolver architecture
        module: &Module,
        impl_block: Option<&ImplBlock>,
        generics: &GenericParams,
        type_ref: &TypeRef,
    ) -> Self {
        match type_ref {
            TypeRef::Never => Ty::Never,
            TypeRef::Tuple(inner) => {
                let inner_tys = inner
                    .iter()
                    .map(|tr| Ty::from_hir(db, module, impl_block, generics, tr))
                    .collect::<Vec<_>>();
                Ty::Tuple(inner_tys.into())
            }
            TypeRef::Path(path) => Ty::from_hir_path(db, module, impl_block, generics, path),
            TypeRef::RawPtr(inner, mutability) => {
                let inner_ty = Ty::from_hir(db, module, impl_block, generics, inner);
                Ty::RawPtr(Arc::new(inner_ty), *mutability)
            }
            TypeRef::Array(inner) => {
                let inner_ty = Ty::from_hir(db, module, impl_block, generics, inner);
                Ty::Array(Arc::new(inner_ty))
            }
            TypeRef::Slice(inner) => {
                let inner_ty = Ty::from_hir(db, module, impl_block, generics, inner);
                Ty::Slice(Arc::new(inner_ty))
            }
            TypeRef::Reference(inner, mutability) => {
                let inner_ty = Ty::from_hir(db, module, impl_block, generics, inner);
                Ty::Ref(Arc::new(inner_ty), *mutability)
            }
            TypeRef::Placeholder => Ty::Unknown,
            TypeRef::Fn(params) => {
                let mut inner_tys = params
                    .iter()
                    .map(|tr| Ty::from_hir(db, module, impl_block, generics, tr))
                    .collect::<Vec<_>>();
                let return_ty = inner_tys
                    .pop()
                    .expect("TypeRef::Fn should always have at least return type");
                let sig = FnSig {
                    input: inner_tys,
                    output: return_ty,
                };
                Ty::FnPtr(Arc::new(sig))
            }
            TypeRef::Error => Ty::Unknown,
        }
    }

    pub(crate) fn from_hir_opt(
        db: &impl HirDatabase,
        module: &Module,
        impl_block: Option<&ImplBlock>,
        generics: &GenericParams,
        type_ref: Option<&TypeRef>,
    ) -> Self {
        type_ref.map_or(Ty::Unknown, |t| {
            Ty::from_hir(db, module, impl_block, generics, t)
        })
    }

    pub(crate) fn from_hir_path(
        db: &impl HirDatabase,
        module: &Module,
        impl_block: Option<&ImplBlock>,
        generics: &GenericParams,
        path: &Path,
    ) -> Self {
        if let Some(name) = path.as_ident() {
            if let Some(int_ty) = primitive::UncertainIntTy::from_name(name) {
                return Ty::Int(int_ty);
            } else if let Some(float_ty) = primitive::UncertainFloatTy::from_name(name) {
                return Ty::Float(float_ty);
            } else if name.as_known_name() == Some(KnownName::SelfType) {
                // TODO pass the impl block's generics?
                let generics = &GenericParams::default();
                return Ty::from_hir_opt(
                    db,
                    module,
                    None,
                    generics,
                    impl_block.map(|i| i.target_type()),
                );
            } else if let Some(known) = name.as_known_name() {
                match known {
                    KnownName::Bool => return Ty::Bool,
                    KnownName::Char => return Ty::Char,
                    KnownName::Str => return Ty::Str,
                    _ => {}
                }
            } else if let Some(generic_param) = generics.find_by_name(&name) {
                return Ty::Param {
                    idx: generic_param.idx,
                    name: generic_param.name.clone(),
                };
            }
        }

        // Resolve in module (in type namespace)
        let resolved = match module.resolve_path(db, path).take_types() {
            Some(ModuleDef::Def(r)) => r,
            None | Some(ModuleDef::Module(_)) => return Ty::Unknown,
        };
        let ty = db.type_for_def(resolved);
        let substs = Ty::substs_from_path(db, module, impl_block, generics, path, resolved);
        ty.apply_substs(substs)
    }

    /// Collect generic arguments from a path into a `Substs`. See also
    /// `create_substs_for_ast_path` and `def_to_ty` in rustc.
    fn substs_from_path(
        db: &impl HirDatabase,
        // the scope of the segment...
        module: &Module,
        impl_block: Option<&ImplBlock>,
        outer_generics: &GenericParams,
        path: &Path,
        resolved: DefId,
    ) -> Substs {
        let mut substs = Vec::new();
        let def = resolved.resolve(db);
        let last = path
            .segments
            .last()
            .expect("path should have at least one segment");
        let (def_generics, segment) = match def {
            Def::Struct(s) => (s.generic_params(db), last),
            Def::Enum(e) => (e.generic_params(db), last),
            Def::Function(f) => (f.generic_params(db), last),
            Def::Trait(t) => (t.generic_params(db), last),
            Def::EnumVariant(ev) => {
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
                (ev.parent_enum(db).generic_params(db), segment)
            }
            _ => return Substs::empty(),
        };
        // substs_from_path
        if let Some(generic_args) = &segment.args_and_bindings {
            // if args are provided, it should be all of them, but we can't rely on that
            let param_count = def_generics.params.len();
            for arg in generic_args.args.iter().take(param_count) {
                match arg {
                    GenericArg::Type(type_ref) => {
                        let ty = Ty::from_hir(db, module, impl_block, outer_generics, type_ref);
                        substs.push(ty);
                    }
                }
            }
        }
        // add placeholders for args that were not provided
        // TODO: handle defaults
        for _ in segment
            .args_and_bindings
            .as_ref()
            .map(|ga| ga.args.len())
            .unwrap_or(0)..def_generics.params.len()
        {
            substs.push(Ty::Unknown);
        }
        assert_eq!(substs.len(), def_generics.params.len());
        Substs(substs.into())
    }

    pub fn unit() -> Self {
        Ty::Tuple(Arc::new([]))
    }

    fn walk_mut(&mut self, f: &mut impl FnMut(&mut Ty)) {
        f(self);
        match self {
            Ty::Slice(t) | Ty::Array(t) => Arc::make_mut(t).walk_mut(f),
            Ty::RawPtr(t, _) => Arc::make_mut(t).walk_mut(f),
            Ty::Ref(t, _) => Arc::make_mut(t).walk_mut(f),
            Ty::Tuple(ts) => {
                // Without an Arc::make_mut_slice, we can't avoid the clone here:
                let mut v: Vec<_> = ts.iter().cloned().collect();
                for t in &mut v {
                    t.walk_mut(f);
                }
                *ts = v.into();
            }
            Ty::FnPtr(sig) => {
                let sig_mut = Arc::make_mut(sig);
                for input in &mut sig_mut.input {
                    input.walk_mut(f);
                }
                sig_mut.output.walk_mut(f);
            }
            Ty::Adt { substs, .. } => {
                // Without an Arc::make_mut_slice, we can't avoid the clone here:
                let mut v: Vec<_> = substs.0.iter().cloned().collect();
                for t in &mut v {
                    t.walk_mut(f);
                }
                substs.0 = v.into();
            }
            _ => {}
        }
    }

    fn fold(mut self, f: &mut impl FnMut(Ty) -> Ty) -> Ty {
        self.walk_mut(&mut |ty_mut| {
            let ty = mem::replace(ty_mut, Ty::Unknown);
            *ty_mut = f(ty);
        });
        self
    }

    fn builtin_deref(&self) -> Option<Ty> {
        match self {
            Ty::Ref(t, _) => Some(Ty::clone(t)),
            Ty::RawPtr(t, _) => Some(Ty::clone(t)),
            _ => None,
        }
    }

    /// If this is a type with type parameters (an ADT or function), replaces
    /// the `Substs` for these type parameters with the given ones. (So e.g. if
    /// `self` is `Option<_>` and the substs contain `u32`, we'll have
    /// `Option<u32>` afterwards.)
    pub fn apply_substs(self, substs: Substs) -> Ty {
        match self {
            Ty::Adt { def_id, name, .. } => Ty::Adt {
                def_id,
                name,
                substs,
            },
            _ => self,
        }
    }

    /// Replaces type parameters in this type using the given `Substs`. (So e.g.
    /// if `self` is `&[T]`, where type parameter T has index 0, and the
    /// `Substs` contain `u32` at index 0, we'll have `&[u32]` afterwards.)
    pub fn subst(self, substs: &Substs) -> Ty {
        self.fold(&mut |ty| match ty {
            Ty::Param { idx, name } => {
                if (idx as usize) < substs.0.len() {
                    substs.0[idx as usize].clone()
                } else {
                    // TODO: does this indicate a bug? i.e. should we always
                    // have substs for all type params? (they might contain the
                    // params themselves again...)
                    Ty::Param { idx, name }
                }
            }
            ty => ty,
        })
    }

    /// Returns the type parameters of this type if it has some (i.e. is an ADT
    /// or function); so if `self` is `Option<u32>`, this returns the `u32`.
    fn substs(&self) -> Option<Substs> {
        match self {
            Ty::Adt { substs, .. } => Some(substs.clone()),
            _ => None,
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Ty::Bool => write!(f, "bool"),
            Ty::Char => write!(f, "char"),
            Ty::Int(t) => write!(f, "{}", t.ty_to_string()),
            Ty::Float(t) => write!(f, "{}", t.ty_to_string()),
            Ty::Str => write!(f, "str"),
            Ty::Slice(t) | Ty::Array(t) => write!(f, "[{}]", t),
            Ty::RawPtr(t, m) => write!(f, "*{}{}", m.as_keyword_for_ptr(), t),
            Ty::Ref(t, m) => write!(f, "&{}{}", m.as_keyword_for_ref(), t),
            Ty::Never => write!(f, "!"),
            Ty::Tuple(ts) => {
                if ts.len() == 1 {
                    write!(f, "({},)", ts[0])
                } else {
                    join(ts.iter())
                        .surround_with("(", ")")
                        .separator(", ")
                        .to_fmt(f)
                }
            }
            Ty::FnPtr(sig) => {
                join(sig.input.iter())
                    .surround_with("fn(", ")")
                    .separator(", ")
                    .to_fmt(f)?;
                write!(f, " -> {}", sig.output)
            }
            Ty::Adt { name, substs, .. } => {
                write!(f, "{}", name)?;
                if substs.0.len() > 0 {
                    join(substs.0.iter())
                        .surround_with("<", ">")
                        .separator(", ")
                        .to_fmt(f)?;
                }
                Ok(())
            }
            Ty::Param { name, .. } => write!(f, "{}", name),
            Ty::Unknown => write!(f, "[unknown]"),
            Ty::Infer(..) => write!(f, "_"),
        }
    }
}

// Functions returning declared types for items

/// Compute the declared type of a function. This should not need to look at the
/// function body.
fn type_for_fn(db: &impl HirDatabase, f: Function) -> Ty {
    let signature = f.signature(db);
    let module = f.module(db);
    let impl_block = f.impl_block(db);
    let generics = f.generic_params(db);
    let input = signature
        .params()
        .iter()
        .map(|tr| Ty::from_hir(db, &module, impl_block.as_ref(), &generics, tr))
        .collect::<Vec<_>>();
    let output = Ty::from_hir(
        db,
        &module,
        impl_block.as_ref(),
        &generics,
        signature.ret_type(),
    );
    let sig = FnSig { input, output };
    Ty::FnPtr(Arc::new(sig))
}

fn make_substs(generics: &GenericParams) -> Substs {
    Substs(
        generics
            .params
            .iter()
            .map(|_p| Ty::Unknown)
            .collect::<Vec<_>>()
            .into(),
    )
}

fn type_for_struct(db: &impl HirDatabase, s: Struct) -> Ty {
    let generics = s.generic_params(db);
    Ty::Adt {
        def_id: s.def_id(),
        name: s.name(db).unwrap_or_else(Name::missing),
        substs: make_substs(&generics),
    }
}

pub(crate) fn type_for_enum(db: &impl HirDatabase, s: Enum) -> Ty {
    let generics = s.generic_params(db);
    Ty::Adt {
        def_id: s.def_id(),
        name: s.name(db).unwrap_or_else(Name::missing),
        substs: make_substs(&generics),
    }
}

pub(crate) fn type_for_enum_variant(db: &impl HirDatabase, ev: EnumVariant) -> Ty {
    let enum_parent = ev.parent_enum(db);

    type_for_enum(db, enum_parent)
}

pub(super) fn type_for_def(db: &impl HirDatabase, def_id: DefId) -> Ty {
    let def = def_id.resolve(db);
    match def {
        Def::Function(f) => type_for_fn(db, f),
        Def::Struct(s) => type_for_struct(db, s),
        Def::Enum(e) => type_for_enum(db, e),
        Def::EnumVariant(ev) => type_for_enum_variant(db, ev),
        _ => {
            log::debug!(
                "trying to get type for item of unknown type {:?} {:?}",
                def_id,
                def
            );
            Ty::Unknown
        }
    }
}

pub(super) fn type_for_field(db: &impl HirDatabase, def_id: DefId, field: Name) -> Option<Ty> {
    let def = def_id.resolve(db);
    let (variant_data, generics) = match def {
        Def::Struct(s) => (s.variant_data(db), s.generic_params(db)),
        Def::EnumVariant(ev) => (ev.variant_data(db), ev.parent_enum(db).generic_params(db)),
        // TODO: unions
        Def::Enum(_) => {
            // this can happen in (invalid) code, but enums don't have fields themselves
            return None;
        }
        _ => panic!(
            "trying to get type for field {:?} in non-struct/variant {:?}",
            field, def_id
        ),
    };
    let module = def_id.module(db);
    let impl_block = def_id.impl_block(db);
    let type_ref = variant_data.get_field_type_ref(&field)?;
    Some(Ty::from_hir(
        db,
        &module,
        impl_block.as_ref(),
        &generics,
        &type_ref,
    ))
}

/// The result of type inference: A mapping from expressions and patterns to types.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InferenceResult {
    /// For each method call expr, record the function it resolved to.
    method_resolutions: FxHashMap<ExprId, DefId>,
    type_of_expr: ArenaMap<ExprId, Ty>,
    type_of_pat: ArenaMap<PatId, Ty>,
}

impl InferenceResult {
    pub fn method_resolution(&self, expr: ExprId) -> Option<DefId> {
        self.method_resolutions.get(&expr).map(|it| *it)
    }
}

impl Index<ExprId> for InferenceResult {
    type Output = Ty;

    fn index(&self, expr: ExprId) -> &Ty {
        self.type_of_expr.get(expr).unwrap_or(&Ty::Unknown)
    }
}

impl Index<PatId> for InferenceResult {
    type Output = Ty;

    fn index(&self, pat: PatId) -> &Ty {
        self.type_of_pat.get(pat).unwrap_or(&Ty::Unknown)
    }
}

/// The inference context contains all information needed during type inference.
#[derive(Clone, Debug)]
struct InferenceContext<'a, D: HirDatabase> {
    db: &'a D,
    body: Arc<Body>,
    scopes: Arc<FnScopes>,
    module: Module,
    impl_block: Option<ImplBlock>,
    var_unification_table: InPlaceUnificationTable<TypeVarId>,
    method_resolutions: FxHashMap<ExprId, DefId>,
    type_of_expr: ArenaMap<ExprId, Ty>,
    type_of_pat: ArenaMap<PatId, Ty>,
    /// The return type of the function being inferred.
    return_ty: Ty,
}

fn binary_op_return_ty(op: BinaryOp, rhs_ty: Ty) -> Ty {
    match op {
        BinaryOp::BooleanOr
        | BinaryOp::BooleanAnd
        | BinaryOp::EqualityTest
        | BinaryOp::LesserEqualTest
        | BinaryOp::GreaterEqualTest
        | BinaryOp::LesserTest
        | BinaryOp::GreaterTest => Ty::Bool,
        BinaryOp::Assignment
        | BinaryOp::AddAssign
        | BinaryOp::SubAssign
        | BinaryOp::DivAssign
        | BinaryOp::MulAssign
        | BinaryOp::RemAssign
        | BinaryOp::ShrAssign
        | BinaryOp::ShlAssign
        | BinaryOp::BitAndAssign
        | BinaryOp::BitOrAssign
        | BinaryOp::BitXorAssign => Ty::unit(),
        BinaryOp::Addition
        | BinaryOp::Subtraction
        | BinaryOp::Multiplication
        | BinaryOp::Division
        | BinaryOp::Remainder
        | BinaryOp::LeftShift
        | BinaryOp::RightShift
        | BinaryOp::BitwiseAnd
        | BinaryOp::BitwiseOr
        | BinaryOp::BitwiseXor => match rhs_ty {
            Ty::Int(..) | Ty::Float(..) => rhs_ty,
            _ => Ty::Unknown,
        },
        BinaryOp::RangeRightOpen | BinaryOp::RangeRightClosed => Ty::Unknown,
    }
}

fn binary_op_rhs_expectation(op: BinaryOp, lhs_ty: Ty) -> Ty {
    match op {
        BinaryOp::BooleanAnd | BinaryOp::BooleanOr => Ty::Bool,
        BinaryOp::Assignment | BinaryOp::EqualityTest => match lhs_ty {
            Ty::Int(..) | Ty::Float(..) | Ty::Str | Ty::Char | Ty::Bool => lhs_ty,
            _ => Ty::Unknown,
        },
        BinaryOp::LesserEqualTest
        | BinaryOp::GreaterEqualTest
        | BinaryOp::LesserTest
        | BinaryOp::GreaterTest
        | BinaryOp::AddAssign
        | BinaryOp::SubAssign
        | BinaryOp::DivAssign
        | BinaryOp::MulAssign
        | BinaryOp::RemAssign
        | BinaryOp::ShrAssign
        | BinaryOp::ShlAssign
        | BinaryOp::BitAndAssign
        | BinaryOp::BitOrAssign
        | BinaryOp::BitXorAssign
        | BinaryOp::Addition
        | BinaryOp::Subtraction
        | BinaryOp::Multiplication
        | BinaryOp::Division
        | BinaryOp::Remainder
        | BinaryOp::LeftShift
        | BinaryOp::RightShift
        | BinaryOp::BitwiseAnd
        | BinaryOp::BitwiseOr
        | BinaryOp::BitwiseXor => match lhs_ty {
            Ty::Int(..) | Ty::Float(..) => lhs_ty,
            _ => Ty::Unknown,
        },
        _ => Ty::Unknown,
    }
}

impl<'a, D: HirDatabase> InferenceContext<'a, D> {
    fn new(
        db: &'a D,
        body: Arc<Body>,
        scopes: Arc<FnScopes>,
        module: Module,
        impl_block: Option<ImplBlock>,
    ) -> Self {
        InferenceContext {
            method_resolutions: FxHashMap::default(),
            type_of_expr: ArenaMap::default(),
            type_of_pat: ArenaMap::default(),
            var_unification_table: InPlaceUnificationTable::new(),
            return_ty: Ty::Unknown, // set in collect_fn_signature
            db,
            body,
            scopes,
            module,
            impl_block,
        }
    }

    fn resolve_all(mut self) -> InferenceResult {
        let mut expr_types = mem::replace(&mut self.type_of_expr, ArenaMap::default());
        for ty in expr_types.values_mut() {
            let resolved = self.resolve_ty_completely(mem::replace(ty, Ty::Unknown));
            *ty = resolved;
        }
        let mut pat_types = mem::replace(&mut self.type_of_pat, ArenaMap::default());
        for ty in pat_types.values_mut() {
            let resolved = self.resolve_ty_completely(mem::replace(ty, Ty::Unknown));
            *ty = resolved;
        }
        InferenceResult {
            method_resolutions: mem::replace(&mut self.method_resolutions, Default::default()),
            type_of_expr: expr_types,
            type_of_pat: pat_types,
        }
    }

    fn write_expr_ty(&mut self, expr: ExprId, ty: Ty) {
        self.type_of_expr.insert(expr, ty);
    }

    fn write_method_resolution(&mut self, expr: ExprId, def_id: DefId) {
        self.method_resolutions.insert(expr, def_id);
    }

    fn write_pat_ty(&mut self, pat: PatId, ty: Ty) {
        self.type_of_pat.insert(pat, ty);
    }

    fn make_ty(&mut self, type_ref: &TypeRef) -> Ty {
        // TODO provide generics of function
        let generics = GenericParams::default();
        let ty = Ty::from_hir(
            self.db,
            &self.module,
            self.impl_block.as_ref(),
            &generics,
            type_ref,
        );
        let ty = self.insert_type_vars(ty);
        ty
    }

    fn unify_substs(&mut self, substs1: &Substs, substs2: &Substs) -> bool {
        substs1
            .0
            .iter()
            .zip(substs2.0.iter())
            .all(|(t1, t2)| self.unify(t1, t2))
    }

    fn unify(&mut self, ty1: &Ty, ty2: &Ty) -> bool {
        // try to resolve type vars first
        let ty1 = self.resolve_ty_shallow(ty1);
        let ty2 = self.resolve_ty_shallow(ty2);
        match (&*ty1, &*ty2) {
            (Ty::Unknown, ..) => true,
            (.., Ty::Unknown) => true,
            (Ty::Int(t1), Ty::Int(t2)) => match (t1, t2) {
                (primitive::UncertainIntTy::Unknown, _)
                | (_, primitive::UncertainIntTy::Unknown) => true,
                _ => t1 == t2,
            },
            (Ty::Float(t1), Ty::Float(t2)) => match (t1, t2) {
                (primitive::UncertainFloatTy::Unknown, _)
                | (_, primitive::UncertainFloatTy::Unknown) => true,
                _ => t1 == t2,
            },
            (Ty::Bool, _) | (Ty::Str, _) | (Ty::Never, _) | (Ty::Char, _) => ty1 == ty2,
            (
                Ty::Adt {
                    def_id: def_id1,
                    substs: substs1,
                    ..
                },
                Ty::Adt {
                    def_id: def_id2,
                    substs: substs2,
                    ..
                },
            ) if def_id1 == def_id2 => self.unify_substs(substs1, substs2),
            (Ty::Slice(t1), Ty::Slice(t2)) => self.unify(t1, t2),
            (Ty::RawPtr(t1, m1), Ty::RawPtr(t2, m2)) if m1 == m2 => self.unify(t1, t2),
            (Ty::Ref(t1, m1), Ty::Ref(t2, m2)) if m1 == m2 => self.unify(t1, t2),
            (Ty::FnPtr(sig1), Ty::FnPtr(sig2)) if sig1 == sig2 => true,
            (Ty::Tuple(ts1), Ty::Tuple(ts2)) if ts1.len() == ts2.len() => ts1
                .iter()
                .zip(ts2.iter())
                .all(|(t1, t2)| self.unify(t1, t2)),
            (Ty::Infer(InferTy::TypeVar(tv1)), Ty::Infer(InferTy::TypeVar(tv2)))
            | (Ty::Infer(InferTy::IntVar(tv1)), Ty::Infer(InferTy::IntVar(tv2)))
            | (Ty::Infer(InferTy::FloatVar(tv1)), Ty::Infer(InferTy::FloatVar(tv2))) => {
                // both type vars are unknown since we tried to resolve them
                self.var_unification_table.union(*tv1, *tv2);
                true
            }
            (Ty::Infer(InferTy::TypeVar(tv)), other)
            | (other, Ty::Infer(InferTy::TypeVar(tv)))
            | (Ty::Infer(InferTy::IntVar(tv)), other)
            | (other, Ty::Infer(InferTy::IntVar(tv)))
            | (Ty::Infer(InferTy::FloatVar(tv)), other)
            | (other, Ty::Infer(InferTy::FloatVar(tv))) => {
                // the type var is unknown since we tried to resolve it
                self.var_unification_table
                    .union_value(*tv, TypeVarValue::Known(other.clone()));
                true
            }
            _ => false,
        }
    }

    fn new_type_var(&mut self) -> Ty {
        Ty::Infer(InferTy::TypeVar(
            self.var_unification_table.new_key(TypeVarValue::Unknown),
        ))
    }

    fn new_integer_var(&mut self) -> Ty {
        Ty::Infer(InferTy::IntVar(
            self.var_unification_table.new_key(TypeVarValue::Unknown),
        ))
    }

    fn new_float_var(&mut self) -> Ty {
        Ty::Infer(InferTy::FloatVar(
            self.var_unification_table.new_key(TypeVarValue::Unknown),
        ))
    }

    /// Replaces Ty::Unknown by a new type var, so we can maybe still infer it.
    fn insert_type_vars_shallow(&mut self, ty: Ty) -> Ty {
        match ty {
            Ty::Unknown => self.new_type_var(),
            Ty::Int(primitive::UncertainIntTy::Unknown) => self.new_integer_var(),
            Ty::Float(primitive::UncertainFloatTy::Unknown) => self.new_float_var(),
            _ => ty,
        }
    }

    fn insert_type_vars(&mut self, ty: Ty) -> Ty {
        ty.fold(&mut |ty| self.insert_type_vars_shallow(ty))
    }

    /// Resolves the type as far as currently possible, replacing type variables
    /// by their known types. All types returned by the infer_* functions should
    /// be resolved as far as possible, i.e. contain no type variables with
    /// known type.
    fn resolve_ty_as_possible(&mut self, ty: Ty) -> Ty {
        ty.fold(&mut |ty| match ty {
            Ty::Infer(tv) => {
                let inner = tv.to_inner();
                if let Some(known_ty) = self.var_unification_table.probe_value(inner).known() {
                    // known_ty may contain other variables that are known by now
                    self.resolve_ty_as_possible(known_ty.clone())
                } else {
                    ty
                }
            }
            _ => ty,
        })
    }

    /// If `ty` is a type variable with known type, returns that type;
    /// otherwise, return ty.
    fn resolve_ty_shallow<'b>(&mut self, ty: &'b Ty) -> Cow<'b, Ty> {
        match ty {
            Ty::Infer(tv) => {
                let inner = tv.to_inner();
                match self.var_unification_table.probe_value(inner).known() {
                    Some(known_ty) => {
                        // The known_ty can't be a type var itself
                        Cow::Owned(known_ty.clone())
                    }
                    _ => Cow::Borrowed(ty),
                }
            }
            _ => Cow::Borrowed(ty),
        }
    }

    /// Resolves the type completely; type variables without known type are
    /// replaced by Ty::Unknown.
    fn resolve_ty_completely(&mut self, ty: Ty) -> Ty {
        ty.fold(&mut |ty| match ty {
            Ty::Infer(tv) => {
                let inner = tv.to_inner();
                if let Some(known_ty) = self.var_unification_table.probe_value(inner).known() {
                    // known_ty may contain other variables that are known by now
                    self.resolve_ty_completely(known_ty.clone())
                } else {
                    tv.fallback_value()
                }
            }
            _ => ty,
        })
    }

    fn infer_path_expr(&mut self, expr: ExprId, path: &Path) -> Option<Ty> {
        if path.is_ident() || path.is_self() {
            // resolve locally
            let name = path.as_ident().cloned().unwrap_or_else(Name::self_param);
            if let Some(scope_entry) = self.scopes.resolve_local_name(expr, name) {
                let ty = self.type_of_pat.get(scope_entry.pat())?;
                let ty = self.resolve_ty_as_possible(ty.clone());
                return Some(ty);
            };
        };

        // resolve in module
        let resolved = match self.module.resolve_path(self.db, &path).take_values()? {
            ModuleDef::Def(it) => it,
            ModuleDef::Module(_) => return None,
        };
        let ty = self.db.type_for_def(resolved);
        let ty = self.insert_type_vars(ty);
        Some(ty)
    }

    fn resolve_variant(&mut self, path: Option<&Path>) -> (Ty, Option<DefId>) {
        let path = match path {
            Some(path) => path,
            None => return (Ty::Unknown, None),
        };
        let def_id = match self.module.resolve_path(self.db, &path).take_types() {
            Some(ModuleDef::Def(def_id)) => def_id,
            _ => return (Ty::Unknown, None),
        };
        // TODO remove the duplication between here and `Ty::from_path`?
        // TODO provide generics of function
        let generics = GenericParams::default();
        let substs = Ty::substs_from_path(
            self.db,
            &self.module,
            self.impl_block.as_ref(),
            &generics,
            path,
            def_id,
        );
        match def_id.resolve(self.db) {
            Def::Struct(s) => {
                let ty = type_for_struct(self.db, s);
                let ty = self.insert_type_vars(ty.apply_substs(substs));
                (ty, Some(def_id))
            }
            Def::EnumVariant(ev) => {
                let ty = type_for_enum_variant(self.db, ev);
                let ty = self.insert_type_vars(ty.apply_substs(substs));
                (ty, Some(def_id))
            }
            _ => (Ty::Unknown, None),
        }
    }

    fn resolve_fields(&mut self, path: Option<&Path>) -> Option<(Ty, Vec<StructField>)> {
        let (ty, def_id) = self.resolve_variant(path);
        let def_id = def_id?;
        let def = def_id.resolve(self.db);

        match def {
            Def::Struct(s) => {
                let fields = s.fields(self.db);
                Some((ty, fields))
            }
            Def::EnumVariant(ev) => {
                let fields = ev.fields(self.db);
                Some((ty, fields))
            }
            _ => None,
        }
    }

    fn infer_tuple_struct_pat(
        &mut self,
        path: Option<&Path>,
        subpats: &[PatId],
        expected: &Ty,
    ) -> Ty {
        let (ty, fields) = self
            .resolve_fields(path)
            .unwrap_or((Ty::Unknown, Vec::new()));

        self.unify(&ty, expected);

        let substs = ty.substs().unwrap_or_else(Substs::empty);

        for (i, &subpat) in subpats.iter().enumerate() {
            let expected_ty = fields
                .get(i)
                .and_then(|field| field.ty(self.db))
                .unwrap_or(Ty::Unknown)
                .subst(&substs);
            self.infer_pat(subpat, &expected_ty);
        }

        ty
    }

    fn infer_struct_pat(&mut self, path: Option<&Path>, subpats: &[FieldPat], expected: &Ty) -> Ty {
        let (ty, fields) = self
            .resolve_fields(path)
            .unwrap_or((Ty::Unknown, Vec::new()));

        self.unify(&ty, expected);

        let substs = ty.substs().unwrap_or_else(Substs::empty);

        for subpat in subpats {
            let matching_field = fields.iter().find(|field| field.name() == &subpat.name);
            let expected_ty = matching_field
                .and_then(|field| field.ty(self.db))
                .unwrap_or(Ty::Unknown)
                .subst(&substs);
            self.infer_pat(subpat.pat, &expected_ty);
        }

        ty
    }

    fn infer_pat(&mut self, pat: PatId, expected: &Ty) -> Ty {
        let body = Arc::clone(&self.body); // avoid borrow checker problem

        let ty = match &body[pat] {
            Pat::Tuple(ref args) => {
                let expectations = match *expected {
                    Ty::Tuple(ref tuple_args) => &**tuple_args,
                    _ => &[],
                };
                let expectations_iter = expectations
                    .into_iter()
                    .chain(std::iter::repeat(&Ty::Unknown));

                let inner_tys = args
                    .iter()
                    .zip(expectations_iter)
                    .map(|(&pat, ty)| self.infer_pat(pat, ty))
                    .collect::<Vec<_>>()
                    .into();

                Ty::Tuple(inner_tys)
            }
            Pat::Ref { pat, mutability } => {
                let expectation = match *expected {
                    Ty::Ref(ref sub_ty, exp_mut) => {
                        if *mutability != exp_mut {
                            // TODO: emit type error?
                        }
                        &**sub_ty
                    }
                    _ => &Ty::Unknown,
                };
                let subty = self.infer_pat(*pat, expectation);
                Ty::Ref(subty.into(), *mutability)
            }
            Pat::TupleStruct {
                path: ref p,
                args: ref subpats,
            } => self.infer_tuple_struct_pat(p.as_ref(), subpats, expected),
            Pat::Struct {
                path: ref p,
                args: ref fields,
            } => self.infer_struct_pat(p.as_ref(), fields, expected),
            Pat::Path(path) => self
                .module
                .resolve_path(self.db, &path)
                .take_values()
                .and_then(|module_def| match module_def {
                    ModuleDef::Def(it) => Some(it),
                    ModuleDef::Module(_) => None,
                })
                .map_or(Ty::Unknown, |resolved| self.db.type_for_def(resolved)),
            Pat::Bind {
                mode,
                name: _name,
                subpat,
            } => {
                let subty = if let Some(subpat) = subpat {
                    self.infer_pat(*subpat, expected)
                } else {
                    expected.clone()
                };

                match mode {
                    BindingAnnotation::Ref => Ty::Ref(subty.into(), Mutability::Shared),
                    BindingAnnotation::RefMut => Ty::Ref(subty.into(), Mutability::Mut),
                    BindingAnnotation::Mutable | BindingAnnotation::Unannotated => subty,
                }
            }
            _ => Ty::Unknown,
        };
        // use a new type variable if we got Ty::Unknown here
        let ty = self.insert_type_vars_shallow(ty);
        self.unify(&ty, expected);
        let ty = self.resolve_ty_as_possible(ty);
        self.write_pat_ty(pat, ty.clone());
        ty
    }

    fn infer_expr(&mut self, expr: ExprId, expected: &Expectation) -> Ty {
        let body = Arc::clone(&self.body); // avoid borrow checker problem
        let ty = match &body[expr] {
            Expr::Missing => Ty::Unknown,
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => {
                // if let is desugared to match, so this is always simple if
                self.infer_expr(*condition, &Expectation::has_type(Ty::Bool));
                let then_ty = self.infer_expr(*then_branch, expected);
                match else_branch {
                    Some(else_branch) => {
                        self.infer_expr(*else_branch, expected);
                    }
                    None => {
                        // no else branch -> unit
                        self.unify(&then_ty, &Ty::unit()); // actually coerce
                    }
                };
                then_ty
            }
            Expr::Block { statements, tail } => self.infer_block(statements, *tail, expected),
            Expr::Loop { body } => {
                self.infer_expr(*body, &Expectation::has_type(Ty::unit()));
                // TODO handle break with value
                Ty::Never
            }
            Expr::While { condition, body } => {
                // while let is desugared to a match loop, so this is always simple while
                self.infer_expr(*condition, &Expectation::has_type(Ty::Bool));
                self.infer_expr(*body, &Expectation::has_type(Ty::unit()));
                Ty::unit()
            }
            Expr::For {
                iterable,
                body,
                pat,
            } => {
                let _iterable_ty = self.infer_expr(*iterable, &Expectation::none());
                self.infer_pat(*pat, &Ty::Unknown);
                self.infer_expr(*body, &Expectation::has_type(Ty::unit()));
                Ty::unit()
            }
            Expr::Lambda {
                body,
                args,
                arg_types,
            } => {
                assert_eq!(args.len(), arg_types.len());

                for (arg_pat, arg_type) in args.iter().zip(arg_types.iter()) {
                    let expected = if let Some(type_ref) = arg_type {
                        let ty = self.make_ty(type_ref);
                        ty
                    } else {
                        Ty::Unknown
                    };
                    self.infer_pat(*arg_pat, &expected);
                }

                // TODO: infer lambda type etc.
                let _body_ty = self.infer_expr(*body, &Expectation::none());
                Ty::Unknown
            }
            Expr::Call { callee, args } => {
                let callee_ty = self.infer_expr(*callee, &Expectation::none());
                let (param_tys, ret_ty) = match &callee_ty {
                    Ty::FnPtr(sig) => (&sig.input[..], sig.output.clone()),
                    _ => {
                        // not callable
                        // TODO report an error?
                        (&[][..], Ty::Unknown)
                    }
                };
                for (i, arg) in args.iter().enumerate() {
                    self.infer_expr(
                        *arg,
                        &Expectation::has_type(param_tys.get(i).cloned().unwrap_or(Ty::Unknown)),
                    );
                }
                ret_ty
            }
            Expr::MethodCall {
                receiver,
                args,
                method_name,
            } => {
                let receiver_ty = self.infer_expr(*receiver, &Expectation::none());
                let resolved = receiver_ty.clone().lookup_method(self.db, method_name);
                let method_ty = match resolved {
                    Some(def_id) => {
                        self.write_method_resolution(expr, def_id);
                        self.db.type_for_def(def_id)
                    }
                    None => Ty::Unknown,
                };
                let method_ty = self.insert_type_vars(method_ty);
                let (expected_receiver_ty, param_tys, ret_ty) = match &method_ty {
                    Ty::FnPtr(sig) => {
                        if sig.input.len() > 0 {
                            (&sig.input[0], &sig.input[1..], sig.output.clone())
                        } else {
                            (&Ty::Unknown, &[][..], sig.output.clone())
                        }
                    }
                    _ => (&Ty::Unknown, &[][..], Ty::Unknown),
                };
                // TODO we would have to apply the autoderef/autoref steps here
                // to get the correct receiver type to unify...
                self.unify(expected_receiver_ty, &receiver_ty);
                for (i, arg) in args.iter().enumerate() {
                    self.infer_expr(
                        *arg,
                        &Expectation::has_type(param_tys.get(i).cloned().unwrap_or(Ty::Unknown)),
                    );
                }
                ret_ty
            }
            Expr::Match { expr, arms } => {
                let expected = if expected.ty == Ty::Unknown {
                    Expectation::has_type(self.new_type_var())
                } else {
                    expected.clone()
                };
                let input_ty = self.infer_expr(*expr, &Expectation::none());

                for arm in arms {
                    for &pat in &arm.pats {
                        let _pat_ty = self.infer_pat(pat, &input_ty);
                    }
                    // TODO type the guard
                    self.infer_expr(arm.expr, &expected);
                }

                expected.ty
            }
            Expr::Path(p) => self.infer_path_expr(expr, p).unwrap_or(Ty::Unknown),
            Expr::Continue => Ty::Never,
            Expr::Break { expr } => {
                if let Some(expr) = expr {
                    // TODO handle break with value
                    self.infer_expr(*expr, &Expectation::none());
                }
                Ty::Never
            }
            Expr::Return { expr } => {
                if let Some(expr) = expr {
                    self.infer_expr(*expr, &Expectation::has_type(self.return_ty.clone()));
                }
                Ty::Never
            }
            Expr::StructLit {
                path,
                fields,
                spread,
            } => {
                let (ty, def_id) = self.resolve_variant(path.as_ref());
                let substs = ty.substs().unwrap_or_else(Substs::empty);
                for field in fields {
                    let field_ty = if let Some(def_id) = def_id {
                        self.db
                            .type_for_field(def_id, field.name.clone())
                            .unwrap_or(Ty::Unknown)
                            .subst(&substs)
                    } else {
                        Ty::Unknown
                    };
                    self.infer_expr(field.expr, &Expectation::has_type(field_ty));
                }
                if let Some(expr) = spread {
                    self.infer_expr(*expr, &Expectation::has_type(ty.clone()));
                }
                ty
            }
            Expr::Field { expr, name } => {
                let receiver_ty = self.infer_expr(*expr, &Expectation::none());
                let ty = receiver_ty
                    .autoderef(self.db)
                    .find_map(|derefed_ty| match derefed_ty {
                        // this is more complicated than necessary because type_for_field is cancelable
                        Ty::Tuple(fields) => {
                            let i = name.to_string().parse::<usize>().ok();
                            i.and_then(|i| fields.get(i).cloned())
                        }
                        Ty::Adt {
                            def_id, ref substs, ..
                        } => self
                            .db
                            .type_for_field(def_id, name.clone())
                            .map(|ty| ty.subst(substs)),
                        _ => None,
                    })
                    .unwrap_or(Ty::Unknown);
                self.insert_type_vars(ty)
            }
            Expr::Try { expr } => {
                let _inner_ty = self.infer_expr(*expr, &Expectation::none());
                Ty::Unknown
            }
            Expr::Cast { expr, type_ref } => {
                let _inner_ty = self.infer_expr(*expr, &Expectation::none());
                let cast_ty = self.make_ty(type_ref);
                // TODO check the cast...
                cast_ty
            }
            Expr::Ref { expr, mutability } => {
                // TODO pass the expectation down
                let inner_ty = self.infer_expr(*expr, &Expectation::none());
                // TODO reference coercions etc.
                Ty::Ref(Arc::new(inner_ty), *mutability)
            }
            Expr::UnaryOp { expr, op } => {
                let inner_ty = self.infer_expr(*expr, &Expectation::none());
                match op {
                    UnaryOp::Deref => {
                        if let Some(derefed_ty) = inner_ty.builtin_deref() {
                            derefed_ty
                        } else {
                            // TODO Deref::deref
                            Ty::Unknown
                        }
                    }
                    UnaryOp::Neg => {
                        match inner_ty {
                            Ty::Int(primitive::UncertainIntTy::Unknown)
                            | Ty::Int(primitive::UncertainIntTy::Signed(..))
                            | Ty::Infer(InferTy::IntVar(..))
                            | Ty::Infer(InferTy::FloatVar(..))
                            | Ty::Float(..) => inner_ty,
                            // TODO: resolve ops::Neg trait
                            _ => Ty::Unknown,
                        }
                    }
                    UnaryOp::Not if inner_ty == Ty::Bool => Ty::Bool,
                    // TODO: resolve ops::Not trait for inner_ty
                    UnaryOp::Not => Ty::Unknown,
                }
            }
            Expr::BinaryOp { lhs, rhs, op } => match op {
                Some(op) => {
                    let lhs_expectation = match op {
                        BinaryOp::BooleanAnd | BinaryOp::BooleanOr => {
                            Expectation::has_type(Ty::Bool)
                        }
                        _ => Expectation::none(),
                    };
                    let lhs_ty = self.infer_expr(*lhs, &lhs_expectation);
                    // TODO: find implementation of trait corresponding to operation
                    // symbol and resolve associated `Output` type
                    let rhs_expectation = binary_op_rhs_expectation(*op, lhs_ty);
                    let rhs_ty = self.infer_expr(*rhs, &Expectation::has_type(rhs_expectation));

                    // TODO: similar as above, return ty is often associated trait type
                    binary_op_return_ty(*op, rhs_ty)
                }
                _ => Ty::Unknown,
            },
            Expr::Tuple { exprs } => {
                let mut ty_vec = Vec::with_capacity(exprs.len());
                for arg in exprs.iter() {
                    ty_vec.push(self.infer_expr(*arg, &Expectation::none()));
                }

                Ty::Tuple(Arc::from(ty_vec))
            }
            Expr::Array { exprs } => {
                let elem_ty = match &expected.ty {
                    Ty::Slice(inner) | Ty::Array(inner) => Ty::clone(&inner),
                    _ => self.new_type_var(),
                };

                for expr in exprs.iter() {
                    self.infer_expr(*expr, &Expectation::has_type(elem_ty.clone()));
                }

                Ty::Array(Arc::new(elem_ty))
            }
            Expr::Literal(lit) => match lit {
                Literal::Bool(..) => Ty::Bool,
                Literal::String(..) => Ty::Ref(Arc::new(Ty::Str), Mutability::Shared),
                Literal::ByteString(..) => {
                    let byte_type = Arc::new(Ty::Int(primitive::UncertainIntTy::Unsigned(
                        primitive::UintTy::U8,
                    )));
                    let slice_type = Arc::new(Ty::Slice(byte_type));
                    Ty::Ref(slice_type, Mutability::Shared)
                }
                Literal::Char(..) => Ty::Char,
                Literal::Int(_v, ty) => Ty::Int(*ty),
                Literal::Float(_v, ty) => Ty::Float(*ty),
            },
        };
        // use a new type variable if we got Ty::Unknown here
        let ty = self.insert_type_vars_shallow(ty);
        self.unify(&ty, &expected.ty);
        let ty = self.resolve_ty_as_possible(ty);
        self.write_expr_ty(expr, ty.clone());
        ty
    }

    fn infer_block(
        &mut self,
        statements: &[Statement],
        tail: Option<ExprId>,
        expected: &Expectation,
    ) -> Ty {
        for stmt in statements {
            match stmt {
                Statement::Let {
                    pat,
                    type_ref,
                    initializer,
                } => {
                    let decl_ty = type_ref
                        .as_ref()
                        .map(|tr| self.make_ty(tr))
                        .unwrap_or(Ty::Unknown);
                    let decl_ty = self.insert_type_vars(decl_ty);
                    let ty = if let Some(expr) = initializer {
                        let expr_ty = self.infer_expr(*expr, &Expectation::has_type(decl_ty));
                        expr_ty
                    } else {
                        decl_ty
                    };

                    self.infer_pat(*pat, &ty);
                }
                Statement::Expr(expr) => {
                    self.infer_expr(*expr, &Expectation::none());
                }
            }
        }
        let ty = if let Some(expr) = tail {
            self.infer_expr(expr, expected)
        } else {
            Ty::unit()
        };
        ty
    }

    fn collect_fn_signature(&mut self, signature: &FnSignature) {
        let body = Arc::clone(&self.body); // avoid borrow checker problem
        for (type_ref, pat) in signature.params().iter().zip(body.params()) {
            let ty = self.make_ty(type_ref);

            self.infer_pat(*pat, &ty);
        }
        self.return_ty = {
            let ty = self.make_ty(signature.ret_type());
            ty
        };
    }

    fn infer_body(&mut self) {
        self.infer_expr(
            self.body.body_expr(),
            &Expectation::has_type(self.return_ty.clone()),
        );
    }
}

pub fn infer(db: &impl HirDatabase, def_id: DefId) -> Arc<InferenceResult> {
    db.check_canceled();
    let function = Function::new(def_id); // TODO: consts also need inference
    let body = function.body(db);
    let scopes = db.fn_scopes(def_id);
    let module = function.module(db);
    let impl_block = function.impl_block(db);
    let mut ctx = InferenceContext::new(db, body, scopes, module, impl_block);

    let signature = function.signature(db);
    ctx.collect_fn_signature(&signature);

    ctx.infer_body();

    Arc::new(ctx.resolve_all())
}
