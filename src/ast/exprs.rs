//! Expression AST — every expression form (literals, calls, blocks,
//! control flow, loops, closures, ranges, patterns-in-expression
//! positions, etc.) plus the operator and label/capture-mode enums and
//! the call-argument and struct-init field shapes used in expression
//! positions.

use crate::token::{FloatSuffix, IntSuffix, Span};

use super::{Block, GenericArg, MatchArm, Pattern, TypeExpr};

// ── Expressions ──────────────────────────────────────────────────

/// A part of a parsed f-string — static text or a fully-parsed expression.
/// Replaces `token::InterpolationPart::Expr(raw_string)` after the parser
/// sub-parses each interpolation hole at parse time.
#[derive(Debug, Clone)]
pub enum ParsedInterpolationPart {
    Text(String),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    // Literals
    Integer(i64, Option<IntSuffix>),
    Float(f64, Option<FloatSuffix>),
    CharLit(char),
    StringLit(String),
    MultiStringLit(String),
    InterpolatedStringLit(Vec<ParsedInterpolationPart>),
    Bool(bool),

    // Identifiers
    Identifier(String),
    Path {
        segments: Vec<String>,
        /// Mixed type / const generic arguments at the expression position.
        /// Const generics slice 1b (2026-05-11) widened this from
        /// `Vec<TypeExpr>` to `Vec<GenericArg>` so call-site expressions
        /// like `make_arr[i64, 4]()` carry the `4` literal through to the
        /// codegen mango key.
        generic_args: Option<Vec<GenericArg>>,
    },
    SelfValue,
    SelfType,

    // Operators
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },

    // Postfix
    Question(Box<Expr>),
    OptionalChain {
        object: Box<Expr>,
        field_or_method: String,
        args: Option<Vec<CallArg>>, // None for field, Some for method
    },

    // Infix
    NilCoalesce {
        left: Box<Expr>,
        right: Box<Expr>,
    },

    Call {
        callee: Box<Expr>,
        args: Vec<CallArg>,
    },
    MethodCall {
        object: Box<Expr>,
        method: String,
        turbofish: Option<Vec<TypeExpr>>,
        args: Vec<CallArg>,
    },
    FieldAccess {
        object: Box<Expr>,
        field: String,
    },
    TupleIndex {
        object: Box<Expr>,
        index: u64,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },

    // Compound expressions
    Block(Block),
    If {
        condition: Box<Expr>,
        then_block: Block,
        else_branch: Option<Box<Expr>>,
    },
    IfLet {
        pattern: Pattern,
        value: Box<Expr>,
        then_block: Block,
        else_branch: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    While {
        label: Option<String>,
        condition: Box<Expr>,
        body: Block,
    },
    WhileLet {
        label: Option<String>,
        pattern: Pattern,
        value: Box<Expr>,
        body: Block,
    },
    For {
        label: Option<String>,
        pattern: Pattern,
        iterable: Box<Expr>,
        body: Block,
    },
    Loop {
        label: Option<String>,
        body: Block,
    },
    /// Labeled block expression — `label: { ... }` (design.md § Loops >
    /// Labeled blocks; syntax.md §5.3). The block becomes a `break` target
    /// (with optional value); `continue label` referring to a labeled block
    /// is rejected by the resolver. The block's type is the LUB of all
    /// reachable `break label expr` value sites and the tail expression.
    /// Unlabeled blocks continue to use `ExprKind::Block` — the
    /// `LabeledBlock` variant is added rather than mutating `Block` so
    /// existing AST consumers (which heavily destructure `Block`) keep
    /// working unchanged.
    LabeledBlock {
        label: String,
        /// Source span of the label identifier (the `IDENT` before the
        /// colon). Threaded through for diagnostic span fidelity —
        /// `error[E_CONTINUE_LABEL_BLOCK]` points its secondary span at
        /// the label binding using this.
        label_span: Span,
        body: Block,
    },
    Closure {
        params: Vec<ClosureParam>,
        /// Explicit per-closure capture-mode prefix (design.md § Closures,
        /// Rule 2½). `None` = bare `|...|` — each capture's mode is
        /// inferred from the body's first classifying use per Rule 2
        /// (read → `Ref`, mutate → `MutRef`, consume → `Own`).
        /// `Some(Own | Ref | MutRef)` = explicit prefix pinning every
        /// captured path to the declared mode; the ownership checker
        /// fires K2 violations when body usage exceeds the declared
        /// mode (consume under `ref` / `mut ref`) and a perf note when
        /// `mut ref` is declared but the body only reads.
        capture_mode: Option<CaptureMode>,
        /// Span of the explicit prefix tokens (`mut ref` / `ref` / `own` /
        /// `move`) when present. `None` for bare `|...|` closures. Lets
        /// diagnostics target the prefix region precisely — used by N0507
        /// (UnusedMutCaptureNote) to attach a machine-applicable
        /// `mut ref` → `ref` rewrite without disturbing the closure body.
        prefix_span: Option<Span>,
        body: Box<Expr>,
    },
    Return(Option<Box<Expr>>),
    Break {
        label: Option<String>,
        value: Option<Box<Expr>>,
    },
    Continue {
        label: Option<String>,
    },

    // Composite literals
    Tuple(Vec<Expr>),
    ArrayLiteral(Vec<Expr>),
    /// `TypeName[e1, e2, ...]` — prefix collection literal.
    /// `type_name` is one of `Vec`, `Array`, `Set`, `Map`.
    /// `Array[e1, e2, e3]` produces a fixed-size array; `Vec[...]` produces a growable vec.
    PrefixCollectionLiteral {
        type_name: String,
        items: Vec<Expr>,
    },
    /// `[value; count]` (bare) or `Vec[value; count]` / `Array[value; count]`
    /// (prefix). Equivalent to a literal with `count` copies of `value`. Bare
    /// form defaults to `Vec[T]` in synthesis mode and coerces to `Array[T, N]`
    /// in check mode against an Array-typed expected. `Array[v; n]` requires
    /// `count` to be a compile-time integer literal. Restricted to `Vec` /
    /// `Array` only; repeating into `Set` / `Map` is rejected.
    RepeatLiteral {
        /// `None` → bare `[v; n]`; `Some("Vec")` / `Some("Array")` → prefix form.
        type_name: Option<String>,
        value: Box<Expr>,
        count: Box<Expr>,
    },
    MapLiteral(Vec<(Expr, Expr)>),
    StructLiteral {
        path: Vec<String>,
        fields: Vec<FieldInit>,
        spread: Option<Box<Expr>>,
    },

    // Pipe
    Pipe {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `_` placeholder in pipe argument position — replaced by left-hand value during desugaring
    PipePlaceholder,

    // Cast
    Cast {
        expr: Box<Expr>,
        ty: TypeExpr,
    },

    /// `offset_of[T](field.path)` — compile-time byte offset of a field
    /// (or nested field path) from the start of a value of type `T`.
    /// Parser special form because the second argument is a field-name
    /// path, not a value expression. The typechecker walks `field_path`
    /// against `T`'s declared fields, validating each segment and
    /// emitting `E_OFFSET_OF_OPAQUE_TYPE` / `E_OFFSET_OF_GENERIC_PARAM`
    /// / `E_OFFSET_OF_UNKNOWN_FIELD` / `E_OFFSET_OF_PRIVATE_FIELD` /
    /// `E_OFFSET_OF_ENUM_VARIANT` as appropriate. The codegen lowers
    /// to inkwell's `TargetData::offset_of_element` (chained for
    /// nested paths). Returns `usize`. See `design.md § Field Offsets`.
    OffsetOf {
        ty: TypeExpr,
        field_path: Vec<String>,
    },

    // Range — start and/or end may be absent for half-open forms.
    // `a..b`   → start=Some, end=Some, inclusive=false  → Range[T]
    // `a..=b`  → start=Some, end=Some, inclusive=true   → RangeInclusive[T]
    // `a..`    → start=Some, end=None, inclusive=false  → RangeFrom[T]
    // `..b`    → start=None, end=Some, inclusive=false  → RangeTo[T]
    // `..=b`   → start=None, end=Some, inclusive=true   → RangeToInclusive[T]
    // `..`     → start=None, end=None, inclusive=false  → RangeFull
    Range {
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    },

    // Unsafe
    Unsafe(Block),

    /// `try { ... }` — try block. The body may use `?` to short-circuit
    /// out of the block; the block itself produces a `Result`-shaped
    /// value. Parsed at v1; the typechecker pipeline (?-retargeting
    /// against the block, error-type unification, From-chain coercion)
    /// lands in P1. See design.md § Error Handling > Try Blocks.
    Try(Block),

    // Sequential block (suppresses auto-parallelism)
    Seq(Block),

    // Parallel block (explicit fork-join)
    Par(Block),

    // Lock block
    Lock {
        mutex: String,
        alias: Option<String>,
        body: Block,
    },

    // `providers { R => p, ... } in { body }` — multi-provider bootstrapping
    // (design.md § `providers { } in { }` Block).
    Providers {
        bindings: Vec<ProviderBinding>,
        body: Block,
    },

    // Error recovery placeholder
    Error,
}

#[derive(Debug, Clone)]
pub struct ProviderBinding {
    pub resource: String,
    pub resource_span: Span,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    // Comparison
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    // Logical
    And,
    Or,
    // Bitwise
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    // Range
    Range,
    RangeInclusive,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnaryOp {
    Neg,    // -
    Not,    // !
    BitNot, // ~
    Deref,  // *
}

// ── Closures ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ClosureParam {
    pub pattern: Pattern,
    pub ty: Option<TypeExpr>,
    pub span: Span,
}

/// Discriminator for active label-stack entries — distinguishes labeled
/// loops (which accept both `break label` and `continue label`) from
/// labeled blocks (which accept `break label` only). Carried alongside
/// the label name in the parser's and resolver's label stacks; the
/// resolver consults this when validating `continue label` targets.
/// See design.md § Loops > "Labeled blocks".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    Loop,
    Block,
}

/// Explicit closure capture-mode prefix (design.md § Closure Behavior, Rule 2½).
/// Bare `|...|` (no prefix) runs per-capture-path inference; the three variants
/// here pin every captured path to the declared mode. `Own` is Kāra's spelling
/// of capture-by-value; the Rust idiom `move` is rejected with a redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Own,
    Ref,
    MutRef,
}

// ── Call Arguments ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CallArg {
    pub label: Option<String>,
    /// Call-site mutation marker (`mut <expr>`). Required for fresh bindings
    /// passed to `mut ref T` / `mut Slice[T]` parameters; rejected elsewhere.
    /// See design.md Feature 4 Part 1½: Call-site Mutation Markers.
    pub mut_marker: bool,
    pub value: Expr,
    pub span: Span,
}

// ── Struct Literal Fields ────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FieldInit {
    pub name: String,
    pub value: Expr,
    pub shorthand: bool, // true for `Point { x }` (name == value identifier)
    pub span: Span,
}
