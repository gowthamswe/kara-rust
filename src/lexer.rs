// src/lexer.rs

//! Tokenizer for Kāra source code. Converts a character stream into tokens
//! with source location tracking (Span).

use crate::token::{FloatSuffix, IntSuffix, Span, SpannedToken, Token};

/// The Lexer holds state required to tokenize input source code.
pub struct Lexer<'a> {
    source: &'a [u8],
    start: usize,
    current: usize,
    line: usize,
    column: usize,
    start_column: usize,
}

impl<'a> Lexer<'a> {
    /// Creates a new Lexer for the given source code.
    pub fn new(source: &'a str) -> Self {
        Lexer {
            source: source.as_bytes(),
            start: 0,
            current: 0,
            line: 1,
            column: 1,
            start_column: 1,
        }
    }

    /// Scans and returns the next token with its source span.
    pub fn next_token(&mut self) -> SpannedToken {
        self.skip_whitespace();
        self.start = self.current;
        self.start_column = self.column;

        if self.is_at_end() {
            return self.make_spanned(Token::EOF);
        }

        let c = self.advance();

        match c {
            // Delimiters
            b'(' => self.make_spanned(Token::LeftParen),
            b')' => self.make_spanned(Token::RightParen),
            b'{' => self.make_spanned(Token::LeftBrace),
            b'}' => self.make_spanned(Token::RightBrace),
            b'[' => self.make_spanned(Token::LeftBracket),
            b']' => self.make_spanned(Token::RightBracket),

            // Punctuation
            b',' => self.make_spanned(Token::Comma),
            b';' => self.make_spanned(Token::Semicolon),
            b'#' => {
                // Reserved `#`-guarded string syntax (v60 item 11). The forms
                // `#"..."#`, `##"..."##`, `###"..."###`, etc. (Rust-style raw
                // strings) and any `#`-followed-by-`#`-or-`"` cluster are
                // reserved at v1; emit a focused diagnostic and consume the
                // entire cluster + optional string body for clean recovery.
                // `#[attr]` continues to lex as `Token::Pound` followed by
                // `Token::LeftBracket`, unchanged.
                if self.peek() == b'#' || self.peek() == b'"' {
                    self.reserved_hash_guarded_string()
                } else {
                    self.make_spanned(Token::Pound)
                }
            }
            b'@' => self.make_spanned(Token::At),
            b'~' => self.make_spanned(Token::Tilde),

            // Colon (path separator is now `.`)
            b':' => self.make_spanned(Token::Colon),

            // Dot / DotDot / DotDotEq
            b'.' => {
                if self.match_char(b'.') {
                    if self.match_char(b'=') {
                        self.make_spanned(Token::DotDotEq)
                    } else {
                        self.make_spanned(Token::DotDot)
                    }
                } else {
                    self.make_spanned(Token::Dot)
                }
            }

            // Question / QuestionDot / QuestionQuestion
            b'?' => {
                if self.match_char(b'.') {
                    self.make_spanned(Token::QuestionDot)
                } else if self.match_char(b'?') {
                    self.make_spanned(Token::QuestionQuestion)
                } else {
                    self.make_spanned(Token::Question)
                }
            }

            // Arithmetic
            b'+' => {
                if self.match_char(b'=') {
                    self.make_spanned(Token::PlusEqual)
                } else {
                    self.make_spanned(Token::Plus)
                }
            }
            b'*' => {
                if self.match_char(b'=') {
                    self.make_spanned(Token::StarEqual)
                } else {
                    self.make_spanned(Token::Star)
                }
            }
            b'/' => {
                if self.peek() == b'/' && self.peek_next() == b'/' {
                    // Doc comment: ///
                    self.advance(); // second /
                    self.advance(); // third /
                                    // Skip optional leading space
                    if self.peek() == b' ' {
                        self.advance();
                    }
                    let start = self.current;
                    while self.peek() != b'\n' && !self.is_at_end() {
                        self.advance();
                    }
                    let text = std::str::from_utf8(&self.source[start..self.current])
                        .unwrap()
                        .to_string();
                    self.make_spanned(Token::DocComment(text))
                } else if self.peek() == b'/' && self.peek_next() == b'!' {
                    // Module-level doc comment: //!
                    self.advance(); // second /
                    self.advance(); // !
                                    // Skip optional leading space
                    if self.peek() == b' ' {
                        self.advance();
                    }
                    let start = self.current;
                    while self.peek() != b'\n' && !self.is_at_end() {
                        self.advance();
                    }
                    let text = std::str::from_utf8(&self.source[start..self.current])
                        .unwrap()
                        .to_string();
                    self.make_spanned(Token::ModuleDocComment(text))
                } else if self.match_char(b'=') {
                    self.make_spanned(Token::SlashEqual)
                } else {
                    self.make_spanned(Token::Slash)
                }
            }
            b'%' => {
                if self.match_char(b'=') {
                    self.make_spanned(Token::PercentEqual)
                } else {
                    self.make_spanned(Token::Percent)
                }
            }

            // Minus / MinusEqual / Arrow
            b'-' => {
                if self.match_char(b'>') {
                    self.make_spanned(Token::Arrow)
                } else if self.match_char(b'=') {
                    self.make_spanned(Token::MinusEqual)
                } else {
                    self.make_spanned(Token::Minus)
                }
            }

            // Equal / EqualEqual / FatArrow
            b'=' => {
                if self.match_char(b'=') {
                    self.make_spanned(Token::EqualEqual)
                } else if self.match_char(b'>') {
                    self.make_spanned(Token::FatArrow)
                } else {
                    self.make_spanned(Token::Equal)
                }
            }

            // Bang / BangEqual
            b'!' => {
                if self.match_char(b'=') {
                    self.make_spanned(Token::BangEqual)
                } else {
                    self.make_spanned(Token::Bang)
                }
            }

            // Less / LessEqual / LessLess / LessLessEqual
            b'<' => {
                if self.match_char(b'=') {
                    self.make_spanned(Token::LessThanOrEqual)
                } else if self.match_char(b'<') {
                    if self.match_char(b'=') {
                        self.make_spanned(Token::LessLessEqual)
                    } else {
                        self.make_spanned(Token::LessLess)
                    }
                } else {
                    self.make_spanned(Token::LessThan)
                }
            }

            // Greater / GreaterEqual / GreaterGreater / GreaterGreaterEqual
            b'>' => {
                if self.match_char(b'=') {
                    self.make_spanned(Token::GreaterThanOrEqual)
                } else if self.match_char(b'>') {
                    if self.match_char(b'=') {
                        self.make_spanned(Token::GreaterGreaterEqual)
                    } else {
                        self.make_spanned(Token::GreaterGreater)
                    }
                } else {
                    self.make_spanned(Token::GreaterThan)
                }
            }

            // Amp / AmpAmp / AmpEqual
            b'&' => {
                if self.match_char(b'&') {
                    self.make_spanned(Token::AmpAmp)
                } else if self.match_char(b'=') {
                    self.make_spanned(Token::AmpEqual)
                } else {
                    self.make_spanned(Token::Amp)
                }
            }

            // Pipe / PipePipe / PipeArrow / PipeEqual
            b'|' => {
                if self.match_char(b'|') {
                    self.make_spanned(Token::PipePipe)
                } else if self.match_char(b'>') {
                    self.make_spanned(Token::PipeArrow)
                } else if self.match_char(b'=') {
                    self.make_spanned(Token::PipeEqual)
                } else {
                    self.make_spanned(Token::Pipe)
                }
            }

            // Caret / CaretEqual
            b'^' => {
                if self.match_char(b'=') {
                    self.make_spanned(Token::CaretEqual)
                } else {
                    self.make_spanned(Token::Caret)
                }
            }

            // String literals
            b'"' => {
                if self.peek() == b'"' && self.peek_next() == b'"' {
                    self.advance(); // second "
                    self.advance(); // third "
                    self.multi_string()
                } else {
                    self.string()
                }
            }

            // Character literals
            b'\'' => self.char_literal(),

            // Interpolated String literals
            b'f' if self.peek() == b'"' => {
                self.advance(); // consume '"'
                self.interpolated_string()
            }

            // Raw-identifier escape `r#NAME` (design.md § Raw Identifiers).
            // `r"` is the reserved-string-prefix path (handled below); `r#"..."#`
            // is the reserved hash-string form (caught later via the lone `r`
            // identifier + the `#`-dispatched `reserved_hash_guarded_string`).
            // Only `r#` followed by an identifier-start byte enters this path.
            b'r' if self.peek() == b'#' && is_alpha(self.peek_next()) => self.raw_identifier(),

            // Reserved single-letter string-prefix syntax (v60 item 10).
            // `f"..."` is already handled above. Every other ASCII-alphabetic
            // single-letter prefix immediately followed by `"` is reserved at
            // v1; emit a focused diagnostic and consume the string body for
            // clean error recovery so the parser sees one diagnostic, not a
            // cascade. The `_` underscore prefix (`_"..."`) is also rejected
            // for consistency.
            _ if (c.is_ascii_alphabetic() || c == b'_') && self.peek() == b'"' => {
                self.reserved_prefix_string(c)
            }

            // Numbers
            _ if is_digit(c) => self.number(),

            // Identifiers and keywords
            _ if is_alpha(c) => self.identifier(),

            // Non-ASCII byte: decode the full UTF-8 codepoint and dispatch.
            // A letter-class codepoint enters the would-be-identifier recovery
            // path (one focused diagnostic per token); any other codepoint
            // becomes a clean single-character "Unexpected character" error
            // (rather than the per-byte cascade the byte-based lexer would
            // otherwise produce for multi-byte sequences). Non-ASCII identifiers
            // are deferred per design.md § Identifiers — Unicode subsection.
            _ if c >= 0x80 => self.non_ascii_at_lead(c),

            // Unknown character (ASCII, not handled above)
            _ => {
                let msg = format!("Unexpected character: '{}'", c as char);
                self.make_spanned(Token::Error(msg))
            }
        }
    }

    // ── Escape sequence helpers ─────────────────────────────

    /// Parse a `\u{XXXX}` unicode escape sequence.
    /// Assumes the `u` has already been consumed.
    fn parse_unicode_escape(&mut self) -> Result<char, String> {
        if !self.match_char(b'{') {
            return Err("Expected '{' after \\u".to_string());
        }
        let hex_start = self.current;
        while self.peek() != b'}' && !self.is_at_end() {
            self.advance();
        }
        if self.is_at_end() {
            return Err("Unterminated unicode escape".to_string());
        }
        let hex_str = std::str::from_utf8(&self.source[hex_start..self.current]).unwrap();
        self.advance(); // consume '}'
        match u32::from_str_radix(hex_str, 16) {
            Ok(code) => match char::from_u32(code) {
                Some(c) => Ok(c),
                None => Err(format!("Invalid unicode scalar value: \\u{{{}}}", hex_str)),
            },
            Err(_) => Err(format!("Invalid unicode escape: \\u{{{}}}", hex_str)),
        }
    }

    // ── Scanning helpers ──────────────────────────────────────

    /// Try to consume an integer type suffix (i8, i16, ..., u128) at the current position.
    fn try_int_suffix(&mut self) -> Option<IntSuffix> {
        use IntSuffix::*;
        let remaining = &self.source[self.current..];
        let candidates: &[(&[u8], IntSuffix)] = &[
            (b"i128", I128),
            (b"i64", I64),
            (b"i32", I32),
            (b"i16", I16),
            (b"i8", I8),
            (b"u128", U128),
            (b"u64", U64),
            (b"u32", U32),
            (b"u16", U16),
            (b"u8", U8),
        ];
        for &(pat, suffix) in candidates {
            if remaining.len() >= pat.len()
                && &remaining[..pat.len()] == pat
                && (remaining.len() == pat.len()
                    || !is_alpha(remaining[pat.len()]) && !is_digit(remaining[pat.len()]))
            {
                for _ in 0..pat.len() {
                    self.advance();
                }
                return Some(suffix);
            }
        }
        None
    }

    /// Try to consume a float type suffix (f32, f64) at the current position.
    fn try_float_suffix(&mut self) -> Option<FloatSuffix> {
        use FloatSuffix::*;
        let remaining = &self.source[self.current..];
        let candidates: &[(&[u8], FloatSuffix)] = &[(b"f64", F64), (b"f32", F32)];
        for &(pat, suffix) in candidates {
            if remaining.len() >= pat.len()
                && &remaining[..pat.len()] == pat
                && (remaining.len() == pat.len()
                    || !is_alpha(remaining[pat.len()]) && !is_digit(remaining[pat.len()]))
            {
                for _ in 0..pat.len() {
                    self.advance();
                }
                return Some(suffix);
            }
        }
        None
    }

    fn number(&mut self) -> SpannedToken {
        // Check for hex (0x), binary (0b), octal (0o)
        if self.source[self.start] == b'0' && !self.is_at_end() {
            match self.peek() {
                b'x' | b'X' => return self.hex_number(),
                b'b' | b'B' => return self.binary_number(),
                b'o' | b'O' => return self.octal_number(),
                _ => {}
            }
        }

        while is_digit(self.peek()) || self.peek() == b'_' {
            self.advance();
        }

        let mut is_float = false;

        // Consume decimal part if present.
        // Accept `N.DIGITS` and also `N.eEXP` (i.e., `1.e10` → `1.0e10`).
        if self.peek() == b'.' {
            let after_dot = self.peek_next();
            let is_exp_after_dot =
                (after_dot == b'e' || after_dot == b'E') && is_exp_start(self.peek_at(2));
            if is_digit(after_dot) || is_exp_after_dot {
                is_float = true;
                self.advance(); // consume '.'
                while is_digit(self.peek()) || self.peek() == b'_' {
                    self.advance();
                }
            }
        }

        // Consume exponent part if present: `e` | `E` followed by optional `+`/`-` and digits.
        if self.peek() == b'e' || self.peek() == b'E' {
            is_float = true;
            self.advance(); // consume 'e'/'E'
            if self.peek() == b'+' || self.peek() == b'-' {
                self.advance(); // consume optional sign
            }
            if !is_digit(self.peek()) {
                return self.make_spanned(Token::Error("exponent has no digits".to_string()));
            }
            while is_digit(self.peek()) || self.peek() == b'_' {
                self.advance();
            }
        }

        if is_float {
            let text: String = self.token_text().chars().filter(|&c| c != '_').collect();
            let suffix = self.try_float_suffix();
            match text.parse::<f64>() {
                Ok(v) => self.make_spanned(Token::Float(v, suffix)),
                Err(_) => self.make_spanned(Token::Error("Invalid float literal".to_string())),
            }
        } else {
            let text: String = self.token_text().chars().filter(|&c| c != '_').collect();
            let suffix = self.try_int_suffix();
            match text.parse::<i64>() {
                Ok(v) => self.make_spanned(Token::Integer(v, suffix)),
                Err(_) => self.make_spanned(Token::Error("Invalid integer literal".to_string())),
            }
        }
    }

    fn hex_number(&mut self) -> SpannedToken {
        self.advance(); // consume 'x'
        while is_hex_digit(self.peek()) || self.peek() == b'_' {
            self.advance();
        }
        let text: String = self.token_text()[2..]
            .chars()
            .filter(|&c| c != '_')
            .collect();
        let suffix = self.try_int_suffix();
        match i64::from_str_radix(&text, 16) {
            Ok(v) => self.make_spanned(Token::Integer(v, suffix)),
            Err(_) => self.make_spanned(Token::Error("Invalid hex literal".to_string())),
        }
    }

    fn binary_number(&mut self) -> SpannedToken {
        self.advance(); // consume 'b'
        while self.peek() == b'0' || self.peek() == b'1' || self.peek() == b'_' {
            self.advance();
        }
        let text: String = self.token_text()[2..]
            .chars()
            .filter(|&c| c != '_')
            .collect();
        let suffix = self.try_int_suffix();
        match i64::from_str_radix(&text, 2) {
            Ok(v) => self.make_spanned(Token::Integer(v, suffix)),
            Err(_) => self.make_spanned(Token::Error("Invalid binary literal".to_string())),
        }
    }

    fn octal_number(&mut self) -> SpannedToken {
        self.advance(); // consume 'o'
        while (self.peek() >= b'0' && self.peek() <= b'7') || self.peek() == b'_' {
            self.advance();
        }
        let text: String = self.token_text()[2..]
            .chars()
            .filter(|&c| c != '_')
            .collect();
        let suffix = self.try_int_suffix();
        match i64::from_str_radix(&text, 8) {
            Ok(v) => self.make_spanned(Token::Integer(v, suffix)),
            Err(_) => self.make_spanned(Token::Error("Invalid octal literal".to_string())),
        }
    }

    /// Reserved `#`-guarded string syntax (v60 item 11). The opening `#`
    /// has been consumed; the next byte is either another `#` or the
    /// opening `"`. Consume the full cluster — any number of leading `#`s,
    /// optionally followed by a `"..."` body — so the diagnostic replaces
    /// the entire form.
    fn reserved_hash_guarded_string(&mut self) -> SpannedToken {
        let mut leading_hashes = 1; // the `#` consumed by the dispatch
        while self.peek() == b'#' {
            self.advance();
            leading_hashes += 1;
        }
        let consumed_string = self.peek() == b'"';
        if consumed_string {
            // Scan to a matching `"` followed by the same number of `#`s,
            // or to the next newline / EOF for clean recovery.
            self.advance(); // opening `"`
            loop {
                if self.is_at_end() || self.peek() == b'\n' {
                    break;
                }
                if self.peek() == b'"' {
                    self.advance();
                    let mut trailing = 0;
                    while self.peek() == b'#' && trailing < leading_hashes {
                        self.advance();
                        trailing += 1;
                    }
                    if trailing == leading_hashes {
                        break;
                    }
                } else if self.peek() == b'\\' {
                    self.advance();
                    if !self.is_at_end() {
                        self.advance();
                    }
                } else {
                    self.advance();
                }
            }
        }
        let msg = if consumed_string {
            format!(
                "`#`-guarded string syntax (`{}\"...\"{}`) is reserved for future use; not available in v1",
                "#".repeat(leading_hashes),
                "#".repeat(leading_hashes)
            )
        } else {
            format!(
                "`{}` (multi-`#` cluster) is reserved for future use; only `#[...]` attribute syntax is recognized in v1",
                "#".repeat(leading_hashes)
            )
        };
        self.make_spanned(Token::Error(msg))
    }

    /// Reserved single-letter string-prefix syntax (v60 item 10). The opening
    /// `prefix` letter has been consumed; the next byte is the opening `"`.
    /// We consume the string body — handling escape sequences identically to
    /// the regular string lexer — so the error token replaces the entire
    /// prefix-string construct, not just the prefix.
    ///
    /// `c"..."` (v60 item 18) is specced at v1 but lex acceptance ships in
    /// Phase 5.2 alongside parser handling and the stdlib `CStr` type; until
    /// then it gets a focused diagnostic pointing at design.md rather than the
    /// generic reserved-prefix message.
    fn reserved_prefix_string(&mut self, prefix: u8) -> SpannedToken {
        self.advance(); // consume opening `"`
        while self.peek() != b'"' && !self.is_at_end() {
            if self.peek() == b'\n' {
                self.line += 1;
                self.column = 0;
            }
            if self.peek() == b'\\' {
                self.advance();
                if !self.is_at_end() {
                    self.advance();
                }
            } else {
                self.advance();
            }
        }
        if self.peek() == b'"' {
            self.advance(); // closing quote
        }
        let msg = if prefix == b'c' {
            "string prefix 'c\"...\"' (C-string literal) is specced for v1 but lex acceptance lands in Phase 5.2; see design.md § C-String Literals".to_string()
        } else {
            format!(
                "string prefix '{}\"...\"' is reserved for future use; only `f\"...\"` is recognized in v1",
                prefix as char
            )
        };
        self.make_spanned(Token::Error(msg))
    }

    fn string(&mut self) -> SpannedToken {
        let mut value = String::new();
        while self.peek() != b'"' && !self.is_at_end() {
            if self.peek() == b'\n' {
                self.line += 1;
                self.column = 0;
            }
            if self.peek() == b'\\' {
                self.advance(); // consume backslash
                if self.is_at_end() {
                    return self.make_spanned(Token::Error(
                        "Unterminated string: trailing backslash".to_string(),
                    ));
                }
                match self.peek() {
                    b'n' => {
                        self.advance();
                        value.push('\n');
                    }
                    b't' => {
                        self.advance();
                        value.push('\t');
                    }
                    b'r' => {
                        self.advance();
                        value.push('\r');
                    }
                    b'\\' => {
                        self.advance();
                        value.push('\\');
                    }
                    b'"' => {
                        self.advance();
                        value.push('"');
                    }
                    b'0' => {
                        self.advance();
                        value.push('\0');
                    }
                    b'u' => {
                        self.advance();
                        match self.parse_unicode_escape() {
                            Ok(c) => value.push(c),
                            Err(msg) => return self.make_spanned(Token::Error(msg)),
                        }
                    }
                    _ => {
                        let c = self.consume_codepoint();
                        return self
                            .make_spanned(Token::Error(format!("Unknown escape sequence: \\{c}")));
                    }
                }
            } else {
                value.push(self.consume_codepoint());
            }
        }

        if self.is_at_end() {
            return self.make_spanned(Token::Error("Unterminated string".to_string()));
        }

        self.advance(); // closing quote
        self.make_spanned(Token::StringLiteral(value))
    }

    fn char_literal(&mut self) -> SpannedToken {
        if self.is_at_end() {
            return self.make_spanned(Token::Error("Unterminated character literal".to_string()));
        }

        let ch = if self.peek() == b'\\' {
            self.advance(); // consume backslash
            if self.is_at_end() {
                return self
                    .make_spanned(Token::Error("Unterminated character literal".to_string()));
            }
            // Escape selectors are ASCII; peek to dispatch, then consume.
            match self.peek() {
                b'n' => {
                    self.advance();
                    '\n'
                }
                b't' => {
                    self.advance();
                    '\t'
                }
                b'r' => {
                    self.advance();
                    '\r'
                }
                b'\\' => {
                    self.advance();
                    '\\'
                }
                b'\'' => {
                    self.advance();
                    '\''
                }
                b'0' => {
                    self.advance();
                    '\0'
                }
                b'u' => {
                    self.advance();
                    // Unicode escape: \u{XXXX}
                    match self.parse_unicode_escape() {
                        Ok(c) => c,
                        Err(msg) => return self.make_spanned(Token::Error(msg)),
                    }
                }
                _ => {
                    let other = self.consume_codepoint();
                    return self.make_spanned(Token::Error(format!(
                        "Unknown escape sequence in character literal: \\{other}"
                    )));
                }
            }
        } else {
            self.consume_codepoint()
        };

        if self.is_at_end() || self.peek() != b'\'' {
            return self.make_spanned(Token::Error("Unterminated character literal".to_string()));
        }
        self.advance(); // closing quote
        self.make_spanned(Token::CharLiteral(ch))
    }

    fn multi_string(&mut self) -> SpannedToken {
        let mut value = String::new();
        loop {
            if self.is_at_end() {
                return self
                    .make_spanned(Token::Error("Unterminated multi-line string".to_string()));
            }
            if self.peek() == b'"'
                && self.peek_next() == b'"'
                && self.current + 2 < self.source.len()
                && self.source[self.current + 2] == b'"'
            {
                self.advance(); // 1st
                self.advance(); // 2nd
                self.advance(); // 3rd
                break;
            }
            if self.peek() == b'\n' {
                self.line += 1;
                self.column = 0;
            }
            value.push(self.consume_codepoint());
        }
        self.make_spanned(Token::MultiStringLiteral(value))
    }

    fn interpolated_string(&mut self) -> SpannedToken {
        use crate::token::InterpolationPart;
        let mut parts = Vec::new();
        let mut current_text = String::new();

        while self.peek() != b'"' && !self.is_at_end() {
            if self.peek() == b'\n' {
                self.line += 1;
                self.column = 0;
            }

            if self.peek() == b'{' {
                if !current_text.is_empty() {
                    parts.push(InterpolationPart::Text(current_text.clone()));
                    current_text.clear();
                }
                self.advance(); // consume '{'
                let mut expr_text = String::new();
                let mut brace_depth = 1;
                while brace_depth > 0 && !self.is_at_end() {
                    let c = self.peek();
                    if c == b'{' {
                        brace_depth += 1;
                    } else if c == b'}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            self.advance(); // consume '}'
                            break;
                        }
                    }
                    if c == b'\n' {
                        self.line += 1;
                        self.column = 0;
                    }
                    expr_text.push(self.consume_codepoint());
                }
                parts.push(InterpolationPart::Expr(expr_text));
            } else if self.peek() == b'\\' {
                self.advance(); // consume backslash
                match self.peek() {
                    b'n' => {
                        self.advance();
                        current_text.push('\n');
                    }
                    b't' => {
                        self.advance();
                        current_text.push('\t');
                    }
                    b'r' => {
                        self.advance();
                        current_text.push('\r');
                    }
                    b'\\' => {
                        self.advance();
                        current_text.push('\\');
                    }
                    b'"' => {
                        self.advance();
                        current_text.push('"');
                    }
                    b'{' => {
                        self.advance();
                        current_text.push('{');
                    } // escaped brace
                    b'}' => {
                        self.advance();
                        current_text.push('}');
                    } // escaped brace
                    b'0' => {
                        self.advance();
                        current_text.push('\0');
                    }
                    b'u' => {
                        self.advance();
                        match self.parse_unicode_escape() {
                            Ok(c) => current_text.push(c),
                            Err(msg) => return self.make_spanned(Token::Error(msg)),
                        }
                    }
                    _ => {
                        let c = self.consume_codepoint();
                        return self
                            .make_spanned(Token::Error(format!("Unknown escape sequence: \\{c}")));
                    }
                }
            } else {
                current_text.push(self.consume_codepoint());
            }
        }

        if self.is_at_end() {
            return self.make_spanned(Token::Error("Unterminated interpolated string".to_string()));
        }

        self.advance(); // closing quote
        if !current_text.is_empty() {
            parts.push(InterpolationPart::Text(current_text));
        }

        self.make_spanned(Token::InterpolatedStringLiteral(parts))
    }

    /// Raw-identifier escape `r#NAME` (design.md § Raw Identifiers). On entry
    /// the leading `r` has been consumed and `self.peek()` is `#`. The keyword
    /// table is bypassed entirely so reserved-for-future-use words can flow
    /// through as ordinary identifiers; structural markers (`self`, `Self`, `_`,
    /// `super`, `crate`, `mod`, `pub`, `priv`, `private`, `mut`, `ref`, `own`)
    /// remain unescapable and are rejected here.
    fn raw_identifier(&mut self) -> SpannedToken {
        self.advance(); // consume `#`
        let name_start = self.current;
        while is_alpha(self.peek()) || is_digit(self.peek()) {
            self.advance();
        }
        let name = std::str::from_utf8(&self.source[name_start..self.current]).unwrap();
        if matches!(
            name,
            "self"
                | "Self"
                | "_"
                | "super"
                | "crate"
                | "mod"
                | "pub"
                | "priv"
                | "private"
                | "mut"
                | "ref"
                | "own"
        ) {
            return self.make_spanned(Token::Error(format!(
                "'r#{name}' is not legal; '{name}' is a structural marker, not a reservable keyword"
            )));
        }
        self.make_spanned(Token::Identifier {
            name: name.to_string(),
            raw: true,
        })
    }

    fn identifier(&mut self) -> SpannedToken {
        let mut non_ascii_seen: Option<char> = None;
        loop {
            if is_alpha(self.peek()) || is_digit(self.peek()) {
                self.advance();
                continue;
            }
            if let Some(nc) = self.try_consume_non_ascii_ident_continue() {
                if non_ascii_seen.is_none() {
                    non_ascii_seen = Some(nc);
                }
                continue;
            }
            break;
        }

        if let Some(nc) = non_ascii_seen {
            return self.make_spanned(Token::Error(format!(
                "non-ASCII identifier (containing '{nc}') is deferred to a future edition; identifiers must be ASCII in v1"
            )));
        }

        let text = self.token_text();
        let token = match text {
            // Declarations
            "fn" => Token::Fn,
            "struct" => Token::Struct,
            "enum" => Token::Enum,
            "trait" => Token::Trait,
            "impl" => Token::Impl,
            "mod" => Token::Mod,
            "use" => Token::Use,
            "import" => Token::Import,
            "const" => Token::Const,
            "type" => Token::Type,
            "distinct" => Token::Distinct,
            // Visibility
            "pub" => Token::Pub,
            "private" => Token::Private,
            // Control flow
            "if" => Token::If,
            "else" => Token::Else,
            "match" => Token::Match,
            "while" => Token::While,
            "for" => Token::For,
            "in" => Token::In,
            "loop" => Token::Loop,
            "return" => Token::Return,
            "break" => Token::Break,
            "continue" => Token::Continue,
            "defer" => Token::Defer,
            "errdefer" => Token::ErrDefer,
            "asm" => Token::Asm,
            "global_asm" => Token::GlobalAsm,
            // Bindings
            "let" => Token::Let,
            "mut" => Token::Mut,
            // Logical (keyword forms; `&&`/`||`/`!` are rejected by the parser)
            "and" => Token::And,
            "or" => Token::Or,
            "not" => Token::Not,
            // Ownership
            "own" => Token::Own,
            "ref" => Token::Ref,
            "weak" => Token::Weak,
            "lock" => Token::Lock,
            // Closure capture: `own |...|` is the explicit capture-by-value
            // prefix (see Rule 2½). `move` is reserved against the Rust idiom
            // and is rejected by the parser with a redirect to `own`.
            "move" => Token::Move,
            // Effects
            "effect" => Token::Effect,
            "resource" => Token::Resource,
            "verb" => Token::Verb,
            "reads" => Token::Reads,
            "writes" => Token::Writes,
            "sends" => Token::Sends,
            "receives" => Token::Receives,
            "allocates" => Token::Allocates,
            "panics" => Token::Panics,
            "blocks" => Token::Blocks,
            "suspends" => Token::Suspends,
            "with" => Token::With,
            "transparent" => Token::Transparent,
            "stable" => Token::Stable,
            "seq" => Token::Seq,
            "par" => Token::Par,
            "yield" => Token::Yield,
            // Type system
            "as" => Token::As,
            "where" => Token::Where,
            "dyn" => Token::Dyn,
            // Contracts
            "requires" => Token::Requires,
            "ensures" => Token::Ensures,
            "invariant" => Token::Invariant,
            // Safety
            "unsafe" => Token::Unsafe,
            "extern" => Token::Extern,
            "shared" => Token::Shared,
            // Layout
            "layout" => Token::Layout,
            "group" => Token::Group,
            // Literals
            "true" => Token::True,
            "false" => Token::False,
            // Providers
            "providers" => Token::Providers,
            // Other
            "alias" => Token::Alias,
            "independent" => Token::Independent,
            "self" => Token::SelfValue,
            "Self" => Token::SelfType,
            // Underscore as identifier
            "_" => Token::Underscore,
            // Reserved future numeric type keywords — not available until Phase 7.
            // Emit a lexer error so the compiler rejects these as identifiers now,
            // preventing a source-breaking rename when the types ship.
            "f16" => Token::Error(
                "'f16' is a reserved keyword for a future numeric type; not available until Phase 7".to_string(),
            ),
            "bf16" => Token::Error(
                "'bf16' is a reserved keyword for a future numeric type; not available until Phase 7".to_string(),
            ),
            // Reserved-for-future-use keywords — see design.md § Reserved-for-Future-Use Keywords.
            "gen" | "become" | "do" | "final" | "override" | "priv" | "try" | "typeof"
            | "virtual" | "async" | "await" | "comptime" | "pure" | "box" => Token::Error(
                format!("'{text}' is reserved for future use and cannot be used as an identifier"),
            ),
            // Regular identifier
            _ => Token::Identifier {
                name: text.to_string(),
                raw: false,
            },
        };
        self.make_spanned(token)
    }

    // ── Whitespace and comments ───────────────────────────────

    fn skip_whitespace(&mut self) {
        loop {
            match self.peek() {
                b' ' | b'\r' | b'\t' => {
                    self.advance();
                }
                b'\n' => {
                    self.line += 1;
                    self.column = 0; // will be incremented by advance()
                    self.advance();
                }
                b'/' if self.peek_next() == b'/' => {
                    // Check for doc comment (`///`) or module-level doc
                    // comment (`//!`); either way, hand off to scan_token.
                    if self.current + 2 < self.source.len()
                        && (self.source[self.current + 2] == b'/'
                            || self.source[self.current + 2] == b'!')
                    {
                        break;
                    }
                    // Regular line comment
                    while self.peek() != b'\n' && !self.is_at_end() {
                        self.advance();
                    }
                }
                b'/' if self.peek_next() == b'*' => {
                    // Block comment (with nesting)
                    self.advance(); // consume '/'
                    self.advance(); // consume '*'
                    let mut depth = 1;
                    while depth > 0 && !self.is_at_end() {
                        if self.peek() == b'/' && self.peek_next() == b'*' {
                            self.advance();
                            self.advance();
                            depth += 1;
                        } else if self.peek() == b'*' && self.peek_next() == b'/' {
                            self.advance();
                            self.advance();
                            depth -= 1;
                        } else {
                            if self.peek() == b'\n' {
                                self.line += 1;
                                self.column = 0;
                            }
                            self.advance();
                        }
                    }
                }
                _ => break,
            }
        }
    }

    // ── Non-ASCII recovery ────────────────────────────────────

    /// Dispatched from `next_token` when the leading byte is `>= 0x80`. The
    /// lead byte was already consumed (and `column` advanced by 1); we read
    /// the remaining UTF-8 continuation bytes without further column bumps so
    /// the span advances by one *codepoint*, not one byte. A letter-class
    /// codepoint then enters the would-be-identifier recovery path so the
    /// diagnostic spans the full token (e.g. all of `αβγ`) rather than firing
    /// once per codepoint.
    fn non_ascii_at_lead(&mut self, lead: u8) -> SpannedToken {
        let len = utf8_byte_len(lead).unwrap_or(1);
        for _ in 1..len {
            if self.current < self.source.len() {
                self.current += 1;
            }
        }
        let cp = std::str::from_utf8(&self.source[self.start..self.current])
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or('\u{FFFD}');
        if cp.is_alphabetic() || cp == '_' {
            loop {
                if is_alpha(self.peek()) || is_digit(self.peek()) {
                    self.advance();
                    continue;
                }
                if self.try_consume_non_ascii_ident_continue().is_some() {
                    continue;
                }
                break;
            }
            self.make_spanned(Token::Error(format!(
                "non-ASCII identifier (containing '{cp}') is deferred to a future edition; identifiers must be ASCII in v1"
            )))
        } else {
            self.make_spanned(Token::Error(format!("Unexpected character: '{cp}'")))
        }
    }

    /// Consume the next codepoint (1–4 UTF-8 bytes) and return it. ASCII bytes
    /// advance via the normal `advance()` path; multi-byte sequences advance
    /// `current` by their byte length but `column` by exactly one (codepoint).
    /// Invalid UTF-8 falls back to U+FFFD with a single-byte advance — source
    /// is already validated UTF-8 at the lib boundary, so this is a defensive
    /// fallback rather than a normal path. Used by string / char / multi-string
    /// / interpolated-string body lexers so non-ASCII codepoints land in the
    /// resulting `String` as one `char`, not a sequence of misinterpreted bytes.
    fn consume_codepoint(&mut self) -> char {
        let lead = self.peek();
        if lead < 0x80 {
            return self.advance() as char;
        }
        let len = utf8_byte_len(lead).unwrap_or(1);
        let end = (self.current + len).min(self.source.len());
        let cp = std::str::from_utf8(&self.source[self.current..end])
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or('\u{FFFD}');
        self.current = end;
        self.column += 1;
        cp
    }

    /// If the byte at `current` starts a valid UTF-8 sequence whose codepoint
    /// is identifier-continue-class (Unicode letter, numeric, or `_`), consume
    /// the full sequence (advancing `column` by 1 per codepoint, not per byte)
    /// and return the codepoint. Otherwise leave the cursor untouched and
    /// return `None`.
    fn try_consume_non_ascii_ident_continue(&mut self) -> Option<char> {
        let lead = self.peek();
        if lead < 0x80 {
            return None;
        }
        let len = utf8_byte_len(lead)?;
        if self.current + len > self.source.len() {
            return None;
        }
        let s = std::str::from_utf8(&self.source[self.current..self.current + len]).ok()?;
        let c = s.chars().next()?;
        if c.is_alphabetic() || c.is_numeric() || c == '_' {
            self.current += len;
            self.column += 1;
            Some(c)
        } else {
            None
        }
    }

    // ── Low-level character operations ────────────────────────

    fn advance(&mut self) -> u8 {
        let c = self.source[self.current];
        self.current += 1;
        self.column += 1;
        c
    }

    fn peek(&self) -> u8 {
        if self.is_at_end() {
            b'\0'
        } else {
            self.source[self.current]
        }
    }

    fn peek_next(&self) -> u8 {
        if self.current + 1 >= self.source.len() {
            b'\0'
        } else {
            self.source[self.current + 1]
        }
    }

    fn peek_at(&self, offset: usize) -> u8 {
        let pos = self.current + offset;
        if pos >= self.source.len() {
            b'\0'
        } else {
            self.source[pos]
        }
    }

    fn match_char(&mut self, expected: u8) -> bool {
        if self.is_at_end() || self.source[self.current] != expected {
            false
        } else {
            self.current += 1;
            self.column += 1;
            true
        }
    }

    fn is_at_end(&self) -> bool {
        self.current >= self.source.len()
    }

    fn token_text(&self) -> &str {
        std::str::from_utf8(&self.source[self.start..self.current]).unwrap()
    }

    fn make_spanned(&self, token: Token) -> SpannedToken {
        SpannedToken {
            token,
            span: Span {
                line: self.line,
                column: self.start_column,
                offset: self.start,
                length: self.current - self.start,
            },
        }
    }
}

fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

fn is_hex_digit(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// True if `c` can follow the `e`/`E` in a float exponent:
/// a digit, `+`, or `-`.
fn is_exp_start(c: u8) -> bool {
    is_digit(c) || c == b'+' || c == b'-'
}

/// UTF-8 byte length implied by a lead byte, or `None` for an invalid lead.
/// Used by the non-ASCII recovery path so the lexer can advance by whole
/// codepoints rather than emitting one diagnostic per UTF-8 continuation byte.
fn utf8_byte_len(lead: u8) -> Option<usize> {
    if lead < 0x80 {
        Some(1)
    } else if lead & 0xE0 == 0xC0 {
        Some(2)
    } else if lead & 0xF0 == 0xE0 {
        Some(3)
    } else if lead & 0xF8 == 0xF0 {
        Some(4)
    } else {
        None
    }
}

// ── Identifier case-class ─────────────────────────────────────────

/// The three case classes defined by `design.md § Identifiers and Naming`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentClass {
    /// PascalCase or single uppercase letter. Used for types.
    Type,
    /// ALL_CAPS (all alphabetic chars uppercase). Used for module-level constants.
    Const,
    /// snake_case or `_`-prefixed. Used for functions, params, fields, etc.
    Value,
}

/// Classify an identifier by its case pattern. The rules follow CN-1 through
/// CN-7 in `design.md § Identifiers and Naming`:
///
/// - Leading `_` characters are stripped before classifying (CN-5).
/// - After stripping, if the first alphabetic char is lowercase → Value.
/// - If the first alphabetic char is uppercase AND all alphabetic chars are
///   uppercase → Const.
/// - If the first alphabetic char is uppercase AND at least one subsequent
///   alphabetic char is lowercase, OR the stripped name is exactly one
///   alphabetic character → Type (covers CN-7 single-letter generics).
/// - Pure `_` / `__` / no alphabetic chars → Value.
pub fn classify_ident(name: &str) -> IdentClass {
    let stripped = name.trim_start_matches('_');
    classify_stripped(stripped)
}

fn classify_stripped(name: &str) -> IdentClass {
    let first_alpha = name.chars().find(|c| c.is_ascii_alphabetic());
    match first_alpha {
        None => IdentClass::Value,
        Some(c) if c.is_lowercase() => IdentClass::Value,
        Some(_) => {
            // First alphabetic char is uppercase.
            // Type-class: single letter (CN-7) OR at least one subsequent lowercase (PascalCase).
            // Const-class: all alphabetic chars uppercase (SCREAMING_SNAKE).
            let alpha_chars: Vec<char> = name.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if alpha_chars.len() == 1 || alpha_chars.iter().any(|c| c.is_lowercase()) {
                IdentClass::Type
            } else {
                IdentClass::Const
            }
        }
    }
}

/// Suggest a Type-class rename for `name` (convert to PascalCase).
pub fn suggest_type_name(name: &str) -> String {
    let stripped = name.trim_start_matches('_');
    if stripped.is_empty() {
        return "T".to_string();
    }
    // Split on underscores and capitalize each word.
    // Also split on transitions from lowercase to uppercase (already PascalCase fragments).
    let words = split_words(stripped);
    words
        .into_iter()
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + &cs.as_str().to_lowercase(),
            }
        })
        .collect()
}

/// Suggest a Value-class rename for `name` (convert to snake_case).
pub fn suggest_value_name(name: &str) -> String {
    let stripped = name.trim_start_matches('_');
    if stripped.is_empty() {
        return "_value".to_string();
    }
    // Insert `_` before uppercase letters that follow lowercase ones, then lowercase all.
    let mut result = String::new();
    let mut prev_lower = false;
    for c in stripped.chars() {
        if c.is_uppercase() && prev_lower {
            result.push('_');
        }
        result.push(c.to_lowercase().next().unwrap());
        prev_lower = c.is_lowercase();
    }
    // Collapse sequences of underscores left by all-caps acronyms.
    result
}

/// Suggest a Const-class rename for `name` (convert to SCREAMING_SNAKE_CASE).
pub fn suggest_const_name(name: &str) -> String {
    let stripped = name.trim_start_matches('_');
    if stripped.is_empty() {
        return "CONST_VALUE".to_string();
    }
    let words = split_words(stripped);
    words
        .into_iter()
        .map(|w| w.to_uppercase())
        .collect::<Vec<_>>()
        .join("_")
}

/// Split `name` into words on `_` boundaries and PascalCase transitions.
fn split_words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c == '_' {
            if !current.is_empty() {
                words.push(current.clone());
                current.clear();
            }
            prev_lower = false;
        } else if c.is_uppercase() && prev_lower {
            // Transition lower→upper: start new word.
            if !current.is_empty() {
                words.push(current.clone());
                current.clear();
            }
            current.push(c);
            prev_lower = false;
        } else {
            current.push(c);
            prev_lower = c.is_lowercase();
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    if words.is_empty() {
        words.push(name.to_string());
    }
    words
}
