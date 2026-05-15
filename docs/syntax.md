# Kāra Syntax Specification

Complete grammar reference for the Kāra programming language — a systems language with algebraic effects, ownership inference, and auto-parallelism. For semantics and rationale, see [design.md](design.md).

**Notation:** Grammar rules use a simplified EBNF:
- `|` = alternatives
- `[ ]` = optional
- `{ }` = zero or more repetitions
- `( )` = grouping
- `"keyword"` = literal keyword or symbol
- `UPPER_CASE` = grammar rule reference
- `// comment` = explanation

---

## Table of Contents

1. [Lexical Grammar](#1-lexical-grammar) — keywords, symbols, literals, comments
2. [Program Structure](#2-program-structure) — top-level item kinds
3. [Items](#3-items) — functions, structs, enums, traits, impls, effects, layouts, modules, types, providers
4. [Statements](#4-statements) — let, assignment, defer/errdefer
5. [Expressions](#5-expressions) — operators, control flow, closures, calls, pipes, error handling
6. [Types](#6-types) — primitives, generics, function types, references
7. [Effect Annotations](#7-effect-annotations) — effect lists, placement rules, equivalence
8. [Attributes](#8-attributes) — `#[...]` metadata on items
9. [Modules and Paths](#9-modules-and-paths) — module declarations, imports, visibility
10. [Entry Point](#10-entry-point) — `main()` signatures
11. [Semicolons and Separators](#11-semicolons-and-separators)
12. [Design Decisions](#12-design-decisions) — resolved syntax ambiguities

---

## 1. Lexical Grammar

### 1.1 Keywords

```
// Declarations
fn  struct  enum  union  trait  impl  mod  use  type  distinct  marker
            // `union` is the FFI-only untagged-union type; see design.md
            // § FFI Unions. Requires #[repr(C)]; field reads are unsafe.
            // `marker` is the trait-modifier prefix for marker traits
            // (`marker trait Foo;` — no methods, empty impls). See
            // design.md § Marker Traits.

// Visibility
pub  private

// Control flow
if  else  match  while  for  in  loop  return  break  continue

// Bindings
let  mut

// Logical
and  or  not   // word forms; `&&` / `||` / `!` are not accepted

// Ownership
ref  weak  shared  lock  own
            // `own` is the explicit capture-by-value prefix on closure
            // expressions: `own |x| body` forces every captured path to be
            // moved into the closure (see design.md § Rule 2½). The bare
            // closure form `|x| body` runs per-capture-path inference
            // instead. The receiver form `own self` was considered and
            // rejected; the parser emits a targeted diagnostic pointing
            // writers at bare `self` (the unified "bare = owned, borrows
            // are written" rule for parameter modes still holds).

// Closure capture (reserved against Rust idiom)
move          // Reserved but not active. Rust uses `move` for the
              // capture-by-value mode that Kāra spells `own` (see § Ownership
              // above). The parser emits a focused diagnostic redirecting
              // users to `own |...|` rather than accepting `move`.

// Cleanup
defer  errdefer

// Effects
effect  resource  verb  reads  writes  sends  receives  allocates  panics
blocks  suspends
with  transparent  stable  seq  par

// Type system
as  where   // where is used for refinement types and contracts
const       // used in pointer types (*const T) and const generic parameters

// Safety
unsafe

// Foreign / host bindings
extern  host

// Layout
layout  group

// Reserved for future use (not yet meaningful to the parser beyond being reserved)
dyn            // trait objects / dynamic dispatch
yield          // generator / coroutine syntax
asm            // inline assembly expression
global_asm     // module-level assembly block
gen            // generator function form (companion to `yield`)
become         // tail-call return form
do             // reserved for block-expression sugar / try-block-like form
final          // reserved against subclass-style "final" markers
override       // reserved against subclass-style "override" markers
priv           // reserved against `private` rename or alternative spelling
try            // try-block form (`try { ... }` returning a Result-shaped value)
typeof         // reserved against runtime/static type-of-expression queries
virtual        // reserved against subclass-style "virtual" markers
async          // reserved — Kāra uses `suspends` for the same role; keeping `async` reserved blocks accidental syntactic borrowing
await          // reserved — companion to `async`
comptime       // compile-time evaluation expression / block (Zig-style)
pure           // reserved against an explicit "this fn is effect-pure" marker; today purity is implicit (empty `with` clause)
box            // reserved against an owned-heap-pointer primitive (Kāra uses `Rc` / `Arc` / `shared`)

// Contracts
requires  ensures  invariant

// Literals
true  false

// Providers
providers

// Other
alias  independent  self  Self
```

All keywords are reserved — they cannot be used as identifiers without the `r#` raw-identifier escape (§1.3 IDENTIFIER, design.md § Raw Identifiers). `r#NAME` parses as the identifier `NAME` even when `NAME` is a reserved keyword (`r#async`, `r#try`, `r#move`, `r#comptime`, etc.). Structural markers — `self`, `Self`, `_`, `super`, `crate`, `mod`, `pub`, `priv`, `private`, `mut`, `ref`, `own` — cannot be raw-escaped (`error[E_RAW_IDENT_NOT_ALLOWED]`).

**Reserved identifiers** (not keywords, but cannot be used as user-defined identifiers):

```
Fn                // closure/function type constructor (see §6.3)
split_by_variant  // contextual keyword in layout blocks (§3.7)
```

### 1.2 Symbols

```
// Delimiters
(  )  {  }  [  ]

// Punctuation
:  ,  ;  .  ?.  ..  ..=  ->  =>  ?  ??  #  _  |>

// Arithmetic
+  -  *  /  %

// Comparison
==  !=  <  <=  >  >=

// Logical operators are spelled as keywords: `and`, `or`, `not` (see §1.1).
// The symbol forms `&&`, `||`, `!` are not accepted; the parser emits a
// migration error pointing at the keyword form. (Note: `!` still appears
// inside `!=`, which is the not-equal comparison operator above.)

// Bitwise
&  |  ^  ~  <<  >>

// Assignment
=  +=  -=  *=  /=  %=
&=  |=  ^=  <<=  >>=
```

### 1.3 Literals

```
INTEGER     = (DECIMAL | HEX | BINARY | OCTAL) [ INT_SUFFIX ]
DECIMAL     = DIGIT { DIGIT | "_" }
HEX         = "0" ("x" | "X") HEX_DIGIT { HEX_DIGIT | "_" }
BINARY      = "0" ("b" | "B") ("0" | "1") { "0" | "1" | "_" }
OCTAL       = "0" ("o" | "O") OCTAL_DIGIT { OCTAL_DIGIT | "_" }
INT_SUFFIX  = "i8" | "i16" | "i32" | "i64" | "i128"
            | "u8" | "u16" | "u32" | "u64" | "u128"

FLOAT       = FLOAT_MANTISSA [ FLOAT_EXP ] [ FLOAT_SUFFIX ]
            | DIGIT { DIGIT | "_" } FLOAT_EXP [ FLOAT_SUFFIX ]
FLOAT_MANTISSA = DIGIT { DIGIT | "_" } "." DIGIT { DIGIT | "_" }
FLOAT_EXP   = ("e" | "E") [ "+" | "-" ] DIGIT { DIGIT | "_" }
FLOAT_SUFFIX = "f32" | "f64"
              // Examples: 3.14, 1.5e-3, 6.022e23, 1e10, 2.5E+6_f32.
              // A FLOAT must have either a decimal point, an exponent, or both;
              // otherwise it lexes as INTEGER.

STRING      = [ STRING_PREFIX ] '"' { CHAR | ESCAPE_SEQ_FOR_PREFIX } '"'
MULTI_STR   = '"""' { any char } '"""'
              // Multi-line string — preserves newlines and indentation
INTERP_STR  = 'f"' { CHAR | "{" EXPR "}" } '"'
              // f"hello {name}" desugars to String.concat("hello ", name.to_string())

STRING_PREFIX = "b" | "r" | "br" | "rb" | "c"
              // Reserved prefix letters. A prefix is only recognized when it is
              // immediately followed by `'` or `"` with no intervening whitespace;
              // bare identifiers named `b`, `r`, `c`, `br`, `rb` therefore remain
              // legal wherever identifiers are legal.
              //
              // v1 support: only `b` is implemented. `r`, `br`, `rb`, and `c` are
              // grammatically reserved so future additions do not collide with
              // user identifiers; the lexer emits a "not yet implemented" diagnostic
              // for the reserved-but-not-yet-supported combinations.

BOOL        = "true" | "false"

CHAR_LIT    = [ CHAR_PREFIX ] "'" CHAR_CONTENT "'"
CHAR_PREFIX = "b"
              // Byte char literals (`b'A'`) in v1. Raw char literals and C-char
              // literals are not reserved — only string/byte-string forms are.
CHAR_CONTENT = any char except "'" and "\"
             | ESCAPE_SEQ_FOR_PREFIX
ESCAPE_SEQ  = "\" ("n" | "t" | "r" | "\\" | "'" | "\"" | "0")
             | "\u{" HEX_DIGIT { HEX_DIGIT } "}"
             | "\x" HEX_DIGIT HEX_DIGIT
              // `\u{...}` permitted in text strings/chars only.
              // `\x` hex byte escape permitted in both text and byte literals.

ESCAPE_SEQ_FOR_PREFIX =
              // Text literals (no prefix, or `r`/`c` — text-typed): full ESCAPE_SEQ.
              // Byte literals (`b` or `br`/`rb` — byte-typed): ESCAPE_SEQ minus
              // `\u{...}` — Unicode escapes are a lex error in byte context with a
              // diagnostic pointing at `\x`. A byte literal accepts any source byte
              // 0x00..0xFF verbatim; a text literal accepts only valid UTF-8.

IDENTIFIER  = RAW_IDENT_PREFIX? (ALPHA | "_") { ALPHA | DIGIT | "_" }
              // Must not be a keyword UNLESS preceded by RAW_IDENT_PREFIX.
              // Every identifier belongs to one of three case classes —
              // Type, Const, or Value — defined in §1.5. The case class is
              // determined by the identifier portion AFTER the optional
              // RAW_IDENT_PREFIX. Productions that require a specific class
              // write TYPE_IDENT, CONST_IDENT, or VALUE_IDENT; IDENT appears
              // only where the grammar is class-neutral.
              //
              // Reserved namespace: identifiers matching `expr_<NNNN>` where
              // NNNN is a four-digit year in the range 2020-2099 are reserved
              // for future macro / comptime fragment-specifier syntax. Such
              // identifiers reject at lex with E_RESERVED_FRAGMENT_SPECIFIER_NAMESPACE.
              // Use `r#expr_<NNNN>` if the exact name is needed today. See
              // design.md § Reserved Fragment-Specifier Identifier Namespace.

RAW_IDENT_PREFIX = "r#"
              // Raw-identifier escape: `r#async` parses as the identifier
              // `async` even though `async` is a reserved keyword. Forbidden
              // for structural markers (`self`, `Self`, `_`, `super`, `crate`,
              // `mod`, `pub`, `priv`, `private`, `mut`, `ref`, `own`) — see
              // design.md § Raw Identifiers for the full list and semantics.
              // The `r#` prefix is purely lexical: symbol tables, mangled
              // names, and downstream tools see the identifier without the
              // prefix. Discriminator vs. the reserved `r"..."` string-prefix
              // form: if the next char after `r#` is `"`, the lexer treats
              // it as the (currently reserved) string-prefix combination;
              // if it is an identifier-start character, raw-identifier.

ALPHA       = "a".."z" | "A".."Z"
UPPER_ALPHA = "A".."Z"
LOWER_ALPHA = "a".."z"
DIGIT       = "0".."9"
HEX_DIGIT   = DIGIT | "a".."f" | "A".."F"
OCTAL_DIGIT = "0".."7"
```

### 1.4 Comments

```
LINE_COMMENT    = "//" { any char except newline }
BLOCK_COMMENT   = "/*" { any char | BLOCK_COMMENT } "*/"
                  // Nesting supported.
DOC_COMMENT     = "///" { any char except newline }
```

### 1.5 Identifier Classes

Every `IDENTIFIER` belongs to exactly one case class, determined by the pattern of its ASCII alphabetic characters. See `design.md § Identifiers and Naming` for the normative rules and rationale.

```
TYPE_IDENT  = UPPER_ALPHA { ALPHA | DIGIT | "_" }
              // Multi-character: must contain at least one LOWER_ALPHA.
              // Single-character (exactly one UPPER_ALPHA): always Type-class
              // regardless of the lowercase requirement (CN-7 carve-out).
              // Used for structs, enums, enum variants, traits, type aliases,
              // distinct types, generic type parameters, and effect resources.

CONST_IDENT = UPPER_ALPHA { UPPER_ALPHA | DIGIT | "_" }
              // All alphabetic characters uppercase. Used for module-level
              // `let` and `let mut` bindings (function-level `let` uses
              // VALUE_IDENT — scope determines the class).

VALUE_IDENT = (LOWER_ALPHA | "_") { ALPHA | DIGIT | "_" }
              // Used for fn names, let-bindings, parameters, fields, modules,
              // effect groups, and effect verbs. A leading "_" signals an
              // intentionally unused binding and does not change the class.
```

**Post-lex classification algorithm.** The three grammar productions above are intentionally over-permissive — they cannot express all constraints in standard BNF (e.g. "contains at least one lowercase" cannot be stated without unwieldy alternation). The normative classification is applied post-lex to every identifier token, in this order:

1. If the first character is lowercase or `_` → **VALUE_IDENT**.
2. If the first character is uppercase AND the identifier is exactly one alphabetic character (CN-7 carve-out) → **TYPE_IDENT**.
3. If the first character is uppercase AND all remaining alphabetic characters are uppercase → **CONST_IDENT**.
4. If the first character is uppercase AND at least one subsequent alphabetic character is lowercase → **TYPE_IDENT**.

Every identifier falls into exactly one case. A case-class mismatch at a production site (e.g. `struct` followed by a CONST_IDENT or VALUE_IDENT) is a compile error with a diagnostic suggesting the correct casing. Steps 2–4 cover all starting-with-uppercase cases disjointly: single-uppercase-letter → Type, all-uppercase-multi-char → Const, mixed-case-uppercase-start → Type.

Productions elsewhere in this document currently write `IDENT` in every position for brevity. The case class implied by each position is normative regardless: `struct IDENT` means `struct TYPE_IDENT`, `fn IDENT` means `fn VALUE_IDENT`, `const IDENT` means `const CONST_IDENT`, and so on. Productions will be refined to cite the specific class in a follow-up grammar sweep.

**`self` and `Self` as path atoms.** Although `self` and `Self` are reserved keywords (§1.1), they are accepted as the *leading segment* of a path in the positions where they are semantically meaningful:

- `Self` may appear as the first segment of a `PATH_TYPE` (e.g., `Self.Item`, `Self` as a type by itself).
- `self` may appear as the first segment of a place expression (e.g., `self`, `self.field`, `self.field.nested`).

In these positions, the keyword behaves as a `TYPE_IDENT` or `VALUE_IDENT` respectively. Both remain reserved and cannot be redeclared as user identifiers.

---

## 2. Program Structure

A Kāra source file is a sequence of top-level items and (in script mode) statements. There are no separators between items — just whitespace. Items may appear in any order (forward references are allowed); statements execute in file order.

```
PROGRAM = { ITEM | TOPLEVEL_STMT }
          // A file containing any TOPLEVEL_STMT enters script mode — the compiler
          // synthesizes `fn main() -> Result[Unit, Error]` wrapping the statement
          // sequence in file order. A file with both TOPLEVEL_STMTs and an explicit
          // `fn main` is a parse-time error. See design.md § Entry Point > Script mode.

VISIBILITY = "pub" | "private"    // default (no keyword) = project-internal

ITEM = FUNCTION
     | STRUCT_DEF
     | ENUM_DEF
     | UNION_DEF
     | TRAIT_DEF
     | MARKER_TRAIT_DEF
     | TRAIT_ALIAS_DEF
     | IMPL_BLOCK
     | EFFECT_RESOURCE
     | EFFECT_GROUP
     | EFFECT_VERB_DECL
     | LAYOUT_DEF
     | MOD_DECL
     | USE_DECL
     | LET_DECL
     | ALIAS_DECL
     | INDEPENDENT_DECL
     | EXTERN_FUNCTION
     | HOST_FUNCTION
     | TYPE_ALIAS
     | DISTINCT_TYPE

TOPLEVEL_STMT = LET_STMT
              | EXPR_STMT
              | ASSIGN_STMT
              // Any statement legal in a function body (§ 4). Forwards to the
              // synthesized main's body.
```

---

## 3. Items

### 3.1 Functions

```
FUNCTION = [ ATTRIBUTES ] [ VISIBILITY ] "fn" IDENT [ GENERIC_PARAMS ]
           "(" [ FN_PARAMS ] ")" [ "->" TYPE ] [ EFFECT_LIST ]
           { CONTRACT }
           [ WHERE_CLAUSE ]
           BLOCK

FN_PARAMS = METHOD_PARAMS | PARAM_LIST
            // Inside an impl block or trait body, a function may lead with a SELF_PARAM
            // (see below). Free-standing functions use PARAM_LIST only — a SELF_PARAM
            // outside an impl/trait is a parse error.

WHERE_CLAUSE = "where" WHERE_BOUND { "," WHERE_BOUND }
WHERE_BOUND  = TYPE ":" TRAIT_BOUND { "+" TRAIT_BOUND }
             | TYPE "." IDENT "=" TYPE   // associated type equality constraint

PARAM_LIST = PARAM { "," PARAM } [ "," ]
PARAM      = IDENT ":" TYPE [ "=" EXPR ]
           | PATTERN ":" TYPE
             // First form: named parameter with optional default value.
             // Default must be a pure constant expression.
             // Constraint: defaulted params must be trailing — non-defaulted params
             // may not follow a defaulted one (enforced semantically, not in the grammar).
             //
             // **No anonymous parameter form.** A parameter is always either a
             // named IDENT or an explicit PATTERN (which includes the `_` wildcard).
             // Type-only parameters (`fn bar(i32);` style) are rejected at parse with
             // E_TRAIT_METHOD_ANONYMOUS_PARAM (in trait methods) or the equivalent
             // diagnostic for free functions; users write `_: i32` for an unused
             // parameter. See design.md § Trait method parameter names — required.
             // Omitting a defaulted argument requires a label for any argument that follows.
             // Second form: destructuring parameter (irrefutable patterns only).
             //
             // Parameter modes are always declared at the signature (design.md Feature 4
             // Part 1). The TYPE carries the mode: bare `T` = owned (default); `ref T`,
             // `mut ref T`, and `mut Slice[T]` are explicit borrow/exclusive-borrow forms.
             // Ownership inference is a body-level checking aid, not a signature-derivation
             // mechanism.

// self parameter in impl/trait methods:
// Bare "self" = owned (consuming) receiver — the default, written without a keyword.
// "ref self" / "mut ref self" = explicit borrow forms.
// The rule matches non-self parameters: default is bare, borrows are written.
SELF_PARAM = "self" | "ref" "self" | "mut" "ref" "self"
METHOD_PARAMS = SELF_PARAM [ "," PARAM_LIST ]

CONTRACT = "requires" EXPR
         | "ensures" "(" IDENT ")" EXPR
         // `old(EXPR)` is a special form valid only inside `ensures` clauses.
         // It captures the value of EXPR before the function body executes.
         // The expression must be Clone (value is snapshotted at entry).
         // Not valid in `requires` or `invariant`.
```

**Effect annotation placement:** Effects are introduced by the `with` keyword, which comes after the return type (or after the closing paren if no return type), before any contracts and the opening brace. Multiple effects are space-separated (no commas):

```
// No return type, no effects
fn greet() { ... }

// Return type, no effects (pure)
fn add(a: i64, b: i64) -> i64 { ... }

// No return type, with effects
fn log(msg: String) with writes(FileSystem) { ... }

// Return type and effects
fn save(user: User) -> Result[(), Error]
    with writes(UserDB) { ... }

// Multiple effects (space-separated)
fn process(id: i64) -> Result[Report, Error]
    with reads(UserDB) writes(OrderDB) sends(Network) { ... }

// Effect group
fn process(order: Order) -> Result[Receipt, Error]
    with OrderProcessing { ... }

// Mixed: group + individual effects (space-separated)
fn process(order: Order) -> Result[Receipt, Error]
    with OrderProcessing reads(FileSystem) { ... }

// Effect polymorphism
fn run[T: Processor](p: T, data: Data) -> Result[Output, Error] with _ { ... }

// With contracts
fn binary_search[T: Ord](haystack: ref Vec[T], needle: ref T) -> Option[i64]
    with reads(SearchIndex)
    requires haystack.is_sorted()
    ensures(result) match result { Some(i) => haystack[i] == *needle, None => true }
{ ... }

// Postcondition with old() for pre-state capture
fn withdraw(mut ref self, amount: i64) -> i64
    requires amount > 0
    ensures(result) self.balance == old(self.balance) - amount
{ ... }

// Destructuring parameters (irrefutable patterns only)
fn add((a, b): (i64, i64)) -> i64 { a + b }
fn distance(Point { x: x1, y: y1 }: Point, Point { x: x2, y: y2 }: Point) -> f64 { ... }

// Default parameter values
fn create_server(host: String, port: u16 = 8080, timeout_ms: i64 = 5000) -> Server { ... }
```

### 3.2 Structs

```
STRUCT_DEF = [ ATTRIBUTES ] [ VISIBILITY ] [ "shared" ] "struct" IDENT [ GENERIC_PARAMS ]
             [ WHERE_CLAUSE ]
             "{" [ STRUCT_FIELDS ] [ STRUCT_INVARIANT ] "}"

STRUCT_FIELDS    = STRUCT_FIELD { "," STRUCT_FIELD } [ "," ]
STRUCT_FIELD     = [ ATTRIBUTES ] [ "pub" ] [ "mut" ] IDENT ":" TYPE
STRUCT_INVARIANT = "invariant" EXPR
                 | "impl" "invariant" EXPR
```

Fields are **private by default** — accessible only within the defining module. `pub` makes a field visible from outside.

- `mut` on a field is only valid inside `shared struct` (marks a field as mutable after construction). Regular struct fields are always immutable after construction; mutability is controlled by `let mut` on the binding.
- For `shared struct`, `pub` and `mut` are orthogonal — all four combinations are valid: `pub mut field: T`, `pub field: T`, `mut field: T`, `field: T`.
- External code cannot write a struct literal that names a private field — the compiler rejects it and suggests using a constructor.
- **Unit struct:** when `STRUCT_FIELDS` is omitted entirely (empty braces), the struct has no fields. Construction also uses empty braces: `Marker {}`. The semicolon form (`struct Marker;`) is not valid.

**Field visibility:**

```
struct User {
    pub name: String,       // readable and constructible from outside
    pub email: String,
    password_hash: String,  // private — module-internal only
}
```

**Ownership modifiers on fields:**

```
struct Parser {
    source: ref String,       // borrowed reference
    position: i64,
}

struct Child {
    parent: weak Parent,      // weak reference (breaks cycles)
}
```

**Shared structs (reference semantics):**

```
shared struct TreeNode {
    val: i64,
    left: Option[TreeNode],      // shared reference, not owned
    right: Option[TreeNode],
}

shared struct GraphNode {
    id: i64,
    neighbors: Vec[weak GraphNode],  // weak breaks cycles in shared types too
}

// Assignment shares (RC increment), not moves
let a = TreeNode { val: 1, left: None, right: None };
let b = a;    // b and a point to same node
```

**Struct invariants:**

```
struct DateRange {
    start: i64,
    end: i64,

    invariant self.start <= self.end
}
```

### 3.3 Enums

```
ENUM_DEF = [ ATTRIBUTES ] [ VISIBILITY ] [ "shared" ] "enum" IDENT [ GENERIC_PARAMS ]
           [ WHERE_CLAUSE ]
           "{" VARIANT { "," VARIANT } [ "," ] "}"

VARIANT = [ ATTRIBUTES ] IDENT                              [ DISCRIMINANT ]   // unit variant
        | [ ATTRIBUTES ] IDENT "{" STRUCT_FIELDS "}"        [ DISCRIMINANT ]   // struct variant
        | [ ATTRIBUTES ] IDENT "(" TYPE_LIST ")"            [ DISCRIMINANT ]   // tuple variant
        // Variant attributes: #[default] (selects the variant returned by
        // #[derive(Default)] — unit variant only), #[deprecated], and any
        // #[diagnostic::*] / #[TOOL::*] tool-namespaced attributes. See
        // design.md § #[derive(Default)] and #[default] on enum variants.

DISCRIMINANT = "=" CONST_EXPR
        // CONST_EXPR is the constant-expression form already used by
        // module-level bindings (literals, arithmetic on literals, references to
        // other module-level integer constants). Out-of-range / duplicate values
        // are rejected at variant-decl time; partial declarations within one
        // enum (some variants explicit, others not) are rejected with
        // E_PARTIAL_EXPLICIT_DISCRIMINANTS — see design.md § Explicit
        // Discriminants on Payload Variants. Discriminants on payload-bearing
        // variants additionally require #[repr(intN)] or #[repr(C)] on the
        // enum.

TYPE_LIST = TYPE { "," TYPE } [ "," ]
```

All three variant forms are supported:

```
enum Token {
    EOF,                              // unit variant
    Integer(i64),                     // tuple variant
    Identifier(String),              // tuple variant
    Error { message: String, line: i64 }, // struct variant
}

enum Shape {
    Circle { radius: f64 },
    Rectangle { width: f64, height: f64 },
    Triangle { a: Vec2, b: Vec2, c: Vec2 },
}

// Standard library types:
enum Option[T] { Some(T), None }
enum Result[T, E] { Ok(T), Err(E) }

// #[derive(Display)] on enums emits the variant name as the string representation
#[derive(Display)]
enum Direction { Up, Down, Idle }
// Direction.Up.to_string() == "Up"
// Direction.Idle.to_string() == "Idle"

// snake_case option — emits variant name in snake_case
#[derive(Display(snake_case))]
enum Status { InProgress, Done, Failed }
// Status.InProgress.to_string() == "in_progress"
```

### 3.3a Unions (FFI)

```
UNION_DEF = ATTRIBUTES [ VISIBILITY ] "union" IDENT
            "{" UNION_FIELD { "," UNION_FIELD } [ "," ] "}"

UNION_FIELD = [ VISIBILITY ] IDENT ":" TYPE
```

`union` declares an untagged union type for FFI use. Every field shares the same storage; reading a field reinterprets bytes whose interpretation the compiler cannot verify and therefore requires `unsafe { }`. Writing a field is safe because every field type must be `Copy`. See `design.md § FFI Unions` for the full spec — required `#[repr(C)]`, field-`Copy` constraint, no `Drop`, no generics, no derives, no `#[non_exhaustive]`.

```
#[repr(C)]
union FloatBits {
    f:    f32,
    bits: u32,
}

let x = FloatBits { f: 3.14 };           // construction is safe — exactly one field
let raw: u32 = unsafe { x.bits };         // read is unsafe
x.bits = 0x40490FDB;                      // write is safe (Pod field)
```

### 3.4 Traits

```
TRAIT_DEF = [ ATTRIBUTES ] [ VISIBILITY ] "trait" IDENT [ GENERIC_PARAMS ]
            [ WHERE_CLAUSE ]
            "{" { TRAIT_ITEM } "}"

MARKER_TRAIT_DEF = [ ATTRIBUTES ] [ VISIBILITY ] "marker" "trait" IDENT [ GENERIC_PARAMS ]
                   [ ":" TRAIT_BOUND { "+" TRAIT_BOUND } ]
                   [ WHERE_CLAUSE ]
                   ( ";" | "{" "}" )
                   // e.g., marker trait Pod;
                   //       marker trait Concurrent: Sized;
                   //       marker trait Sealed[T];
                   // No methods, no associated types, no associated consts.
                   // Body is empty: either ";" or "{ }". Methods inside the
                   // body produce E_MARKER_TRAIT_HAS_METHOD; items inside
                   // produce E_MARKER_TRAIT_HAS_ITEM. Impls of marker traits
                   // must use empty `{ }` body; methods/items inside an impl
                   // produce E_MARKER_IMPL_HAS_METHOD. See design.md § Marker
                   // Traits.

TRAIT_ALIAS_DEF = [ ATTRIBUTES ] [ VISIBILITY ] "trait" IDENT [ GENERIC_PARAMS ]
                  "=" TRAIT_BOUND { "+" TRAIT_BOUND }
                  [ WHERE_CLAUSE ] ";"
                  // e.g., trait Numeric = Copy + Add + Sub + Mul + Div;
                  //       trait IteratorOver[T] = Iterator[Item = T];
                  // The alias is a name for a bound list, not a new trait.
                  // Cannot be implemented (impl AliasName for T is rejected).
                  // See design.md § Trait Aliases. Grammar lands at v1;
                  // expansion is P1.

TRAIT_ITEM = TRAIT_METHOD | ASSOC_TYPE_DECL

ASSOC_TYPE_DECL = "type" IDENT [ ":" TRAIT_BOUND { "+" TRAIT_BOUND } ] ";"
                  // Associated type declaration. Bound is optional.

TRAIT_METHOD = "fn" IDENT [ GENERIC_PARAMS ] "(" [ FN_PARAMS ] ")" [ "->" TYPE ] [ EFFECT_LIST ] ";"
             | "fn" IDENT [ GENERIC_PARAMS ] "(" [ FN_PARAMS ] ")" [ "->" TYPE ] [ EFFECT_LIST ] BLOCK
```

Trait methods ending with `;` are required — every impl must provide them. Trait methods with a body are defaults — impls can override or omit them. Associated types are declared with `type Name;` and bound in each impl with `type Name = ConcreteType`:

**Receiver is optional.** A trait method with a `SELF_PARAM` is dispatched on a value (`v.method(args)`). A trait method without a `SELF_PARAM` is an **associated function** — dispatched on the type (`T.method(args)` when `T: Trait`). This is the form needed for constructor traits like `Default`, `FromStr`, and factory-style methods:

```
trait Default {
    fn default() -> Self;
}

trait FromStr {
    type Err;
    fn from_str(s: StringSlice) -> Result[Self, Self.Err];
}
```

```
trait Iterator {
    type Item;

    // Required: every impl must provide this
    fn next(mut ref self) -> Option[Self.Item];

    // Default: provided, impls can override
    fn map[U](self, f: Fn(Self.Item) -> U with _) -> Vec[U] {
        let mut result = Vec.new();
        while let Some(item) = self.next() {
            result.push(f(item));
        }
        result
    }

    // Default: derived from next()
    fn count(self) -> i64 {
        let mut n = 0;
        while let Some(_) = self.next() {
            n = n + 1;
        }
        n
    }
}
```

Effect annotations on trait methods follow the same placement rules as regular functions:

```
trait Processor {
    // Effect-polymorphic: impls can have any effects
    fn process(self, data: Data) -> Result[Output, Error] with _;
}

trait Comparator[T] {
    // Pure required: all impls must be effect-free
    fn compare(self, a: T, b: T) -> Ordering;
}

trait DatabaseProvider {
    // Instance methods (provider implementations):
    fn query(self, sql: String) -> Result[Rows, Error];
    fn execute(self, sql: String) -> Result[(), Error];
}
```

### 3.5 Impl Blocks

```
IMPL_BLOCK = [ ATTRIBUTES ] "impl" [ GENERIC_PARAMS ] TYPE
             [ WHERE_CLAUSE ]
             "{" { IMPL_ITEM } "}"
           | [ ATTRIBUTES ] "impl" [ GENERIC_PARAMS ] PATH [ GENERIC_ARGS ]
             "for" TYPE [ WHERE_CLAUSE ] "{" { IMPL_ITEM } "}"

IMPL_ITEM = FUNCTION | ASSOC_TYPE_BINDING
ASSOC_TYPE_BINDING = "type" IDENT "=" TYPE ";"
```

```
// Plain impl
impl WordCount {
    fn total_ratio(self) -> f64 {
        self.unique as f64 / self.total as f64
    }
}

// Trait impl
impl Processor for LocalProcessor {
    fn process(self, data: Data) -> Result[Output, Error] {
        Ok(compute(data))
    }
}

// Trait impl with effects
impl Processor for RemoteProcessor {
    fn process(self, data: Data) -> Result[Output, Error]
        with sends(Network) {
        remote_call(data)
    }
}

// Generic inherent impl — same bracket syntax, no "for" clause
impl[S: Scheduler] Elevator[S] {
    pub fn new(floor: i64, scheduler: S) -> Elevator[S] {
        Elevator { floor, scheduler, stops: [] }
    }
}

// Multiple bounds
impl[T: Eq + Hash] Set[T] {
    pub fn contains(ref self, val: ref T) -> bool { ... }
}
```

### 3.6 Effect Declarations

```
EFFECT_RESOURCE = "effect" "resource" IDENT
                  [ RESOURCE_KEY ]
                  [ ":" TRAIT_BOUND ] ";"

RESOURCE_KEY    = "[" IDENT ":" TYPE "]"
                  // Named partition key for parameterized resources.
                  // Key name is required; anonymous keys are not supported.
                  // Multi-dimensional keys are not supported in v1.

TRAIT_BOUND     = IDENT { "+" IDENT }
                  // One or more provider trait names.
                  // Trait bound is optional; bare resources (no bound) are annotation-only
                  // and cannot be used with with_provider.

EFFECT_ATOM     = EFFECT_VERB "(" EFFECT_RESOURCE_REF { "," EFFECT_RESOURCE_REF } ")"
                | "blocks"
                | "suspends"
                | "panics"

EFFECT_RESOURCE_REF = PATH                     // plain resource: reads(UserDB)
                    | PATH "[" EXPR "]"         // parameterized: reads(UserDB[id])

EFFECT_GROUP = [ VISIBILITY ] [ "stable" ] "effect" "group" IDENT "="
               EFFECT_GROUP_BODY ";"
               // `stable` modifier prevents the group from being widened (compile error to add effects).

EFFECT_GROUP_BODY = EFFECT_TERM { "+" EFFECT_TERM }
EFFECT_TERM = EFFECT_VERB | IDENT   // verb(resources) or group name

EFFECT_VERB_DECL = [ VISIBILITY ] [ "transparent" ] "effect" "verb" IDENT ";"
```

```
// Basic resource (no provider, no key) — annotation-only
effect resource Latency;

// Resource with provider trait (single bound)
effect resource UserDB: DatabaseProvider;
effect resource OrderDB: DatabaseProvider;

// Resource with multiple provider trait bounds
effect resource AuditDB: DatabaseProvider + HealthCheckable;

// Parameterized resource — partition key adds conflict granularity
effect resource UserDB[user_id: i64];

// Parameterized resource used in effect atoms
fn update_profile(id: i64) with writes(UserDB[id]) { ... }
fn update_settings(id: i64) with writes(UserDB[id]) { ... }

// Effect groups
effect group Validation = reads(UserDB, InventoryDB) + sends(FraudService);
effect group Fulfillment = writes(OrderDB) + sends(PaymentGateway);
effect group OrderProcessing = Validation + Fulfillment;

// Non-transparent user-defined verb (propagates like built-in verbs)
effect verb logs;

// Transparent effect (doesn't propagate, never conflicts)
transparent effect verb traces;
```

### 3.7 Layout Declarations

```
LAYOUT_DEF = [ ATTRIBUTES ] [ VISIBILITY ] "layout" IDENT ":" TYPE "{" { LAYOUT_ITEM } "}"

LAYOUT_ITEM = "group" IDENT "{" IDENT { "," IDENT } [ "," ] "}"
            | "split_by_variant"
```

```
layout entities: Vec[Entity] {
    group physics { position, velocity }
    group combat { health, armor, is_alive }
    group metadata { id, name }
}

layout shapes: Vec[Shape] {
    split_by_variant
}
```

### 3.8 Import Declarations

Modules are defined by the directory structure — there is no `mod` declaration. The parser rejects `mod name;` with a diagnostic directing to the directory-tree rule.

```
IMPORT_DECL = [ VISIBILITY ] "import" IMPORT_PATH ";"

IMPORT_PATH = PATH_PREFIX IMPORT_TAIL

PATH_PREFIX = IDENT { "." IDENT }      // e.g., std.collections

IMPORT_TAIL = "." IDENT [ "as" IDENT ]                                    // single item, optional rename
            | "." "{" IMPORT_ITEM { "," IMPORT_ITEM } [ "," ] "}"            // brace-grouped multi-item (flat or nested)
            | "." "*"                                                          // wildcard — all pub items from the module
            | [ "as" IDENT ]                                                   // import the final path segment itself (module or item), optional rename

IMPORT_ITEM = IDENT [ "as" IDENT ]                 // leaf item
            | IDENT "." "{" IMPORT_ITEM { "," IMPORT_ITEM } [ "," ] "}"  // nested group — expands to flat imports before resolution
            | IDENT "." "*"                         // wildcard within a group (e.g. import a.{b.*, c})
```

Paths are absolute from the crate root — there is no `self.` / `super.` / relative form in v1. Nested grouping expands to flat imports before the resolver runs. Wildcard disambiguation rules are in § Module System.

```
// Simple import
import std.collections.Map;
import mylib.db.UserDB;

// Rename
import std.collections.Map as Dict;

// Multi-item (brace-grouped)
import std.collections.{Map, Set};
import log.{debug, info, warn, error};

// Multi-item with renames
import std.collections.{Map as Dict, Set};

// Nested grouping — expands to: import a.b.c; import a.b.d; import a.e
import a.{b.{c, d}, e};

// Wildcard — all pub items from std.collections
import std.collections.*;

// Module as value (binds the module itself)
import db.connection;

// Re-export
pub import db.connection.Connection;
pub import db.connection.{Connection, Pool};
pub import std.collections.*;    // re-export an entire module's public surface
```

### 3.9 Module-Level Bindings

```
LET_DECL = [ VISIBILITY ] "let" [ "mut" ] IDENT ":" TYPE "=" EXPR ";"
```

Module-level bindings require compile-time constant initializers (literals, const-arithmetic, struct/array literals built from constants). `let` is immutable; `let mut` permits runtime mutation (primarily for embedded/bare-metal use).

```
pub let MAX_CONNECTIONS: i64 = 100;
pub let PI: f64 = 3.14159265358979;
let ORIGIN: Point = Point { x: 0, y: 0 };
let mut SCRATCH: Array[u8, 256] = [0; 256];
```

### 3.10 Alias and Independent Declarations

`alias` declares that two effect resources are the same — operations on one conflict with the other. `independent` declares that two resources are disjoint — operations on one never conflict with the other, even if a conservative analysis might assume otherwise.

```
ALIAS_DECL       = [ VISIBILITY ] "alias" PATH "=" PATH ";"
INDEPENDENT_DECL = [ VISIBILITY ] "independent" PATH "," PATH ";"
```

Both are module-level items. `pub` exports the aliasing or independence fact to consumers of the module. Neither declaration is symmetric — `alias A = B` does not imply `alias B = A`; declare both if bidirectional aliasing is required. The right-hand side path may reference items from external packages.

See design.md § Resource Aliasing for full semantics including use with `--strict-effects` mode.

```
// This module wraps an external library's DB resource
alias mylib.UserDB = theirlib.TheirDB;   // module-private alias
pub alias mylib.Cache = theirlib.Cache;  // re-exported to consumers

// Declare two resources as non-aliasing for --strict-effects mode
independent mylib.UserDB, theirlib.TheirDB;
```

### 3.11 Extern Functions (FFI)

```
EXTERN_FUNCTION = [ ATTRIBUTES ] [ VISIBILITY ] "extern" STRING "fn" IDENT
                  "(" [ PARAM_LIST ] ")" [ "->" TYPE ] [ EFFECT_LIST ] ";"
```

No body — ends with `;`. The string specifies the ABI (currently only `"C"`):

```
extern "C" fn write(fd: i32, buf: *const u8, count: usize) -> isize
    with writes(FileSystem);

extern "C" fn read(fd: i32, buf: *mut u8, count: usize) -> isize
    with reads(FileSystem);
```

### 3.12 Type Aliases and Refinement Types

```
TYPE_ALIAS = [ VISIBILITY ] "type" IDENT [ GENERIC_PARAMS ] "=" TYPE
             [ "where" REFINEMENT_PRED ] ";"
        // GENERIC_PARAMS may carry trait bounds (e.g., `[T: Ord]`,
        // `[K: Eq + Hash, V: Clone]`); bounds are **enforced at every use
        // site** of the alias, not silently ignored. See design.md § Type
        // Aliases. A bound that's already implied by the alias's body
        // (`type SortedKeyMap[T: Ord] = TreeMap[T, i32]` — TreeMap already
        // requires K: Ord) is accepted but emits warning[redundant_alias_bound].
        // Use-site failures emit error[E_TYPE_ALIAS_BOUND_NOT_SATISFIED].

REFINEMENT_PRED = PURE_EXPR
PURE_EXPR       = PURE_EXPR ARITH_OP PURE_EXPR        // + - * / % & | ^ << >>
                | PURE_EXPR COMPARE_OP PURE_EXPR       // < <= > >= == !=
                | PURE_EXPR ("&&" | "||") PURE_EXPR
                | "!" PURE_EXPR
                | "(" PURE_EXPR ")"
                | "self"
                | "self" "." FIELD_NAME                // struct field: self.lo
                | "self" "." PURE_METHOD "(" ")"       // no-arg method with no effect annotations
                | CONST_EXPR                           // literal or module-level const
```

```
type UserId = i64;
type Callback = Fn(Event) -> Result[(), Error] with _;

// Refinement types — constraint checked at construction via try_from
type NonZero = i32 where self != 0;
type Percentage = f64 where self >= 0.0 && self <= 100.0;
type NonEmpty[T] = Vec[T] where self.len() > 0;
```

### 3.13 Distinct Types

```
DISTINCT_TYPE = [ ATTRIBUTES ] [ VISIBILITY ] "distinct" "type" IDENT "=" TYPE
                [ "where" REFINEMENT_PRED ] ";"
```

```
// Basic distinct type
distinct type UserId = i64;
distinct type PostId = i64;

// With refinement constraint
distinct type ValidPort = u16 where self >= 1 && self <= 65535;

// Opt-in arithmetic — allows +, -, *, / between values of the same distinct type;
// cross-type arithmetic (FloorNum + UserId) remains a compile error
#[derive(Eq, Ord, Arithmetic)]
distinct type FloorNum = i64;

#[derive(Eq, Ord, Arithmetic)]
distinct type PixelCoord = i32;
```

`#[derive(Arithmetic)]` is only valid on distinct types. It enables the standard arithmetic operators (`+`, `-`, `*`, `/`, `%`, `Neg`) between values of the **same** distinct type and returns a value of that type. See design.md § Distinct Types.

### 3.14 Provider Injection

`with_provider` is a stdlib function (not syntax), but it uses effect resources as type parameters:

```
with_provider[ResourceName](provider_instance, || {
    // code that uses the resource runs with this provider
});
```

The type parameter (`ResourceName`) is an effect resource identifier, not a regular type. The compiler resolves it to the provider trait bound declared on the resource (`effect resource ResourceName: ProviderTrait;`) and verifies the provider instance implements that trait. Closures that capture a provider-rooted resource cannot escape the block — the compiler rejects returning, storing, channel-sending, or spawning long-lived tasks that would outlive the provider. See design.md § Provider-Rooted Resources for semantics.

### 3.15 Providers Block

```
PROVIDERS_BLOCK = "providers" "{" PROVIDER_BINDING { "," PROVIDER_BINDING } [ "," ] "}"
                  "in" BLOCK

PROVIDER_BINDING = IDENT "=>" EXPR
```

Syntactic sugar for nested `with_provider` calls. All provider expressions evaluate top-to-bottom before any scope starts:

```
fn main() -> Result[(), AppError] {
    let config = load_config("app.toml")?;

    providers {
        OrderDB => PostgresOrderDB.connect(config.db_url)?,
        UserDB  => PostgresUserDB.connect(config.db_url)?,
        Cache   => RedisCache.connect(config.cache_url)?,
    } in {
        run_server()
    }
}
```

First-listed = outermost scope (last to clean up). `?` in provider expressions propagates to the enclosing function. See design.md § `providers { }` Block for full semantics.

### 3.16 Host Functions

```
HOST_FUNCTION = [ ATTRIBUTES ] [ VISIBILITY ] "host" "fn" IDENT
                "(" [ PARAM_LIST ] ")" [ "->" TYPE ] EFFECT_LIST ";"
```

No body — ends with `;`. Unlike `extern`, the `EFFECT_LIST` (a `with` clause) is **required** — `host fn` has no effect default. Parameter and return types are restricted to primitives, `Copy` types, and opaque-handle newtypes (see design.md § Host Functions).

```
host fn dom_append(parent: ElementHandle, child: ElementHandle)
    with writes(Display);

host fn fetch_json(url: ref String) -> Result[Bytes, HttpError]
    with sends(Network), receives(Network), suspends;
```

Target-neutral at the source level; the compiler lowers to `extern "C"` on native, WASM imports on browser-WASM, and WIT-backed Component Model (or C-ABI shim) on server-WASM.

---

## 4. Statements

```
STATEMENT = LET_STATEMENT
          | LET_ELSE_STATEMENT
          | ASSIGN_STATEMENT
          | DEFER_STATEMENT
          | ERRDEFER_STATEMENT
          | EXPR_STATEMENT
          // Note: `return`, `break`, and `continue` are expressions (§5.19, §5.7),
          // so `return expr;` parses as EXPR_STATEMENT. This is intentional —
          // same design as Rust.

LET_STATEMENT      = "let" [ "mut" ] PATTERN [ ":" TYPE ] "=" EXPR ";"
LET_UNINIT_STATEMENT = "let" [ "mut" ] IDENT ":" TYPE ";"
                     // Uninitialized declaration — no initializer. The IDENT is in scope
                     // but unusable until a first assignment (`IDENT = EXPR ;`) is reached
                     // on every control-flow path that reads it. Requires an explicit TYPE
                     // annotation (the compiler needs the type to reserve stack space).
                     // Only IDENT is allowed as the pattern (not a destructuring) — the
                     // entire binding is initialized atomically by the first assignment.
LET_ELSE_STATEMENT = "let" [ "mut" ] PATTERN "=" EXPR "else" BLOCK ";"
                     // The else block must diverge (return, break, continue,
                     // or call a diverging function like unreachable()).
                     // The binding from the pattern is in scope after the statement.
ASSIGN_STATEMENT   = PLACE_EXPR "=" EXPR ";"
                   | PLACE_EXPR COMPOUND_OP EXPR ";"
DEFER_STATEMENT    = "defer" ( EXPR ";" | BLOCK )
ERRDEFER_STATEMENT = "errdefer" ( EXPR ";" | BLOCK )
                   | "errdefer" "(" IDENT ")" BLOCK
                     // errdefer(e) { ... } — `e` binds the error value

COMPOUND_OP = "+=" | "-=" | "*=" | "/=" | "%="
            | "&=" | "|=" | "^=" | "<<=" | ">>="
            // Desugars to simple assignment: `x += 1` becomes `x = x + 1`
            // Requires `let mut` on the target variable.
```

`defer` evaluates its expression when the enclosing scope exits (normal or error). `errdefer` evaluates only on error paths (`?` propagation or explicit `return Err(...)`). Multiple defers in a scope execute in reverse declaration order (LIFO). `errdefer` with a binding — `errdefer(e) { ... }` — gives access to the error value inside the cleanup block.

Rules:
- On the error path, both fire: `errdefer` first, then `defer` (reverse declaration order within each group).
- No `?` inside a `defer`/`errdefer` block — compile error.
- `defer`/`errdefer` may not capture by move.
- Effects inside `defer`/`errdefer` contribute to the enclosing function's effect set.

```
let x = 5;
let mut count = 0;
let name: String = "Alice".to_string();
let (a, b) = get_pair();
count = count + 1;
process(data);

// defer/errdefer
let conn = Connection.open(addr)?;
errdefer conn.close();           // runs only if we return Err below
defer log("connection done");    // runs on all exits

// errdefer with error binding
errdefer(e) {
    log_error(e);
    conn.close();
}

// let...else — destructure with early exit on failure
let Ok(config) = load_config(path) else {
    return Err(ConfigError.Missing);
};
// config is in scope here
```

---

## 5. Expressions

### 5.1 Operator Precedence (lowest to highest)

Lower number = lower precedence (binds looser).

| Precedence | Operators | Associativity |
|---|---|---|
| 1 (loosest) | pipe `\|>` | Left |
| 2 | nil-coalesce `??` | Left |
| 3 | logical `or` | Left |
| 4 | logical `and` | Left |
| 5 | comparison `==` `!=` `<` `<=` `>` `>=` | Left (no chaining) |
| 6 | bitwise or `\|` | Left |
| 7 | bitwise xor `^` | Left |
| 8 | bitwise and `&` | Left |
| 9 | shift `<<` `>>` | Left |
| 10 | additive `+` `-` | Left |
| 11 | multiplicative `*` `/` `%` | Left |
| 12 | unary `not` `-` `~` `*` | Prefix |
| 13 | error propagation `?` | Postfix |
| 14 (tightest) | access `.` `?.` `()` `[]` | Left |

`not` binds tighter than the comparison operators, so `not x == y` parses as
`(not x) == y` — the same precedence relationship C-family languages use for
`!`. For the natural-English reading `not (x == y)`, parenthesize explicitly.
The `ambiguous_not_comparison` lint warns on `not` immediately adjacent to a
comparison without parentheses.

### 5.2 Primary Expressions

```
PRIMARY = INTEGER | FLOAT | STRING | MULTI_STR | INTERP_STR | CHAR_LIT | BOOL
        | IDENT
        | "self"
        | "Self"
        | "(" EXPR ")"                    // parenthesized
        | "(" EXPR "," { EXPR "," } ")"  // tuple
        | ARRAY_LITERAL                   // [expr, ...]
        | MAP_LITERAL                     // [key: val, ...]
        | BLOCK
        | LABELED_BLOCK
        | IF_EXPR
        | MATCH_EXPR
        | WHILE_EXPR
        | FOR_EXPR
        | LOOP_EXPR
        | SEQUENTIAL_EXPR
        | PAR_EXPR
        | LOCK_EXPR
        | UNSAFE_BLOCK
        | TRY_EXPR
        | STRUCT_LITERAL
        | CLOSURE
        | RETURN_EXPR
        | BREAK_EXPR
        | CONTINUE_EXPR
```

### 5.3 Block Expressions

```
BLOCK         = "{" { STATEMENT } [ EXPR ] "}"
                // Last expression without ";" is the block's return value.

LABELED_BLOCK = IDENT ":" BLOCK
                // Labeled block — same as BLOCK but `break IDENT [expr]` from
                // anywhere inside exits the block (with optional value).
                // `continue IDENT` is a compile error — only loops accept it.

TRY_EXPR      = "try" BLOCK
                // Try block — value is Result[T, E]. The block's tail expr
                // becomes Ok(T); `?` inside the block short-circuits to the
                // block's Err arm rather than to the enclosing function.
                // Grammar lands at v1; full typechecker pipeline (?-retarget,
                // error-type unification, From-chain, T/E inference) is P1.
                // See design.md § Try Blocks (`try { ... }`).
```

```
let result = {
    let a = compute_a();
    let b = compute_b();
    a + b    // no semicolon — this is the return value
};

// Labeled block — early exit with value
let result = found: {
    for row in matrix {
        for cell in row {
            if cell == target { break found cell; }
        }
    }
    -1                       // tail expression when nothing broke out
};
```

### 5.4 If/Else Expressions

```
IF_EXPR = "if" EXPR BLOCK [ "else" ( IF_EXPR | BLOCK ) ]
        | "if" "let" PATTERN "=" EXPR BLOCK [ "else" ( IF_EXPR | BLOCK ) ]
```

`if` is an expression — it returns a value. `if let` executes a block when a pattern matches, binding the destructured values inside the block:

```
let max = if a > b { a } else { b };

if condition {
    do_something();
} else if other_condition {
    do_other();
} else {
    do_default();
}

// if let — single-pattern match
if let Some(u) = user.find(id) {
    process(u);
} else {
    log("not found");
}

// if let with or-patterns
if let Left(x) | Right(x) = val {
    use(x);
}
```

The condition must be a `bool` expression. Implicit truthiness (non-zero integer, non-null, non-empty `Option`) is a compile error. Postfix ternary (`val if cond else other`) is not supported — use the prefix form above. `if let` chains (multiple `&&`-joined `let` bindings) are deferred — see design.md Future Decisions.

### 5.5 Match Expressions

```
MATCH_EXPR = "match" EXPR "{" MATCH_ARM { "," MATCH_ARM } [ "," ] "}"
           // Comma after an arm is required when the arm body is an expression.
           // Comma is optional (and conventionally omitted) when the arm body is a block.

MATCH_ARM = PATTERN [ "if" EXPR ] "=>" ( EXPR | BLOCK )
```

Match is exhaustive — all variants must be covered.

**Pattern guards** (`if EXPR` after the pattern) add a runtime condition. A guarded arm does not satisfy exhaustiveness — if the guard fails, matching continues to the next arm. An unguarded arm (or wildcard) is still required to cover the remaining cases:

```
match score {
    Ok(n) if n >= 90 => "A",
    Ok(n) if n >= 80 => "B",
    Ok(_) => "C or below",    // unguarded — covers remaining Ok values
    Err(e) => handle(e),
}
```

Guard expressions are ordinary expressions. Their effects are inferred and contribute to the enclosing function's effect set, the same as any other subexpression.

```
match shape {
    Circle { radius } => pi * radius * radius,
    Rectangle { width, height } => width * height,
    Triangle { a, b, c } => triangle_area(a, b, c),
}

match result {
    Ok(value) => process(value),
    Err(e) => {
        log_error(e);
        return Err(e);
    }
}
```

### 5.6 Patterns

```
PATTERN = "_"                                          // wildcard
        | IDENT                                        // binding
        | LITERAL                                      // literal
        | RANGE_PATTERN                                // range pattern (see below)
        | IDENT "@" PATTERN                            // binding with pattern test
        | IDENT "{" FIELD_PATTERN { "," FIELD_PATTERN } [ "," ] "}"  // struct destructure
        | IDENT "(" PATTERN { "," PATTERN } ")"        // tuple variant
        | "(" PATTERN { "," PATTERN } ")"              // tuple destructure
        | "[" [ PATTERN { "," PATTERN } [ "," ".." IDENT ] ] "]"  // array/slice pattern
        | PATTERN "|" PATTERN                          // or-pattern (alternation)

FIELD_PATTERN = IDENT                      // shorthand: field_name binds as field_name
              | IDENT ":" PATTERN          // named sub-pattern: field_name matched against PATTERN
              | ".."                       // rest wildcard: skip all remaining fields (must appear last)

RANGE_PATTERN = CONST_PAT_BOUND ".."  CONST_PAT_BOUND  // exclusive, [lo, hi)
              | CONST_PAT_BOUND "..=" CONST_PAT_BOUND  // inclusive, [lo, hi]
              | CONST_PAT_BOUND ".."                   // unbounded above, [lo, ∞)
              |                  ".."  CONST_PAT_BOUND // exclusive end, (∞, hi)
              |                  "..=" CONST_PAT_BOUND // inclusive end, (∞, hi]

CONST_PAT_BOUND = LITERAL                              // integer or char literal
                | QUALIFIED_PATH                       // resolves at typecheck to a
                                                       // module-level const of integer
                                                       // or char type. Function calls,
                                                       // arbitrary const expressions
                                                       // not accepted in pattern position.
                                                       // See design.md § Range Patterns
                                                       // > Const-expression bounds.
```

**`FIELD_PATTERN` notes.** The shorthand `IDENT` is equivalent to `IDENT: IDENT` — the field name serves as both the pattern selector and the created binding. Arbitrary nesting is valid: `{ a: Foo { b } }` matches an inner struct. `{ .. }` alone (skip everything) is valid. `{ field, .. }` (match some fields, skip rest) is valid; the `..` must be the last item in the field list. Trailing comma before `..` is optional. The `..` rest wildcard is distinct from range-expression `..` — context disambiguates: inside `{...}` pattern position, `..` is the rest wildcard, never a range.

**Array/slice pattern notes.** `[a, b, c]` is an exhaustive pattern on `Array[T, 3]` (irrefutable if each element pattern is irrefutable). `[first, ..rest]` is a slice pattern on `Vec[T]` — it covers all non-empty cases; `[]` or `_` is still required to cover the empty case. `..rest` must be the last element; only one rest segment per array/slice pattern. Array/slice patterns are valid in `let` bindings when irrefutable (fixed-size `Array[T, N]`); slice patterns on `Vec[T]` are refutable. `[]` on `Array[T, 0]` is irrefutable; `[]` on `Vec[T]` is refutable (matches empty only).

**`mut` and patterns.** `mut` is **not** valid inside a pattern — there is no `let (mut a, b) = ...` form. The `mut` modifier appears only before the entire pattern in `LET_STATEMENT` and `LET_UNINIT_STATEMENT`, and it applies uniformly to every binding the pattern introduces. For mixed mutability, destructure first and then shadow the bindings that need `mut`. `mut` is also not valid inside `match` arm patterns or `if let` / `while let` patterns — use a `let mut` rebind inside the arm body.

**Or-patterns.** A pattern may list multiple alternatives separated by `|`. The arm fires if any alternation matches:

- Every alternation must bind the **same set of names** with the **same types**.
- Or-patterns work in nested position: `Foo(A | B)` is equivalent to `Foo(A) | Foo(B)`.
- Or-patterns compose with guards: `P1 | P2 if guard => body`.
- Or-patterns work in `if let` and `while let`: `if let Left(x) | Right(x) = val { ... }`.

```
match token {
    EOF => ...,                          // unit variant
    Integer(n) => ...,                   // tuple variant binding
    Error { message, line } => ...,      // struct variant destructure
    _ => ...,                            // wildcard
}

// Or-patterns
match event {
    MouseDown { x, y } | TouchStart { x, y } => handle_click(x, y),
    _ => (),
}

// Nested or-patterns
match shape {
    Circle { .. } | Ellipse { .. } => "round",
    _ => "angular",
}

let (x, y, z) = my_tuple;
let Point { x, y } = my_point;
```

**Range patterns.** Match a contiguous range of values. Both bounded (`lo..hi`, `lo..=hi`) and half-open (`lo..`, `..hi`, `..=hi`) forms are accepted. Works with integer types and `char`. Not supported for `f32`/`f64` (no `Ord` — IEEE NaN) or `F32`/`F64` (total order is defined but range patterns are restricted to types with exhaustive, contiguous value spaces). Bounds may be integer/char literals OR qualified paths to module-level integer/char `const` bindings (`MIN_AGE..=MAX_AGE`); see design.md § Range Patterns > Const-expression bounds. Function calls and arbitrary const expressions are not accepted in pattern position. A bare `..` is **not** a pattern — use `_` for the wildcard.

```
match c {
    'a'..='z' => "lowercase",
    'A'..='Z' => "uppercase",
    '0'..='9' => "digit",
    _ => "other",
}

match code {
    200..=299 => "success",
    400..=499 => "client error",
    500..=599 => "server error",
    _ => "other",
}

// Half-open patterns
match n {
    ..=-1   => "negative",
    0       => "zero",
    1..=9   => "single digit",
    10..    => "large",
}
```

For exhaustiveness, a range pattern covers all values in its range. A match on an integer or char type is exhaustive only if some arm is a wildcard (`_` or binding) — the compiler does not attempt to verify that ranges cover the entire domain, even if the user has written a set of half-open patterns that together form a partition.

**Exception — bounded refinement types.** A refinement type whose constraint is exactly `self >= A && self <= B` (where `A` and `B` are integer compile-time constants and the base type is an integer primitive) defines a closed finite domain. The compiler treats it like an enum whose variants are the integers `A..=B`: a match that covers every value in `[A, B]` via literal and range patterns is accepted as exhaustive without a wildcard arm. When `B − A` exceeds 1024 the compiler falls back to requiring a wildcard. See design.md § Refinement Types — Pattern Exhaustiveness.

**`@` bindings.** Bind the matched value to a name while simultaneously testing a pattern. The binding captures the value at that position in the pattern. Ownership of the binding follows the existing match-arm-binding-mode rules: owned scrutinee → `@` binding consumes the value; `ref T` scrutinee → `@` binding is a borrow; explicit `ref IDENT @ PATTERN` borrows even under an owned scrutinee. `@` bindings nest, compose with or-patterns (every alternation must introduce the same name/type), and work in `let` patterns when the inner pattern is irrefutable. See design.md § @ Bindings for the full rule set including the cannot-double-consume rule and the nested-binding interaction.

```
// Capture the whole value while testing its shape
match val {
    x @ Some(_) => f"got: {x}",
    None => "nothing",
}

// Combine with range patterns
match age {
    n @ 0..=12 => f"child, age {n}",
    n @ 13..=19 => f"teenager, age {n}",
    _ => "adult",
}

// Inside nested patterns — capture a field while testing it
match response {
    Response { status: code @ 500..=599, body } => log_error(code, body),
    _ => ok(),
}
```

### 5.7 Loop Expressions

```
LABELED_LOOP  = IDENT ":" ( WHILE_EXPR | FOR_EXPR | LOOP_EXPR )
WHILE_EXPR    = "while" EXPR BLOCK
              | "while" "let" PATTERN "=" EXPR BLOCK
              // while let Some(x) = iter.next() { ... }
FOR_EXPR      = "for" PATTERN "in" EXPR BLOCK
LOOP_EXPR     = "loop" BLOCK
BREAK_EXPR    = "break" [ IDENT ] [ EXPR ]
              // break;             — exit innermost loop
              // break label;       — exit loop or labeled block named `label`
              // break expr;        — exit innermost loop with value
              // break label expr;  — exit loop or labeled block named `label`
              //                      with value
CONTINUE_EXPR = "continue" [ IDENT ]
              // continue;          — skip to next iteration of innermost loop
              // continue label;    — skip to next iteration of loop named `label`
              //                      (a labeled BLOCK is rejected here — only
              //                      loops accept `continue label`)
```

```
while count < 10 {
    count = count + 1;
}

for item in items {
    process(item);
}

while let Some(val) = iter.next() {
    process(val);
}

loop {
    if done() { break; }
}

// Labeled loops — break/continue target an outer loop by name
outer: for row in matrix {
    for val in row {
        if val == target {
            break outer;       // exits the outer for loop
        }
    }
}
```

### 5.8 Sequential Blocks

```
SEQUENTIAL_EXPR = "seq" BLOCK
```

`seq { }` suppresses auto-parallelism — all statements inside execute in source order. The block is an expression; its value is the value of the last statement. Use for ordering requirements the effect system cannot see (hardware register sequences, protocol steps):

```
seq {
    init_hardware();
    configure_mode();
    enable_output();
}
```

### 5.9 Parallel Blocks

```
PAR_EXPR = "par" BLOCK
```

`par { }` is an explicit fork-join scope — each top-level statement in the block becomes a concurrent branch. All branches join before execution continues past the block. The block is an expression; its value is the value of the last expression. Effect checking still applies: statements with conflicting effects on the same resource are serialized in source order within the block.

```
let (a, b) = par {
    let x = fetch_profile(id);
    let y = fetch_orders(id);
    (x, y)
};
```

See [Explicit Concurrency: `par {}` and `spawn()`](design.md#explicit-concurrency-par--and-spawn) in the design doc for full semantics, failure handling, and relationship to auto-concurrency and `spawn()`.

**Stdlib concurrency primitives.** The following are stdlib functions/types (not syntax), but complement `par {}` and `seq {}`:

| Primitive | Kind | Purpose |
|---|---|---|
| `spawn(f)` | `fn spawn[T, with E](f: Fn() -> T with E) -> TaskHandle[T]` | Structured task creation — child joins at scope exit. `TaskHandle` is scope-bound (cannot escape). |
| `TaskGroup` | Stdlib type | Scoped fan-out for dynamic task counts (accept loops, work queues). All tasks join on drop. |
| `Sender[T]` / `Receiver[T]` | Stdlib types | Typed channels for inter-task communication. |

```
// spawn — child task joins at scope exit
let handle = spawn(|| compute_result());
let result = handle.join();

// TaskGroup — dynamic fan-out
let group = TaskGroup.new();
for item in items {
    group.spawn(|| process(item));
}
// all tasks join when group goes out of scope

// Channels
let (tx, rx) = Channel.new[Message]();
spawn(|| { tx.send(Message.new()); });
let msg = rx.recv();
```

### 5.10 Lock Blocks

```
LOCK_EXPR = "lock" IDENT [ IDENT ] BLOCK
```

`lock` acquires exclusive access to a `Mutex[T]`-wrapped value for the duration of the block. The lock is released on block exit (including early return, break, or panic). The block is an expression — its value is the value of the last expression in the block. The second identifier, if provided, is a positional alias for the locked variable within the block:

```
let counter = Mutex(Counter { count: 0 });

// Single access — block returns the value
let val = lock counter { counter.count };

// Multi-step atomic operation
lock counter {
    counter.count += 1;
    print(counter.count);
}

// Positional alias — useful for long variable names
let connection_pool_manager = Mutex(PoolManager.new());
lock connection_pool_manager mgr {
    mgr.recycle_idle();
    mgr.stats()
}

// Alias in expression position
let count = lock counter c { c.count };
```

No `.lock()` method or guard values. Scope is always visible from the braces.

### 5.11 Closures

```
CLOSURE = [ CAPTURE_MODE ] "|" [ CLOSURE_PARAMS ] "|" ( EXPR | BLOCK )

CAPTURE_MODE   = "own" | "ref" | "mut" "ref"
                 // Bare `|...|` (no prefix) runs per-capture-path inference:
                 // the body's first classifying use of each captured path
                 // determines its mode (read → ref, mutate → mut ref,
                 // consume → own). The three explicit prefixes pin every
                 // captured path to the declared mode regardless of body
                 // usage (see design.md § Rule 2½). The `move` keyword is
                 // reserved against accidental Rust idiom; the parser
                 // redirects users to `own |...|`.
CLOSURE_PARAMS = CLOSURE_PARAM { "," CLOSURE_PARAM }
CLOSURE_PARAM  = PATTERN [ ":" TYPE ]
                 // Closure parameters accept the same irrefutable patterns
                 // as `fn` parameters and `let` bindings — tuple destructure,
                 // struct destructure, wildcard, etc.
```

```
|x| x + 1                       // bare — each capture's mode is inferred from the body
|x, y| x * y
|(a, b)| a + b                  // tuple destructure in parameter list
|Point { x, y }| x * y          // struct destructure in parameter list
|_| unit_value                  // wildcard parameter
|item: String| item.len()
|| { print("no params"); }

own |x| use(x)                  // every capture is by value (consume — moved into the closure)
ref |x| x.read()                // every capture is by reference (read-only borrow)
mut ref |x| x.mutate()          // every capture is by mutable reference (mut borrow)
```

Bare closures (no capture-mode prefix) run per-capture-path inference — the
body's first classifying use of each captured path determines its mode
(read → `ref`, mutate → `mut ref`, consume → `own`). The three optional
prefixes (`own` / `ref` / `mut ref`) pin every captured path to the declared
mode, useful when the closure escapes its creation scope and the captures'
fates need to be visible at the closure expression. The keyword is per-closure;
per-capture overrides are not in v1. Body usage that demands a stronger mode
than declared (e.g., `ref |x| x.consume()`) is a compile error at the closure
expression. See `design.md § Closure Behavior`, Rule 2½.

### 5.12 Function and Method Calls

```
CALL_EXPR   = EXPR [ "[" TYPE_LIST "]" ] "(" [ ARG_LIST ] ")"
              // Type args ([T]) for explicit generic specialization, omitted when inferable
ARG_LIST    = ARG { "," ARG } [ "," ]
ARG         = [ IDENT ":" ] [ "mut" ] EXPR
              // Optional label — must match the parameter name at that position.
              // Optional `mut` marker — required for fresh bindings passed to parameters
              // declared `mut ref T` or `mut Slice[T]`; rejected in all other argument
              // positions (see design.md Feature 4 Part 1½: Call-site Mutation Markers).
              // `ref` and `mut ref` are **not** legal at call sites — the keyword is
              // rejected with a diagnostic pointing at the signature-declared mode.

METHOD_CALL = EXPR "." IDENT [ "[" TYPE_LIST "]" ] "(" [ ARG_LIST ] ")"
              // Type args for explicit specialization; no turbofish syntax
              // Method-call receivers never carry the `mut` marker (the `.` dispatch
              // signals mutation through the receiver; marker is suppressed by convention).
```

Arguments may be labeled with the parameter name at the call site. Labels are opt-in — no annotation on the declaration is needed. Order must match declaration order; labels do not allow reordering.

```
save_user(user)
user.validate()
User.validate(user)           // UFCS — equivalent to user.validate()
items.map(|x| x + 1)
Map.new()
sort[i64](list)                // explicit type arg when inference can't resolve

// Named arguments — order follows declaration
create_user(name: "alice", email: "alice@example.com", is_admin: false)

// Partial labels — first two positional, rest labeled
create_user("alice", "alice@example.com", is_admin: false, max_sessions: 3)

// Default parameters — skipping requires label
fn create_server(host: String, port: u16 = 8080, max_connections: i64 = 1000)
create_server("0.0.0.0")                          // uses all defaults
create_server("0.0.0.0", max_connections: 100)    // skip port, label required
```

### 5.13 Field Access, Indexing, and Dereference

Dot access reads struct fields or tuple positions. Bracket access indexes into arrays, vectors, and maps. The prefix `*` operator dereferences a `ref T` or `mut ref T` to `T`.

```
FIELD_ACCESS = EXPR "." IDENT
INDEX_ACCESS = EXPR "[" EXPR { "," EXPR } "]"
                         // Single EXPR: standard index — lowers to Index.index(ref e, idx)
                         // Multiple EXPRs: tuple index — lowers to Index.index(ref e, (i, j, ...))
TUPLE_ACCESS = EXPR "." INTEGER       // my_tuple.0, my_tuple.1
DEREF_EXPR   = "*" EXPR                // unary prefix; precedence 12 (unary)
```

```
user.name              // struct field
my_tuple.0             // tuple element by position
items[i]               // array/vector index
scores["alice"]        // map lookup
t[i, j, k]            // tensor multi-index; desugars to t[(i, j, k)]
*needle                // dereference `needle: ref T` to T
*counter = 0           // assign through a `mut ref i64`
```

**Multi-index desugaring.** `e[i, j, k]` (two or more comma-separated expressions inside `[]`) is syntactic sugar for `e[(i, j, k)]` — the indices are folded into a tuple and the single-arg `Index` / `IndexMut` trait is called with that tuple. `Tensor[T, [M, K, N]]` implements `Index[(i64, i64, i64)]`; `t[i, j, k]` and `t[(i, j, k)]` are exactly equivalent. The `:` character inside `[]` on a value-class expression (as in `map["a": 1]`) is a parse error — `key: value` syntax is only valid inside `PREFIX_LITERAL` productions (see §5.17b).

Field access and method calls auto-deref through `ref T` / `mut ref T`, so `r.field` does not need an explicit `*`. Use `*expr` for value-level operations that aren't method calls — comparisons (`a == *b`), arithmetic, and assignment-through-ref (`*r = v`). Kāra does **not** auto-deref in binary-operator position; `a == b` where `b: ref T` is a type error — write `a == *b`.

**Place expressions.** Assignment and compound-assignment targets (see § 4) are restricted to *place expressions*:

```
PLACE_EXPR = VALUE_IDENT                              // let-bound name (must be `let mut`)
           | PLACE_EXPR "." IDENT                    // field / tuple-position access
           | PLACE_EXPR "[" EXPR { "," EXPR } "]"   // index access (multi-index sugar applies)
           | "*" EXPR                                // dereference (expr: mut ref T)
           | "(" PLACE_EXPR ")"
```

A `*expr` place requires `expr: mut ref T`; `ref T` (shared borrow) is read-only. Method-call results, function-call results, and literals are not place expressions — they may appear on the right of `=` but not on the left.

### 5.14 Range Expressions

```
RANGE = EXPR ".." EXPR        // Range              — exclusive end,      [lo, hi)
      | EXPR "..=" EXPR       // RangeInclusive     — inclusive end,      [lo, hi]
      | EXPR ".."             // RangeFrom          — unbounded above,    [lo, ∞)
      | ".." EXPR             // RangeTo            — exclusive end,      (∞, hi)
      | "..=" EXPR            // RangeToInclusive   — inclusive end,      (∞, hi]
      | ".."                  // RangeFull          — fully unbounded
```

```
for i in 0..10 { ... }        // 0, 1, 2, ..., 9
for i in 0..=10 { ... }       // 0, 1, 2, ..., 10
let slice = items[2..5];      // Range
let tail  = items[2..];       // RangeFrom
let head  = items[..n];       // RangeTo
let all   = items[..];        // RangeFull
```

Only `Range` / `RangeInclusive` are directly iterable (they have both endpoints). The other forms are used primarily for slicing and pattern matching. See `design.md` for the six library types and their trait implementations.

### 5.15 Struct Literals

```
STRUCT_LITERAL = IDENT "{" FIELD_INIT { "," FIELD_INIT } [ "," ] [ ".." EXPR ] "}"

FIELD_INIT = IDENT ":" EXPR       // explicit: Point { x: 1.0 }
           | IDENT                // shorthand: Point { x } when variable x matches field x
```

```
WordCount { total: 42, unique: 30 }
Point { x: 1.0, y: 2.0 }

// Shorthand field init — variable name matches field name
let x = 1.0;
let y = 2.0;
Point { x, y }                    // equivalent to Point { x: x, y: y }

// Struct update — copy remaining fields from an existing value
let updated = User { name: "Bob", ..existing_user };
```

Struct update (`..expr`) must appear last. All fields not explicitly listed are taken from `expr`, which must be the same struct type. The source value follows normal ownership rules — fields that are moved out of the source are consumed; `Copy` fields are copied.

### 5.16 Array Literals

```
ARRAY_LITERAL = "[" [ EXPR { "," EXPR } [ "," ] ] "]"
```

```
let primes = [2, 3, 5, 7, 11];            // Array[i64, 5] — type and size inferred
let empty: Array[i64, 0] = [];            // empty array, explicit type
let matrix = [[1, 0], [0, 1]];           // nested arrays
```

Array literals produce `Array[T, N]` values (see §6.2). The element type `T` and size `N` are inferred from the literal contents. To spell the type explicitly, use `Array[T, N]` in a type annotation.

### 5.17 Map Literals

```
MAP_LITERAL = "[" MAP_ENTRY { "," MAP_ENTRY } [ "," ] "]"
MAP_ENTRY   = EXPR ":" EXPR
```

```
let scores = ["alice": 10, "bob": 7];          // Map[String, i64] — inferred
let lookup = [1: "one", 2: "two", 3: "three"]; // Map[i64, String]
```

The parser disambiguates from array literals by the `:` that follows the first key expression. All keys must have the same type and all values the same type; the key type must satisfy `Hash + Eq`. There is no empty map literal — use `Map.new()` instead (the type is uninferable from zero entries).

### 5.17b Prefix Collection Literals

```
PREFIX_LITERAL   = COLLECTION_IDENT "[" PREFIX_CONTENT "]"
COLLECTION_IDENT = "Array" | "Vec" | "Set" | "Map" | "VecDeque" | "TreeMap"
PREFIX_CONTENT   = SEQ_CONTENT    // Array, Vec, Set, VecDeque, TreeMap
                 | MAP_CONTENT    // Map only
                 | REPEAT_CONTENT // Array, Vec only
                 | ε              // empty — requires a binding annotation
SEQ_CONTENT    = EXPR { "," EXPR } [ "," ]
MAP_CONTENT    = MAP_ENTRY { "," MAP_ENTRY } [ "," ]
REPEAT_CONTENT = EXPR ";" EXPR
```

**Disambiguation.** A `COLLECTION_IDENT` is a Type-class identifier (uppercase-first). Because Type-class identifiers are never local value bindings, `Array[...]` in expression context is always a `PREFIX_LITERAL` — never an `INDEX_ACCESS`. The case-class rule makes this decision at lex time with no scope lookup.

Rules:
- **Single element** — `Array[42]` is a one-element prefix literal. `Array[i32]` where `i32` is a type name is a type error (a type is not a valid element expression).
- **Map content** — `Map[...]` requires `key: value` pairs (`MAP_CONTENT`). `Map[a, b]` without `:` is a compile error. `Vec[...]` and `Set[...]` require plain expressions; `:` inside them is a parse error.
- **Empty** — `Array[]`, `Vec[]`, `Map[]` etc. have no element type to infer and require a binding annotation. `let v: Vec[i64] = Vec[]` is valid; bare `Vec[]` is a compile error.

```
let xs = Array[1, 2, 3];            // Array[i64, 3]
let ys = Vec[1, 2, 3];              // Vec[i64]
let s  = Set[1, 2, 3];              // Set[i64]
let m  = Map["a": 1, "b": 2];       // Map[String, i64]
let buf = Array[0; 256];            // Array[i64, 256] — repeat form
let v: Vec[i64] = Vec[];            // empty — annotation required
```

### 5.18 Pipe Expressions

```
PIPE_EXPR = EXPR "|>" EXPR
```

`|>` chains function calls left-to-right, passing the left-hand value as the first argument to the right-hand function. Use `_` as a placeholder when the piped value is not the first argument:

```
// Without pipe — inside-out reading order
let result = save(validate(parse(normalize(raw_input))));

// With pipe — left-to-right reading order
let result = raw_input |> normalize |> parse |> validate |> save;

// Placeholder for non-first argument position
let result = data
    |> filter(_, is_valid)      // filter(data, is_valid)
    |> map(_, transform)        // map(filtered, transform)
    |> Vec.sort;               // first arg — no placeholder needed

// ? applies to the output of each pipe stage
raw |> parse? |> validate? |> save
```

Rules:
- Left-associative: `a |> f |> g` parses as `(a |> f) |> g`.
- `_` is the pipe-hole placeholder — at most one per stage, only valid inside a function argument list in a pipe stage.
- **Postfix `?` in pipe chains:** `?` after a pipe stage applies to the entire stage result, not the preceding primary expression. `raw |> parse?` means `(raw |> parse)?`. This is a pipe-specific rule — in the general precedence table (§5.1), `?` binds tighter than `|>`, but within a pipe chain each `?` attaches to its stage.
- No conflict with `|` or `||` — the lexer produces three distinct tokens.

### 5.19 Cast Expressions

`as` performs type conversions. The semantics depend on the target type:

- **`T` is a primitive numeric type** (`i8`..`i128`, `u8`..`u128`, `f32`, `f64`): numeric cast — every (source, target) pair is fully defined; never UB, never implementation-defined, never panics on the numeric path. Float→int saturates (NaN → 0, out-of-range → MIN/MAX); int→int narrows by bitwise truncation; int→float rounds via IEEE 754 round-to-nearest-even. See `design.md § Numeric Semantics > as-cast semantics — every pair fully defined`. No `panics` effect on any numeric `as` cast.
- **`T` involves `char` or `bool`**: `char as u32` / `char as i32` legal; `char as uN/iN` for N < 32 rejected (`E_CHAR_AS_NARROW_INT`) — write `c as u32 as uN` for explicit two-step truncation, or `c.encode_utf8(buf)` for proper UTF-8 encoding; `iN/uN as char` rejected (`E_INT_AS_CHAR`) — use `char.try_from(u32)` for the fallible inverse; `bool as iN/uN` legal (`false → 0`, `true → 1`); `iN/uN as bool` rejected — write `n != 0` explicitly; `f32/f64 as bool` rejected.
- **`T` is a refinement type** (e.g., `NonZero = i32 where self != 0`): refinement assertion — runtime predicate check, panics on failure, propagates `panics`. The source expression must have exactly the same type as `T`'s base type; if the source type differs, it is a compile error (write the two steps explicitly: `(x as i32) as NonZero`).
- **`T` is a pointer type** (`*const T`, `*mut T`): pointer cast between *raw* pointer types — `unsafe` blocks only. Casting a *reference* to a raw pointer (`&value as *const T` or `&mut value as *mut T`) is forbidden and rejected at typecheck — use `ptr.const(value)` / `ptr.mut(value)` to construct a raw pointer directly without ever creating a reference. See `design.md § Raw Pointer Construction`.
- **Pointer ↔ integer casts are forbidden.** `*const T as usize`, `*mut T as usize`, `usize as *const T`, and `usize as *mut T` are all rejected at typecheck under the strict-provenance model. Use `ptr.addr(p)` to read the address bits, `ptr.with_addr(p, addr)` to reseat an existing pointer, `ptr.expose(p)` to publish provenance for a later round-trip, and `ptr.from_exposed[T](addr)` (unsafe) to recover a pointer from a previously-exposed address. See `design.md § Pointer Provenance`.

```
CAST_EXPR = EXPR "as" TYPE
```

```
let ratio = count as f64;             // numeric — count: i64, result: f64
let byte = large_number as u8;        // numeric — truncates; no check
let nz = x as NonZero;               // refinement — x: i32; runtime check; panics if x == 0
let bad = y as NonZero;              // ERROR if y: i64 — base type mismatch; write (y as i32) as NonZero
let p1 = ptr.const(value);            // raw pointer construction (no unsafe needed)
let p2 = p1 as *mut T;                // raw → raw pointer cast — unsafe block required
```

Implicit numeric coercions are confined to literals. All conversions between typed variables must be explicit.

### 5.20 Return Expressions

```
RETURN_EXPR = "return" [ EXPR ]
```

```
return Err(e);
return;
```

### 5.21 The `?` Operator

```
ERROR_PROP = EXPR "?"
```

Works for both `Result` and `Option`, determined by the enclosing function's return type. Cross-error-type propagation (e.g., `fn foo() -> Result[T, MyError]` calling `bar() -> Result[U, OtherError]` and writing `bar()?`) requires an `impl From[OtherError] for MyError` in scope — the compiler inserts the conversion. See `design.md § Error Handling`.

```
// In a function returning Result[T, E]:
match expr {
    Ok(val) => val,
    Err(e) => return Err(e),
}

// In a function returning Option[T]:
match expr {
    Some(val) => val,
    None => return None,
}
```

```
// Result example:
fn load_config() -> Result[Config, Error] {
    let data = read_file("config.json")?;    // returns Err early if fails
    parse_json(data)?
}

// Option example:
fn get_city(user: User) -> Option[String] {
    let addr = user.address?;                // returns None early if None
    let city = addr.city?;
    Some(city.name)
}
```

### 5.22 Optional Chaining (`?.` and `??`)

```
OPTIONAL_CHAIN = EXPR "?." IDENT
               | EXPR "?." IDENT "(" [ ARG_LIST ] ")"

NIL_COALESCE = EXPR "??" EXPR
```

`?.` short-circuits to `None` if the expression is `None`. `??` provides a default value:

```
// Optional chaining — short-circuits on None
let name = user.address?.city?.name;           // Option[String]

// With default value
let name = user.address?.city?.name ?? "unknown";  // String

// Method calls
let len = user.name?.len();                    // Option[i64]
```

`??` has the second-lowest operator precedence (above `|>`, below `||`). See §5.1 for the full table.

### 5.23 Unsafe Blocks

```
UNSAFE_BLOCK = "unsafe" BLOCK
```

```
let p: *const T = ptr.const(value);   // construction is safe
unsafe {
    let raw = p.offset(4);             // arithmetic — unsafe
    let val = *raw;                    // dereference — unsafe
}
```

### 5.24 Built-in Functions

These are compiler builtins — they look like function calls but have special semantics:

| Function | Type | Purpose |
|---|---|---|
| `todo()` | `-> Never` | Marks unfinished code; panics at runtime with `"not yet implemented"`. Accepts 0 or 1 `String` arg. Carries `panics` effect. |
| `unreachable()` | `-> Never` | Asserts code path cannot be reached; panics at runtime. Accepts 0 or 1 `String` arg. Carries `panics` effect. |
| `dbg(expr)` | `-> T` (returns the value) | Prints file, line, expression text, and value to stderr; returns the value. Uses transparent `debugs` effect (no signature change). Stripped in release builds. |
| `assert(expr)` | `-> ()` | Panics if `expr` is false. Captures expression text, file, and line in the panic message. Accepts optional second `String` arg for custom message. Carries `panics` effect. Stripped in release builds. |
| `assert_eq(a, b)` | `-> ()` | Panics if `a != b`. Captures expression text for both args, file, and line. Shows both values in the panic message. Carries `panics` effect. Stripped in release builds. |
| `collect_all(exprs...)` | `-> (Result[A, E1], ...)` | Runs all branches to completion; returns tuple of Results. Arguments are auto-thunked (closure wrappers optional). Compiler builtin with fixed-arity overloads (max 8). |
| `process.exit(code: i32)` | `-> Never` | Runs all pending `defer`/`errdefer` blocks, then exits with the given code. Carries `panics` effect. Uncatchable — propagates through any future `catch_panic` boundary. |

`todo()` and `unreachable()` return `Never` (the bottom type), which coerces to any type:

```
fn handle(x: Option[i32]) -> i32 {
    match x {
        Some(n) => n,
        None => todo("zero case not handled yet"),
    }
}
```

`collect_all` runs all branches in parallel. Arguments are auto-thunked — the compiler implicitly wraps each argument expression in a closure, so explicit `||` wrappers are optional:

```
let (profile, orders, notifs) = collect_all(
    fetch_profile(user_id),
    fetch_orders(user_id),
    fetch_notifs(user_id),
);
```

---

## 6. Types

### 6.1 Primitive Types

```
// Signed integers
i8  i16  i32  i64  i128

// Unsigned integers
u8  u16  u32  u64  u128  usize   // usize is FFI-only (maps to C's size_t); use i64 for indices/sizes

// Floating point — IEEE 754, PartialEq/PartialOrd only (no Eq/Ord/Hash)
f32  f64

// Floating point — total order, NaN sorts last, Eq/Ord/Hash — use for Map/Set keys, sorting
F32  F64

// Other
bool  char  Never  String  StringSlice
```

`Never` is the bottom type — the return type of diverging functions (`todo()`, `unreachable()`, `panic()`). It coerces to any type, making it valid in any expression position.

`String` is the owned, heap-allocated text type. `StringSlice` is a borrowed string view (pointer + offset + length) that follows borrow rules — the zero-copy counterpart to `String`. String literals have type `String`; `ref String` and `StringSlice` are both valid for borrowing.

### 6.2 Type Syntax

```
TYPE = PATH_TYPE
     | TUPLE_TYPE
     | POINTER_TYPE
     | FUNCTION_TYPE
     | DYN_TYPE
     | "Never"                           // bottom type (diverging functions)
     | "ref" TYPE
     | "ref" "(" IDENT { "," IDENT } ")" TYPE
     | "mut" "ref" TYPE
     | "weak" TYPE
     | "()"                              // unit type

PATH_TYPE    = IDENT { "." IDENT } [ "[" GENERIC_ARG_LIST "]" ]
               // Array[T, N] is a PATH_TYPE — generic args accept types, const exprs, or shapes
TUPLE_TYPE   = "(" TYPE "," TYPE { "," TYPE } ")"
POINTER_TYPE = "*" "const" TYPE | "*" "mut" TYPE
DYN_TYPE     = "dyn" IDENT [ "[" TYPE_LIST "]" ]
               // Future: trait objects for dynamic dispatch.
               // The compiler uses the worst-case union of all known
               // impl effects for effect analysis (see design.md).

// Shape literals appear in type-argument position when the target kind is Shape
// (e.g., Tensor[T, Shape]'s second argument). Not legal as a standalone type.
SHAPE_LIT    = "[" SHAPE_ELEM { "," SHAPE_ELEM } "]"
SHAPE_ELEM   = EXPR         // const expression — static dim (typically an integer literal or Dim-kinded generic param)
             | "?"          // dynamic dim marker — dim determined at runtime
             | "..." IDENT  // variadic shape splice — binds the remainder of the shape (only in function-signature position)
```

`Array[T, N]` (fixed-size array where `N` is a compile-time integer constant) parses as a `PATH_TYPE`. Array literals `[expr, ...]` (§5.15) produce `Array[T, N]` values with inferred `T` and `N`.

`Tensor[T, Shape]` is a `PATH_TYPE` whose second generic argument is a `SHAPE_LIT`. Example: `Tensor[f64, [3, 4, ?]]`. The `?` marker is legal only inside a shape literal; outside shape position, `?` remains the expression-level question-mark operator (§ 5.21). Shape literals do not nest — a dim is a const expression, a `?`, or a `...S` variadic splice, never another shape literal.

```
// Simple types
i64
String
bool

// Generic types
Vec[T]
Map[K, V]
Result[Output, Error]
Option[T]
Array[f64, 3]        // fixed-size array, 3 elements
Array[u8, 256]

// Tuple types
(i64, String, bool)

// Pointer types (unsafe context only)
*const T
*mut u8

// Reference types
ref String              // immutable borrow
// multi-ref returns: compiler conservatively assumes return borrows from all ref params
mut ref String          // mutable borrow
weak Parent             // weak reference (for cycle breaking)

// Unit type
()
```

### 6.3 Function Types

Note: `Fn` (uppercase) is a built-in type identifier for closure/function types, not a keyword. It is recognized by the type parser but cannot be used as a user-defined identifier.

```
FUNCTION_TYPE = "Fn" "(" [ TYPE_LIST ] ")" [ "->" TYPE ] [ "with" EFFECT_SPEC ]
                // EFFECT_SPEC is shared with effect annotations on declarations (§7.1).
                // In Fn types, `with ()` is the way to express "explicitly pure" as a
                // type — in function declarations you'd simply omit the `with` clause.
```

Note: `Fn` (uppercase) is the function/closure type. `fn` (lowercase) declares functions.

```
Fn(T) -> U                              // pure closure type
Fn(T) -> U with _                       // any-effects closure type
Fn(T) -> U with writes(OrderDB)         // specific-effect closure type
Fn(Event) with writes(OrderDB)          // no return type, with effects
Fn(T, T) -> Ordering                    // comparator (pure)
```

### 6.4 Generic Parameters and Arguments

```
GENERIC_PARAMS = "[" GENERIC_PARAM { "," GENERIC_PARAM } "]"
GENERIC_PARAM  = [ VARIANCE_MARKER ] IDENT [ ":" TRAIT_BOUND { "+" TRAIT_BOUND } ]   // type parameter
               | "const" IDENT ":" TYPE                            // const parameter (e.g., Array size)
               | IDENT ":" "Dim"                                   // dim-kinded parameter (for shape-typed APIs)
               | "..." IDENT                                       // variadic shape parameter (Shape-kinded list)
               | "with" IDENT                                       // effect parameter (effect polymorphism)

VARIANCE_MARKER = "+"     // covariant — Foo[Sub] <: Foo[Super] when Sub <: Super
                | "-"     // contravariant — Foo[Super] <: Foo[Sub] when Sub <: Super
                | "="     // invariant (explicit form; same as no marker)
                          // No marker = invariant by default. Stdlib types
                          // declare variance explicitly per parameter; user
                          // types are invariant only at v1 (markers in user
                          // code are rejected with E_VARIANCE_USER_DECL_NOT_YET).
                          // See design.md § Variance.

GENERIC_ARGS     = "[" GENERIC_ARG_LIST "]"
GENERIC_ARG_LIST = GENERIC_ARG { "," GENERIC_ARG }
GENERIC_ARG      = TYPE | EXPR | SHAPE_LIT   // type arg, const expression, or shape literal

TRAIT_BOUND = PATH [ "[" TYPE_LIST "]" ]
```

Effect parameters (`with E`) are declared in the same generic list as type and const parameters. They propagate to the function's effect clause: `fn map[T, U, with E](list: Vec[T], f: Fn(T) -> U with E) -> Vec[U] with E`. See `design.md § Generics` for the effect-polymorphism rules.

**Dim and Shape kinds.** Dim-kinded params (`M: Dim`) are integer-valued at compile time but live in shape position (not expression position). A param that appears only inside shape literals is inferred Dim-kinded without an explicit `: Dim` annotation; the annotation is available for clarity. Variadic shape params `...S` bind a list of dims and splice into shape literals: `Tensor[T, [...S, M]]`. See `design.md § Numerical Types > Tensor` for the unification rules (including `?` dynamic-dim behavior).

```
fn sort[T: Ord](list: Vec[T], cmp: Fn(T, T) -> Ordering) -> Vec[T]
fn map[T, U](list: Vec[T], f: Fn(T) -> U with _) -> Vec[U]
fn run[T: Processor](p: T, data: Data) -> Result[Output, Error] with _
struct Graph[T] { nodes: Arena[Node[T]] }
fn dot(a: ref Array[f64, 3], b: ref Array[f64, 3]) -> f64   // const generic size

// Shape-typed signatures — dim-kinded params inferred from context
fn matmul[M, K, N](
    a: Tensor[f64, [M, K]],
    b: Tensor[f64, [K, N]],
) -> Tensor[f64, [M, N]]
fn reduce[T, ...S](t: Tensor[T, S]) -> T
fn transpose[T, ...S, M: Dim, N: Dim](
    t: Tensor[T, [...S, M, N]],
) -> Tensor[T, [...S, N, M]]
```

---

## 7. Effect Annotations

### 7.1 Effect List

```
EFFECT_LIST = "with" EFFECT_SPEC

EFFECT_SPEC = "_"                                 // effect polymorphism (any effects)
            | "()"                                 // explicitly pure (primarily for Fn types)
            | EFFECT_TERM { EFFECT_TERM }          // one or more terms, space-separated

EFFECT_TERM = EFFECT_VERB                         // reads(X), writes(Y), traces(X), etc.
            | IDENT                                // effect group reference

// Resource verbs (drive conflict analysis):
EFFECT_VERB = "reads" "(" RESOURCE_LIST ")"
            | "writes" "(" RESOURCE_LIST ")"
            | "sends" "(" RESOURCE_LIST ")"
            | "receives" "(" RESOURCE_LIST ")"
            | "allocates" "(" RESOURCE_LIST ")"
            | IDENT "(" RESOURCE_LIST ")"         // user-declared verb (§3.6)
            | "panics"
// Execution verbs (drive scheduler placement, no resource parameter):
            | "blocks"
            | "suspends"

RESOURCE_LIST = RESOURCE { "," RESOURCE }
RESOURCE      = PATH [ "[" EXPR "]" ]            // optional parameter (parameterized resources)
```

User-declared verbs — introduced via `effect verb NAME;` or `transparent effect verb NAME;` (§3.6) — parse the same way as the built-in resource verbs: `VERB_NAME "(" RESOURCE_LIST ")"`. Only the eight built-in verbs above are recognized as keywords; user-defined verb names occupy the `IDENT` alternative and must resolve to an `effect verb` declaration in scope.

`with` is the universal "effects start here" marker. Every effectful function signature reads as: `fn name(params) [-> Type] [with effects] [contracts] { body }`. Effects within the `with` clause are space-separated — the effect verb keywords (`reads`, `writes`, `sends`) provide sufficient visual separation. Effect group definitions use `+` because they are declarative equations, not annotation lists.

### 7.2 Placement

The `with` keyword always introduces the effect clause. It appears **after the return type** (or after the closing paren if no return type), **before any contracts and the opening brace or semicolon**:

```
fn f()                                  { ... }  // pure
fn f()                 with reads(X)    { ... }  // effect, no return type
fn f() -> T                             { ... }  // pure, with return type
fn f() -> T            with reads(X)    { ... }  // effect, with return type
fn f() -> T            with Group       { ... }  // effect group
fn f() -> T            with _           { ... }  // effect polymorphism
extern "C" fn f() -> T with writes(X);           // FFI (semicolon, no body)
fn f(self) -> T        with _;                   // trait method (semicolon, no body)
```

### 7.3 Equivalence

`reads(A) reads(B)` and `reads(A, B)` are equivalent for every resource verb (`reads`, `writes`, `sends`, `receives`, `allocates`, and any user-declared verb). The parser normalizes both to the same representation.

---

## 8. Attributes

```
ATTRIBUTES = { ATTRIBUTE }
ATTRIBUTE  = "#" "[" ATTR_PATH [ "(" ATTR_ARGS ")" ] "]"
           | "#" "[" ATTR_PATH "=" STRING "]"
             // e.g., #[must_use = "reason"]
ATTR_PATH  = IDENT { "::" IDENT }
             // bare names: #[derive(Eq)], #[used]
             // namespaced:  #[diagnostic::on_unimplemented(...)],
             //              #[rustfmt::skip]
ATTR_ARGS  = ATTR_ARG { "," ATTR_ARG }
ATTR_ARG   = IDENT ":" EXPR | IDENT
             // e.g., #[diagnostic::on_unimplemented(message: "...", label: "...")]
```

Attributes precede the item they annotate:

```
#[derive(Eq, Hash, Display)]
struct Point { x: i64, y: i64 }

// Tests are identified by test_ prefix in _test.kara files — no #[test] attribute.
// Use #[test(requires = [...])] only for resource-gating, #[property] for property tests.
fn test_addition() {
    assert(1 + 1 == 2);
}

#[no_rc]
fn hot_loop(data: ref Vec[Entity]) -> f64 { ... }

// Module-level attributes attach to individual items, not to `mod` declarations
// (which don't exist — modules are files). Per-item examples:
#[rc_budget(max: 5)]
fn process_order(o: Order) { ... }

// Target gating — closed target set: native, wasm_browser, wasm_wasi, gpu.
// Argument is a comma-separated list of bare target names, each optionally wrapped in not(...).
// No general boolean logic. See design.md § Cross-target Compilation.
#[target(native)]
pub fn main() -> Result[(), AppError] { ... }

#[target(wasm_browser, wasm_wasi)]
fn platform_info() -> String { ... }

#[target(not(gpu))]
fn io_helper() { ... }

#[allow(rc_fallback)]
fn flexible(condition: bool) -> String { ... }

#[concurrency(max_tasks: 100)]
fn batch_process(items: Vec[Item]) { ... }

#[must_use = "connections must be explicitly disconnected"]
struct Connection[State] { ... }

#[tailrec]
fn sum(acc: i64, n: i64) -> i64 {
    if n == 0 { acc }
    else { sum(acc + n, n - 1) }
}
```

Known attributes:

| Attribute | Scope | Purpose |
|---|---|---|
| `#[derive(Trait, ...)]` | Struct/Enum | Compiler generates trait implementations from fields |
| `#[default]` | Enum variant | Mark the variant returned by `#[derive(Default)]` on the enclosing enum. Exactly one variant per enum may carry the marker, and the marked variant must be field-less. Errors: `E_DEFAULT_NO_VARIANT_MARKED`, `E_DEFAULT_MULTIPLE_VARIANTS`, `E_DEFAULT_VARIANT_HAS_PAYLOAD`, `E_DEFAULT_ATTRIBUTE_INVALID_POSITION`, `E_DEFAULT_ATTRIBUTE_WITHOUT_DERIVE`. See design.md § `#[derive(Default)]` and `#[default]` on enum variants. |
| `#[must_use]` or `#[must_use = "reason"]` | Struct/Enum/Function | Warn if value is silently dropped (types) or return value discarded (functions). `Result` is implicitly `#[must_use]` |
| `#[deprecated]`, `#[deprecated = "note"]`, or `#[deprecated(since: "...", note: "...")]` | Function/Struct/Enum/Variant/Method/Trait/Type alias/Const | Emit the `deprecated` lint at every use site. Suppress with `#[allow(deprecated)]` / `#[expect(deprecated)]`. See design.md § `#[deprecated]` for Item Deprecation. |
| `#[diagnostic::on_unimplemented(message: "...", label: "...", note: "...")]` | Trait | Customize the diagnostic emitted when a bound `T: ThisTrait` fails. `{Self}` placeholder interpolates the offending type. Advisory — compiler may ignore. Malformed → warning, not error. See design.md § Diagnostic Namespace Attributes. |
| `#[diagnostic::do_not_recommend]` | `impl` block | Omit this impl from the "trait is implemented by ..." note in failed-bound diagnostics. Does not affect resolution or coherence. Advisory. |
| `#[TOOL::NAME(...)]` (any other multi-segment path) | Any item | Tool-namespaced attribute. Compiler parses, stores on AST, emits no diagnostic. External tools (formatters, linters, analyzers) read via `karac query attributes` or LSP. v1-reserved tool names: `karafmt::*`, `karalint::*`, `karadoc::*`. See design.md § Tool-Namespaced Attributes. |
| `#[tailrec]` | Function | Guarantee tail-call optimization — compile error if any recursive call is not in tail position |
| `#[no_rc]` | Function | Reject any RC fallback in this function |
| `#[rc_budget(max: N)]` | Module | Limit total RC values in this module |
| `#[allow(LINT)]` / `#[warn(LINT)]` / `#[deny(LINT)]` / `#[expect(LINT)]` | Item/Block/Module | Override level of named lint within scope. `#[expect]` additionally tracks whether the lint fired — emits `unfulfilled_lint_expectation` if it did not. See design.md § Lint Level Attributes. Multiple lints per attribute: `#[allow(rc_fallback, implicit_clone)]`. |
| `#[concurrency(max_tasks: N)]` | Function | Limit concurrent tasks spawned |
| `#[gpu]` | Function | Assert this function uses only GPU-compatible features |
| `#[inline]`, `#[inline(always)]`, `#[inline(never)]` | Function/Method/Trait method | Codegen hint controlling inlining at call sites. `#[inline]` is non-binding; `(always)` and `(never)` are strong directives. Only one inline-axis attribute per function (others are `error[E_INLINE_HINT_CONFLICT]`). See design.md § Codegen Hint Attributes. |
| `#[cold]` | Function/Method/Trait method | Codegen hint that the function is rarely executed — out-of-line placement, fall-through-favoring call-site lowering. Combinable with `#[inline]` and `#[inline(never)]`; conflicts with `#[inline(always)]`. |
| `#[repr(C)]`, `#[repr(packed)]`, `#[repr(align(N))]`, `#[repr(u8)]`, `#[repr(transparent)]` | Struct/Enum | Control ABI layout for FFI, alignment, and newtype transparency |
| `#[cyclic]` | Trait | Mark trait as used in cyclic `shared struct` graphs — compiler enforces `weak` on back-edges |
| `#[interrupt(NAME)]` | Function | ISR handler ABI — compiler generates interrupt entry/exit sequence |
| `#[used]` | Static/Const | Prevent dead-code elimination (linker keeps the symbol) |
| `#[unsafe(no_mangle)]` | Function/Static | Disable name mangling for FFI export. `#[unsafe(...)]` wrap mandatory; bare `#[no_mangle]` rejected at parse time. See design.md § Linker Control Attributes. |
| `#[unsafe(link_section("name"))]` | Function/Static | Place symbol in a specific linker section. `#[unsafe(...)]` wrap mandatory; bare `#[link_section(...)]` rejected at parse time. See design.md § Linker Control Attributes. |
| `#[allow(pure_loop_in_par)]` | Function/Block | Suppress pure-tight-loop-in-par warning |
| `#[property(cases: N)]` | Test function | Property-based test with custom iteration count |
| `#[snapshot]` | Test function | Snapshot test (output compared against saved baseline) |
| `#[test(requires = [...])]` | Test function | Declare resources needed for test |
| `#[with_provider(resource, constructor)]` | Test function | Per-test provider injection |
| `#[prefer_rc]` | Function/Module | Compiler queries channel resolution surface for the RC fallback decision (P1.1). Hint that the Phase 2 RC→Arc promotion respects in ambiguous cases; conflicts with `#[no_rc]`. See design.md § Compiler Queries and § Feature 4 Part 4. |
| `#[specialize(T = TYPE, ...)]` | Function | Compiler queries channel resolution surface for the specialization decision (P1.2). Author directs the compiler to specialize the body for the named monomorphization tuple. See design.md § Compiler Queries. |
| `#[likely]` / `#[unlikely]` | `match` arm / `if`-`else` branch | Compiler queries channel resolution surface for the branch-hint decision (P1.3). Author asserts the branch is hot or cold. See design.md § Compiler Queries and § Codegen Hint Attributes. |
| `#[fork_at(N)]` | Function | Compiler queries channel resolution surface for the auto-concurrency fork threshold decision (P1.6). Author overrides the cost-model fork threshold for this function's parallel groups. See design.md § Compiler Queries and § Auto-concurrency cost-model decisions. |
| `#[jit_template]` | Function (generic) | Marks a generic function whose IR should be embedded in the binary's `.kara_jit_template` section for runtime monomorphization (post-v1; see deferred.md § Runtime Monomorphization JIT). Author opt-in policy: only annotated generics ship bitcode in the binary; binary-size cost is bounded by per-author annotation rather than emergent. v1 reserves the section but does not emit bitcode; the attribute parses without effect under v1 codegen. |
| `#[unstable]` | Stdlib item (struct/enum/fn/method/trait/const) | Marks an API surface point as deliberately unstable across compiler releases — callers must opt in via `#[allow(unstable_api)]` (or globally via `kara.toml`). Used for advanced extension surfaces that ship at v1 but where the API shape may change before lock-in (e.g., `std.tls`'s low-level certificate-verifier customization, `std.http`'s connection-level customization). Distinct from `deferred.md` deferral — the item is *present* and usable, just gated. Stdlib-only at v1; user-side use is a future RFC. See design.md § v1 Positioning > Stable surface vs. unstable extension points. |

---

## 9. Modules and Paths

### 9.1 Module Structure

Modules are defined by the directory structure under `src/`. Each `.kara` file is its own module; the path from the project root (dots between segments) is its module path. There are no `mod` declarations — the parser rejects `mod name;` with a diagnostic directing the user to put the file in the appropriate directory. Entry files (`main.kara`, `lib.kara`) hoist their items to the crate root.

### 9.2 Imports

```
import std.collections.Map;
import mylib.db.UserDB;
```

### 9.3 Paths

```
PATH = IDENT { "." IDENT }
```

Used for:
- Type references: `std.collections.Map`
- Associated functions: `User.new()`, `Map.new()`
- Module-scoped access: `env.args()`
- Effect resource references: `mylib.UserDB`

### 9.4 Visibility

Kāra uses a three-tier visibility model:

| Keyword | Visible to |
|---|---|
| `pub` | End users + all project files |
| *(default, no keyword)* | All files in the project (not end users) |
| `private` | Same directory only |

- **`pub`** marks the public API — visible to end users who depend on your package. Public functions must declare effects explicitly.
- **Default (no keyword)** is project-internal — visible to all files in the project, but not to end users. This is the most common case. Private function effects are inferred.
- **`private`** restricts visibility to files in the same directory. Useful for internal helpers shared between related files (e.g., `db/connection.kara` and `db/schema.kara` sharing a helper in `db/helpers.kara`). Does not leak into parent or child directories.

```
pub fn public_api() { ... }            // visible to end users
fn project_internal() { ... }          // visible across the project
private fn dir_helper() { ... }        // visible only in this directory
pub struct PublicStruct { ... }
pub effect group OrderProcessing = ...;
pub let MAX: i64 = 100;
```

No `pub(crate)` or `pub(super)` variants. Directories are organizational folders for namespacing — they do not affect visibility beyond the `private` keyword.

### 9.5 Prelude

The following items from `std` are automatically imported into every Kāra source file without an explicit `use` statement:

**Types:**
`bool`, `i8`, `i16`, `i32`, `i64`, `i128`, `u8`, `u16`, `u32`, `u64`, `u128`, `f32`, `f64`, `F32`, `F64`, `char`, `String`, `StringSlice`, `Never`, `ExitCode`, `Option`, `Result`, `Vec`, `Map`, `Set`

**Enum variants:**
`Some`, `None`, `Ok`, `Err`

**I/O functions (stdlib):**

| Function | Signature | Purpose |
|---|---|---|
| `print(value)` | `fn print(value: impl Display) with writes(Stdout)` | Print to stdout, no newline |
| `println(value)` | `fn println(value: impl Display) with writes(Stdout)` | Print to stdout, with newline |
| `eprintln(value)` | `fn eprintln(value: impl Display) with writes(Stderr)` | Print to stderr, with newline |

Compiler builtins (§5.24) are also available everywhere without import.

The test prelude (`_test.kara` files) additionally imports test framework items like `Arbitrary`.

---

## 10. Entry Point

`main()` is not `pub`, so **effects are inferred** — no annotation needed.

**Allowed return types:** `()` (default), `Result[(), E: Display]`, or `ExitCode`. Any other return type is a compile error.

```
fn main() {
    // Program starts here.
    // No arguments — use env.args() with reads(Env) for CLI args.
    // No pub required.
    // No return type — implicit ().
    // Effects inferred from body — no annotation needed.
}

// main() can return Result for programs that propagate errors with ?:
// E must implement Display — the runtime prints "Error: {e}" to stderr on Err.
fn main() -> Result[(), Error] {
    let args = env.args();
    let content = read_file(args[1])?;
    print(content);
    Ok(())
}

// main() can return ExitCode for arbitrary exit codes:
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode.SUCCESS,
        Err(e) => {
            eprintln("Error: {e}");
            ExitCode.from(e.exit_code())
        }
    }
}
```

---

## 11. Semicolons and Separators

- **Statements** end with `;`
- **Last expression in a block** has no `;` — it's the block's return value
- **Match arms** separated by `,`
- **Struct/enum fields** separated by `,` (trailing comma allowed)
- **Function parameters** separated by `,` (trailing comma allowed)
- **Top-level items** (fn, struct, etc.) have no separator — just whitespace

```
fn example() -> i64 {
    let a = 1;          // statement — semicolon
    let b = 2;          // statement — semicolon
    a + b               // return expression — no semicolon
}
```

---

## 12. Design Decisions

Syntax choices where multiple reasonable options existed. Recorded here so the rationale is not lost:

| # | Ambiguity | Resolution |
|---|---|---|
| 1 | `mod` vs `module` keyword | `mod` (Rust convention) |
| 2 | Enum variant forms | All three: unit, tuple, struct |
| 3 | Effect resource provider trait | Optional — both `effect resource X;` and `effect resource X: Trait;` valid |
| 4 | `self` in methods | Explicit — instance methods have `self`, associated functions don't |
| 5 | Named arguments | Opt-in labels at call site; order must match declaration; no reordering |
| 6 | Entry point | `fn main()` — no `pub`, no args. CLI args via `env.args()` + `reads(Env)` |
| 7 | Duplicate effect verbs | `reads(A), reads(B)` and `reads(A, B)` are equivalent |
| 8 | `List[T]` vs `Vec[T]` | `Vec[T]` is the standard library type |
| 9 | `Fn` vs `fn` | `fn` declares functions, `Fn` is the closure/function type |
| 10 | `format()` vs `format!()` | `f"..."` string interpolation is the language feature (compiler-desugared, preferred for all literal format strings). `format()` is a stdlib function for dynamic format strings where the template is a runtime value (e.g., i18n, config-driven formatting) |
| 11 | Semicolons | Statements end with `;`, implicit return has no `;`, match arms use `,` (optional after block arms) |
