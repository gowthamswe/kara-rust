// src/token.rs

//! Defines the tokens produced by the Kāra lexer.

/// Source location attached to every token.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Span {
    pub line: usize,
    pub column: usize,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum IntSuffix {
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum FloatSuffix {
    F32,
    F64,
}

#[derive(Debug, PartialEq, Clone)]
pub enum InterpolationPart {
    Text(String),
    Expr(String), // The raw string of the expression, parsing it is deferred to the parser
}

/// A token with its source location.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub span: Span,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Token {
    // ── Keywords ──────────────────────────────────────────────
    // Declarations
    Fn,
    Struct,
    Enum,
    Trait,
    Impl,
    Mod,
    Use,
    Import,
    Const,
    Type,
    // Visibility
    Pub,
    Private,
    // Control flow
    If,
    Else,
    Match,
    While,
    For,
    In,
    Loop,
    Return,
    Break,
    Continue,
    // Bindings
    Let,
    Mut,
    // Logical operators (keyword forms; symbol `&&`/`||`/`!` are not accepted)
    And,
    Or,
    Not,
    // Ownership
    Own,
    Ref,
    Weak,
    Lock,
    // Closure capture (kept reserved for future ref/mut-ref capture work; bare `|...|` is owned-by-default and `move` is not used)
    Move,
    // Effects
    Effect,
    Resource,
    Verb,
    Reads,
    Writes,
    Sends,
    Receives,
    Allocates,
    Panics,
    Blocks,
    Suspends,
    With,
    Transparent,
    Stable,
    Seq,
    Par,
    Yield,
    // Type system
    As,
    Where,
    Dyn,
    // Safety
    Unsafe,
    Extern,
    // Shared
    Shared,
    // Layout
    Layout,
    Group,
    // Literals
    True,
    False,
    // Contracts
    Requires,
    Ensures,
    Invariant,
    // Defer
    Defer,
    ErrDefer,
    /// `try { ... }` — try block. Parsed at v1; the typechecker pipeline
    /// (?-retargeting + error-type unification) lands in P1. See
    /// design.md § Error Handling > Try Blocks (try { ... }).
    Try,
    /// `marker trait NAME;` — marker-trait declaration. Per design.md §
    /// Marker Traits (v60 item 55). Users with a local binding named
    /// `marker` must rename or use `r#marker`.
    Marker,
    // Assembly
    Asm,
    GlobalAsm,
    // `providers` is parsed as a contextual keyword: the lexer emits it
    // as `Identifier { name: "providers" }`, and the parser dispatches to
    // `parse_providers_block` when an identifier expression named
    // "providers" is followed by `{`. This frees the bareword for module
    // names, function names, variable bindings, etc. (e.g.,
    // `examples/parallax/src/providers.kara`).
    // Other
    Distinct,
    Alias,
    Independent,
    SelfValue, // self
    SelfType,  // Self

    // ── Symbols ───────────────────────────────────────────────
    // Delimiters
    LeftParen,    // (
    RightParen,   // )
    LeftBrace,    // {
    RightBrace,   // }
    LeftBracket,  // [
    RightBracket, // ]

    // Punctuation
    Colon,            // :
    Comma,            // ,
    Semicolon,        // ;
    Dot,              // .
    DotDot,           // ..
    DotDotEq,         // ..=
    QuestionDot,      // ?.
    QuestionQuestion, // ??
    Arrow,            // ->
    FatArrow,         // =>
    Question,         // ?
    Pound,            // #
    Underscore,       // _ (as a token, e.g., in patterns)
    At,               // @ (pattern bindings)

    // Arithmetic
    Plus,    // +
    Minus,   // -
    Star,    // *
    Slash,   // /
    Percent, // %

    // Comparison
    EqualEqual,         // ==
    BangEqual,          // !=
    LessThan,           // <
    LessThanOrEqual,    // <=
    GreaterThan,        // >
    GreaterThanOrEqual, // >=

    // Logical (legacy — produce an error in parse position; still produced by the
    // lexer so the parser can emit a targeted "use `and`/`or`/`not` instead" message
    // rather than a confusing generic error. PipePipe also opens an empty-param
    // closure (`|| body`); Bang is also the prefix of `!=`, lexed separately as
    // BangEqual.)
    AmpAmp,   // &&  -> "use `and`"
    PipePipe, // ||  -> "use `or`" (in operator position only)
    Bang,     // !   -> "use `not`"

    // Bitwise
    Amp,            // &
    Pipe,           // |
    PipeArrow,      // |>
    Caret,          // ^
    Tilde,          // ~
    LessLess,       // <<
    GreaterGreater, // >>

    // Assignment
    Equal,               // =
    PlusEqual,           // +=
    MinusEqual,          // -=
    StarEqual,           // *=
    SlashEqual,          // /=
    PercentEqual,        // %=
    AmpEqual,            // &=
    PipeEqual,           // |=
    CaretEqual,          // ^=
    LessLessEqual,       // <<=
    GreaterGreaterEqual, // >>=

    // ── Literals ──────────────────────────────────────────────
    Identifier {
        name: String,
        /// `true` when the source wrote `r#NAME` (raw-identifier escape).
        /// The `name` field stores the bare identifier without the `r#` prefix.
        raw: bool,
    },
    Integer(i64, Option<IntSuffix>),
    Float(f64, Option<FloatSuffix>),
    CharLiteral(char),
    StringLiteral(String),
    MultiStringLiteral(String),
    InterpolatedStringLiteral(Vec<InterpolationPart>),
    /// `c"..."` — C-string literal. `bytes` excludes the trailing NUL (the
    /// codegen layer appends it); `source_len` records the textual length
    /// of the source-form body so `len()` / `as_bytes()` can answer
    /// without re-walking. Interior NUL bytes are rejected at lex time.
    /// See design.md § C-String Literals (v60 item 18).
    CStringLiteral {
        bytes: Vec<u8>,
        source_len: usize,
    },

    // ── Special ───────────────────────────────────────────────
    DocComment(String),
    /// `//!` module-level doc comment. Distinct from `///` so the parser
    /// can attach it to the enclosing module rather than the next item.
    ModuleDocComment(String),
    Error(String),
    EOF,
}
