//! Item AST — every top-level declaration form: functions, structs,
//! enums, traits, impl blocks, effect declarations, layouts, modules,
//! consts, externs, type aliases, distinct types.
//!
//! `Item` is the discriminator; each variant wraps a per-kind struct
//! holding the parsed shape plus attributes / doc comments / spans.

use crate::token::Span;

use super::{
    Attribute, Block, Expr, GenericParams, PathExpr, Pattern, PatternKind, TraitBound, TypeExpr,
    Visibility, WhereClause,
};

// ── Top-level Items ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Item {
    Function(Function),
    StructDef(StructDef),
    EnumDef(EnumDef),
    TraitDef(TraitDef),
    /// `trait NAME[GENERICS] = bound1 + bound2 + ... [where ...];` — a
    /// trait alias declaration. v60 item 40 / design.md § Trait Aliases.
    /// At v1 the parser, AST, and resolver recognize the form; the
    /// typechecker emits a stub diagnostic (`E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET`)
    /// at every use site. Bound substitution lands in P1.
    TraitAlias(TraitAliasDef),
    /// `marker trait NAME[GENERICS] [: SUPERTRAITS] [where ...] ;` (or
    /// `{ }`). Method-less trait used as a typing tag — the body must be
    /// empty, and impls of the trait must be empty. v60 item 55 /
    /// design.md § Marker Traits.
    MarkerTrait(MarkerTraitDef),
    ImplBlock(ImplBlock),
    EffectResource(EffectResourceDecl),
    EffectGroup(EffectGroupDecl),
    EffectVerbDecl(EffectVerbDecl),
    LayoutDef(LayoutDef),
    UseDecl(UseDecl),
    Import(ImportDecl),
    ConstDecl(ConstDecl),
    AliasDecl(AliasDecl),
    IndependentDecl(IndependentDecl),
    ExternFunction(ExternFunction),
    /// `unsafe extern "ABI" { ... }` block — the trust-boundary form for
    /// foreign-import declarations. The bare standalone `extern "C" fn
    /// name(...);` shape is rejected at module scope; foreign imports
    /// must live inside one of these blocks. Block-level attributes
    /// (`@noblock`, `#[noblock]`) are pre-merged into each contained
    /// item's `attributes` at parse time, so downstream phases process
    /// the inner `ExternFunction`s identically to today's standalone
    /// form. The block itself carries the abi/doc/attributes for
    /// block-scoped consumers (e.g. the `undocumented_unsafe` lint).
    ExternBlock(ExternBlock),
    TypeAlias(TypeAliasDef),
    DistinctType(DistinctTypeDef),
}

// ── Functions ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Function {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments immediately preceding the
    /// item. `None` when no doc comments were attached. CommonMark.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    /// `unsafe fn ...` declaration marker. The `unsafe` keyword on a fn
    /// declaration is a *precondition* the function asserts callers must
    /// satisfy — it is NOT an implicit `unsafe { ... }` block around the
    /// body. The `unsafe_op_in_unsafe_fn` rule (slice 3 of the v2 unsafe
    /// epic) walks every fn body uniformly and requires raw-ptr derefs,
    /// calls to other `unsafe fn`s, asm intrinsics, volatile reads/writes,
    /// and union field access to be wrapped in `unsafe { ... }` even
    /// inside an `unsafe fn` body. Slice 1 only captures the surface
    /// marker; the lint that consumes it lands in slice 3.
    pub is_unsafe: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub params: Vec<Param>,
    pub self_param: Option<SelfParam>,
    pub return_type: Option<TypeExpr>,
    pub effects: Option<EffectList>,
    pub requires: Vec<Expr>,
    pub ensures: Vec<EnsuresClause>,
    pub where_clause: Option<WhereClause>,
    pub body: Block,
    /// True iff this item came from baked stdlib source (CR-202 slice 3b).
    /// The parser writes `false`; the bake step in `prelude.rs` flips it
    /// to `true` after parsing. The resolver's `#[compiler_builtin]` gate
    /// (`E0237`) bypasses items where this is `true` so stdlib source can
    /// use the attribute even when the resolver's session-wide
    /// `is_stdlib_source` flag is unset (e.g. when the bake AST is
    /// spliced into a user-mode program tree).
    pub stdlib_origin: bool,
}

#[derive(Debug, Clone)]
pub struct EnsuresClause {
    pub param: Option<String>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelfParam {
    Owned,  // self (owned/consuming receiver — the default; `own self` is rejected)
    Ref,    // ref self
    MutRef, // mut ref self
}

#[derive(Debug, Clone)]
pub struct Param {
    pub span: Span,
    pub pattern: Pattern,
    pub ty: TypeExpr,
    pub default_value: Option<Expr>,
    pub doc_comment: Option<String>,
}

impl Param {
    /// Returns the simple name if this param is a plain binding (not a destructuring pattern).
    pub fn name(&self) -> Option<&str> {
        if let PatternKind::Binding(ref name) = self.pattern.kind {
            Some(name)
        } else {
            None
        }
    }
}

/// Items that carry a three-level visibility (pub / default / private).
/// A tiny helper trait lets the resolver / typechecker treat them
/// uniformly without each call site having to read both bool fields.
macro_rules! impl_vis {
    ($ty:ident) => {
        impl $ty {
            pub fn visibility(&self) -> Visibility {
                Visibility::from_flags(self.is_pub, self.is_private)
            }
        }
    };
}

impl_vis!(Function);
impl_vis!(StructDef);
impl_vis!(EnumDef);
impl_vis!(TraitDef);
impl_vis!(ConstDecl);
impl_vis!(ExternFunction);
impl_vis!(OpaqueTypeDecl);
impl_vis!(TypeAliasDef);
impl_vis!(DistinctTypeDef);

// ── Structs ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StructDef {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments preceding the struct.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub is_shared: bool,
    pub no_rc: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub where_clause: Option<WhereClause>,
    pub fields: Vec<StructField>,
    pub invariants: Vec<Expr>,
    /// See [`Function::stdlib_origin`]. CR-202 slice 3b.
    pub stdlib_origin: bool,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments preceding the field. `None`
    /// when no doc comments were attached. CommonMark, same shape as the
    /// item-level `doc_comment` fields.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_mut: bool,
    pub name: String,
    pub ty: TypeExpr,
}

// ── Enums ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments preceding the enum.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub is_shared: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub where_clause: Option<WhereClause>,
    pub variants: Vec<Variant>,
    /// See [`Function::stdlib_origin`]. CR-202 slice 3b.
    pub stdlib_origin: bool,
}

#[derive(Debug, Clone)]
pub struct Variant {
    pub span: Span,
    /// Joined contents of `///` doc comments preceding the variant.
    /// `None` when no doc comments were attached. CommonMark.
    pub doc_comment: Option<String>,
    pub name: String,
    pub kind: VariantKind,
}

#[derive(Debug, Clone)]
pub enum VariantKind {
    Unit,
    Tuple(Vec<TypeExpr>),
    Struct(Vec<StructField>),
}

// ── Traits ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TraitDef {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments preceding the trait.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    /// `trait Foo: Bar + Baz` — logical supertrait constraints.
    pub supertraits: Vec<TraitBound>,
    /// `trait Foo with reads(R)` — default effect ceiling for all methods.
    pub trait_effects: Option<EffectList>,
    pub where_clause: Option<WhereClause>,
    pub items: Vec<TraitItem>,
    /// See [`Function::stdlib_origin`]. CR-202 slice 3b.
    pub stdlib_origin: bool,
}

#[derive(Debug, Clone)]
pub enum TraitItem {
    Method(Box<TraitMethod>),
    AssocType(AssocTypeDecl),
}

/// `trait NAME[GENERICS] = bound1 + bound2 + ... [where ...];`
/// (v60 item 40 / design.md § Trait Aliases). Parallel in shape to
/// `TraitDef` so resolver reuse is mechanical; bound substitution at
/// use sites is deferred to P1.
#[derive(Debug, Clone)]
pub struct TraitAliasDef {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub bounds: Vec<TraitBound>,
    pub where_clause: Option<WhereClause>,
}

/// `marker trait NAME[GENERICS] [: SUPERTRAITS] [where ...] (";" | "{" "}")`
/// (v60 item 55 / design.md § Marker Traits). The body is structurally
/// empty — methods, associated types, and associated consts are
/// rejected at parse with a focused diagnostic. The `body_brace`
/// flag preserves whether the user wrote the empty-brace form
/// (`marker trait Foo { }`) or the canonical short form
/// (`marker trait Foo;`) for round-trip-faithful formatting.
#[derive(Debug, Clone)]
pub struct MarkerTraitDef {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub supertraits: Vec<TraitBound>,
    pub where_clause: Option<WhereClause>,
    /// `true` when the user wrote `marker trait Foo { }`; `false` when
    /// they wrote the canonical `marker trait Foo;`. Drives
    /// formatter round-trip; the resolver treats both forms identically.
    pub body_brace: bool,
}

#[derive(Debug, Clone)]
pub struct AssocTypeDecl {
    pub span: Span,
    pub name: String,
    pub bounds: Vec<TraitBound>,
}

#[derive(Debug, Clone)]
pub struct TraitMethod {
    pub span: Span,
    /// `unsafe fn ...` in a trait-method declaration. Mirrors the
    /// `Function.is_unsafe` precondition role: an impl satisfying this
    /// trait method must itself be `unsafe fn`, and every call site
    /// against the trait must be wrapped per `unsafe_op_in_unsafe_fn`.
    /// Slice 1 captures the surface marker only; the impl/trait
    /// coherence check that an `unsafe`-trait-method is implemented
    /// by an `unsafe fn` is part of the wider v2 epic.
    pub is_unsafe: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub self_param: Option<SelfParam>,
    pub params: Vec<Param>,
    pub return_type: Option<TypeExpr>,
    pub effects: Option<EffectList>,
    pub requires: Vec<Expr>,
    pub ensures: Vec<EnsuresClause>,
    pub where_clause: Option<WhereClause>,
    pub body: Option<Block>,
}

// ── Impl Blocks ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub generic_params: Option<GenericParams>,
    pub trait_name: Option<PathExpr>,
    pub target_type: TypeExpr,
    pub where_clause: Option<WhereClause>,
    pub items: Vec<ImplItem>,
}

#[derive(Debug, Clone)]
pub enum ImplItem {
    Method(Box<Function>),
    AssocType(AssocTypeBinding),
}

#[derive(Debug, Clone)]
pub struct AssocTypeBinding {
    pub span: Span,
    pub name: String,
    pub ty: TypeExpr,
}

// ── Effect Declarations ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EffectResourceDecl {
    pub span: Span,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub provider_trait: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EffectGroupDecl {
    pub span: Span,
    pub is_pub: bool,
    pub is_stable: bool,
    pub name: String,
    pub body: Vec<EffectGroupTerm>,
}

#[derive(Debug, Clone)]
pub enum EffectGroupTerm {
    Verb(EffectVerb),
    GroupRef(String),
}

#[derive(Debug, Clone)]
pub struct EffectVerbDecl {
    pub span: Span,
    pub is_pub: bool,
    pub is_transparent: bool,
    pub verb_name: String,
}

// ── Effect Annotations ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EffectList {
    pub items: Vec<EffectItem>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum EffectItem {
    Verb(EffectVerb),
    Group(String),
    Polymorphic,      // with _
    Variable(String), // with E  (named effect variable declared in [with E])
}

#[derive(Debug, Clone)]
pub struct EffectVerb {
    pub kind: EffectVerbKind,
    pub resources: Vec<Resource>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EffectVerbKind {
    Reads,
    Writes,
    Sends,
    Receives,
    Allocates,
    Panics,
    Blocks,
    Suspends,
    UserDefined(String),
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub path: Vec<String>,
    pub param: Option<Box<Expr>>,
    pub span: Span,
}

// ── Layout ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LayoutDef {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments preceding the layout.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub name: String,
    pub collection_type: TypeExpr,
    pub items: Vec<LayoutItem>,
}

#[derive(Debug, Clone)]
pub enum LayoutItem {
    Group {
        name: String,
        fields: Vec<String>,
        /// Optional `align(N)` modifier — N must be a power of two.
        align: Option<u32>,
        span: Span,
    },
    Cold {
        fields: Vec<String>,
        span: Span,
    },
    SplitByVariant(Span),
}

// ── Module & Import ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UseDecl {
    pub span: Span,
    pub is_pub: bool,
    pub path: Vec<String>,
}

/// New module-system import (CR-24). Replaces `UseDecl` once the parser emits
/// `import` syntax in slice 5. Supports brace-grouped multi-item imports and
/// per-item `as` renames:
///
/// ```text
/// import db.connection.Connection;
/// import db.connection.{Connection, Pool as P};
/// pub import db.auth.Token;
/// ```
///
/// `path` is the module prefix (dotted path, no item), and `items` lists the
/// names being imported from that module. A bare `import path.Item;` maps to
/// a single-element `items` vector.
#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub span: Span,
    pub is_pub: bool,
    pub path: Vec<String>,
    /// Per-segment spans for `path` (same length, same order). Lets resolver
    /// diagnostics target a precise sub-range of the import statement when a
    /// prefix segment is misspelled — e.g., `import grret.foo;` gets a
    /// machine-applicable replacement covering only `grret.foo`, not the
    /// entire import statement.
    pub path_spans: Vec<Span>,
    pub items: Vec<ImportItem>,
}

#[derive(Debug, Clone)]
pub struct ImportItem {
    pub span: Span,
    pub name: String,
    pub alias: Option<String>,
}

// ── Constants ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ConstDecl {
    pub span: Span,
    /// Joined contents of `///` doc comments preceding the const.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub name: String,
    pub ty: TypeExpr,
    pub value: Expr,
}

// ── Alias & Independent ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AliasDecl {
    pub span: Span,
    pub left: Vec<String>,
    pub right: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IndependentDecl {
    pub span: Span,
    pub left: Vec<String>,
    pub right: Vec<String>,
}

// ── Extern Functions (FFI) ───────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExternFunction {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments preceding the extern.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub abi: String,
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: Option<TypeExpr>,
    pub effects: Option<EffectList>,
}

/// `unsafe extern "ABI" { ... }` block — see [`Item::ExternBlock`].
#[derive(Debug, Clone)]
pub struct ExternBlock {
    pub span: Span,
    /// Block-level attributes (e.g. block-scoped `@noblock`). Held
    /// here at the block level and NOT pre-merged into per-item
    /// `attributes` — formatter round-trip needs to preserve which
    /// attributes were authored block-level vs. per-item. Downstream
    /// consumers (effectchecker, codegen) that need the effective union
    /// for a given item take both sets explicitly.
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc-comments preceding the block. The
    /// `undocumented_unsafe` lint reads this to enforce a `# Safety`
    /// section at the block level.
    pub doc_comment: Option<String>,
    pub abi: String,
    pub items: Vec<ExternItem>,
}

#[derive(Debug, Clone)]
pub enum ExternItem {
    /// Boxed to keep the `ExternItem` enum compact: `ExternFunction`
    /// is ~344 bytes (params, return type, effects, attributes) while
    /// `OpaqueTypeDecl` is ~112 bytes — boxing the larger variant
    /// keeps the discriminated union shrinkwrap to one pointer per
    /// item without churning size on the rare `OpaqueType` arm.
    Function(Box<ExternFunction>),
    /// Opaque foreign type declaration: `type Name;` inside an
    /// `unsafe extern "ABI" { ... }` block. Names a C-side type whose
    /// layout is unknown to Kāra (`FILE`, `xmlNode`, `sqlite3`, etc.) —
    /// see [`OpaqueTypeDecl`] and design.md § FFI > Opaque Foreign Types.
    OpaqueType(OpaqueTypeDecl),
}

/// `type Name;` declaration inside an `unsafe extern "ABI" { ... }` block.
/// Carries no fields, no methods, no derives, no body — the type's
/// layout is private to the foreign library. The Kāra side may only
/// reference it behind a pointer (`*const`/`*mut`) or reference
/// (`ref`/`mut ref`); by-value uses are rejected at typecheck.
#[derive(Debug, Clone)]
pub struct OpaqueTypeDecl {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments preceding the decl.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub name: String,
}

// ── Type Aliases ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TypeAliasDef {
    pub span: Span,
    /// Joined contents of `///` doc comments preceding the alias.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub ty: TypeExpr,
    pub refinement: Option<Expr>,
}

// ── Distinct Types (Newtypes) ────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DistinctTypeDef {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    /// Joined contents of `///` doc comments preceding the distinct type.
    pub doc_comment: Option<String>,
    pub is_pub: bool,
    pub is_private: bool,
    pub name: String,
    pub generic_params: Option<GenericParams>,
    pub base_type: TypeExpr,
    pub refinement: Option<Expr>,
}
