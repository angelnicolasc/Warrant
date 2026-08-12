//! Reading a proof.
//!
//! The language is small on purpose. It has boolean structure, four
//! primitives and integer comparison, and nothing else — no variables, no
//! arithmetic, no shell. A proof that could compute could also be made to
//! compute the answer it wanted, and the point of compiling to a sealed
//! module is that the module's behaviour is fixed at declaration time.

use crate::ast::{CmpOp, ConstIdx, Expr, Value, ValueType};
use crate::error::{AttestError, Result};

/// A parsed proof: its expression and the constants it refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    /// The expression tree.
    pub expr: Expr,
    /// Commands and path patterns, interned and deduplicated.
    pub constants: Vec<String>,
}

/// Parse a proof.
///
/// ```
/// # use warrant_attest::parse;
/// let parsed = parse(r#"exit(pytest -q) == 0 AND NOT diff_touches("tests/**")"#).unwrap();
/// assert_eq!(parsed.constants, ["pytest -q", "tests/**"]);
/// ```
pub fn parse(source: &str) -> Result<Parsed> {
    let mut parser = Parser { source, pos: 0, constants: Vec::new() };
    let expr = parser.parse_or()?;
    parser.skip_whitespace();
    if parser.pos < source.len() {
        return Err(parser.error("unexpected trailing input"));
    }
    Ok(Parsed { expr, constants: parser.constants })
}

struct Parser<'a> {
    source: &'a str,
    pos: usize,
    constants: Vec<String>,
}

impl<'a> Parser<'a> {
    fn error(&self, message: impl Into<String>) -> AttestError {
        AttestError::parse(message, self.pos, self.source)
    }

    fn rest(&self) -> &'a str {
        &self.source[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, text: &str) -> bool {
        if self.rest().starts_with(text) {
            self.pos += text.len();
            true
        } else {
            false
        }
    }

    /// Match a keyword case-insensitively, but only when it stands alone —
    /// `ANDROID` must not read as `AND`.
    fn eat_keyword(&mut self, keyword: &str) -> bool {
        let rest = self.rest();
        if rest.len() < keyword.len() || !rest[..keyword.len()].eq_ignore_ascii_case(keyword) {
            return false;
        }
        let boundary = rest[keyword.len()..].chars().next();
        if boundary.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return false;
        }
        self.pos += keyword.len();
        true
    }

    fn expect(&mut self, c: char, context: &str) -> Result<()> {
        self.skip_whitespace();
        if self.eat(&c.to_string()) {
            Ok(())
        } else {
            Err(self.error(format!("expected `{c}` {context}")))
        }
    }

    fn intern(&mut self, value: String) -> ConstIdx {
        if let Some(existing) = self.constants.iter().position(|c| *c == value) {
            return existing as ConstIdx;
        }
        self.constants.push(value);
        (self.constants.len() - 1) as ConstIdx
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_whitespace();
            if self.eat_keyword("OR") || self.eat("||") {
                let right = self.parse_and()?;
                left = Expr::Or(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_whitespace();
            if self.eat_keyword("AND") || self.eat("&&") {
                let right = self.parse_unary()?;
                left = Expr::And(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        self.skip_whitespace();
        if self.eat_keyword("NOT") || self.eat("!") {
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        self.skip_whitespace();
        if self.eat("(") {
            let inner = self.parse_or()?;
            self.expect(')', "to close a group")?;
            return Ok(inner);
        }
        self.parse_term()
    }

    fn parse_term(&mut self) -> Result<Expr> {
        self.skip_whitespace();
        let start = self.pos;
        let name = self.read_identifier()?;

        let value = match name.as_str() {
            "exit" => {
                self.expect('(', "after `exit`")?;
                let command = self.read_command()?;
                if command.trim().is_empty() {
                    return Err(self.error("`exit` needs a command to run"));
                }
                Value::ExitCode(self.intern(command))
            }
            "diff_touches" => {
                self.expect('(', "after `diff_touches`")?;
                let pattern = self.read_text_argument()?;
                self.expect(')', "to close `diff_touches`")?;
                Value::DiffTouches(self.intern(pattern))
            }
            "file_exists" => {
                self.expect('(', "after `file_exists`")?;
                let path = self.read_text_argument()?;
                self.expect(')', "to close `file_exists`")?;
                Value::FileExists(self.intern(path))
            }
            "changed_files" => {
                self.expect('(', "after `changed_files`")?;
                self.expect(')', "to close `changed_files`")?;
                Value::ChangedFiles
            }
            other => {
                self.pos = start;
                return Err(self.error(format!(
                    "unknown function `{other}`; a proof may use exit, diff_touches, file_exists or changed_files"
                )));
            }
        };

        self.skip_whitespace();
        match self.read_comparison() {
            Some(op) => {
                if value.value_type() != ValueType::Integer {
                    return Err(AttestError::Type(format!(
                        "`{}` is already a yes-or-no question; it cannot be compared with `{}`",
                        value.function_name(),
                        op.symbol()
                    )));
                }
                self.skip_whitespace();
                let right = self.read_integer()?;
                Ok(Expr::Compare { left: value, op, right })
            }
            None => {
                if value.value_type() != ValueType::Boolean {
                    return Err(AttestError::Type(format!(
                        "`{}` produces a number, so it needs a comparison — for example `{}(…) == 0`",
                        value.function_name(),
                        value.function_name()
                    )));
                }
                Ok(Expr::Truth(value))
            }
        }
    }

    fn read_identifier(&mut self) -> Result<String> {
        let rest = self.rest();
        let len = rest
            .char_indices()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);
        if len == 0 {
            return Err(self.error("expected a proof term"));
        }
        let name = rest[..len].to_string();
        self.pos += len;
        Ok(name)
    }

    /// Read a command written bare inside `exit( … )`, up to the matching
    /// close paren. Parentheses inside quotes do not count, so
    /// `exit(pytest -k "a (b)")` reads as one command.
    fn read_command(&mut self) -> Result<String> {
        let mut depth = 1usize;
        let mut quote: Option<char> = None;
        let start = self.pos;

        while let Some(c) = self.peek() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '"') | (None, '\'') => quote = Some(c),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        let command = self.source[start..self.pos].trim().to_string();
                        self.pos += 1;
                        return Ok(strip_quotes(&command));
                    }
                }
                (None, _) => {}
            }
            self.pos += c.len_utf8();
        }
        Err(self.error("unterminated `exit(` — no closing parenthesis"))
    }

    /// Read a quoted string, or a bare token up to the closing paren.
    fn read_text_argument(&mut self) -> Result<String> {
        self.skip_whitespace();
        let Some(first) = self.peek() else {
            return Err(self.error("expected a path pattern"));
        };

        if first == '"' || first == '\'' {
            self.pos += first.len_utf8();
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c == first {
                    let text = self.source[start..self.pos].to_string();
                    self.pos += c.len_utf8();
                    return Ok(text);
                }
                self.pos += c.len_utf8();
            }
            return Err(self.error("unterminated string"));
        }

        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == ')' {
                break;
            }
            self.pos += c.len_utf8();
        }
        let text = self.source[start..self.pos].trim().to_string();
        if text.is_empty() {
            return Err(self.error("expected a path pattern"));
        }
        Ok(text)
    }

    fn read_comparison(&mut self) -> Option<CmpOp> {
        // Longest match first, so `<=` is never read as `<`.
        for (text, op) in [
            ("==", CmpOp::Eq),
            ("!=", CmpOp::Ne),
            ("<=", CmpOp::Le),
            (">=", CmpOp::Ge),
            ("=", CmpOp::Eq),
            ("<", CmpOp::Lt),
            (">", CmpOp::Gt),
        ] {
            if self.eat(text) {
                return Some(op);
            }
        }
        None
    }

    fn read_integer(&mut self) -> Result<i32> {
        let rest = self.rest();
        let negative = rest.starts_with('-');
        let digits_at = usize::from(negative);
        let len = rest[digits_at..].chars().take_while(char::is_ascii_digit).count();
        if len == 0 {
            return Err(self.error("expected a number to compare against"));
        }
        let text = &rest[..digits_at + len];
        let value: i32 = text.parse().map_err(|_| self.error("number does not fit in 32 bits"))?;
        self.pos += text.len();
        Ok(value)
    }
}

fn strip_quotes(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        return text[1..text.len() - 1].to_string();
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(source: &str) -> String {
        let parsed = parse(source).unwrap();
        parsed.expr.render(&parsed.constants)
    }

    #[test]
    fn the_readme_example_parses() {
        let parsed = parse(
            r#"exit(pytest tests/auth -k expired) == 0
               AND diff_touches("src/auth/**")
               AND NOT diff_touches("tests/**")"#,
        )
        .unwrap();
        assert_eq!(parsed.constants, ["pytest tests/auth -k expired", "src/auth/**", "tests/**"]);
        assert_eq!(parsed.expr.commands(&parsed.constants), ["pytest tests/auth -k expired"]);
    }

    #[test]
    fn and_binds_tighter_than_or() {
        assert_eq!(
            rendered("file_exists(a) OR file_exists(b) AND file_exists(c)"),
            r#"(file_exists("a") OR (file_exists("b") AND file_exists("c")))"#
        );
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(
            rendered("(file_exists(a) OR file_exists(b)) AND file_exists(c)"),
            r#"((file_exists("a") OR file_exists("b")) AND file_exists("c"))"#
        );
    }

    #[test]
    fn symbolic_operators_are_accepted() {
        assert_eq!(
            rendered("file_exists(a) && !file_exists(b)"),
            r#"(file_exists("a") AND NOT file_exists("b"))"#
        );
    }

    #[test]
    fn keywords_are_case_insensitive_but_not_prefixes() {
        assert!(parse("file_exists(a) and file_exists(b)").is_ok());
        assert!(parse("file_exists(a) Or file_exists(b)").is_ok());
        // `android` must not be read as `and` followed by a stray term.
        assert!(parse("file_exists(a) android file_exists(b)").is_err());
    }

    #[test]
    fn a_command_may_contain_parentheses_inside_quotes() {
        let parsed = parse(r#"exit(pytest -k "test_a (fast)") == 0"#).unwrap();
        assert_eq!(parsed.constants, [r#"pytest -k "test_a (fast)""#]);
    }

    #[test]
    fn a_command_may_be_written_quoted_or_bare() {
        assert_eq!(parse(r#"exit("pytest -q") == 0"#).unwrap().constants, ["pytest -q"]);
        assert_eq!(parse("exit(pytest -q) == 0").unwrap().constants, ["pytest -q"]);
    }

    #[test]
    fn every_comparison_operator_is_recognised() {
        for (text, symbol) in
            [("==", "=="), ("!=", "!="), ("<", "<"), ("<=", "<="), (">", ">"), (">=", ">=")]
        {
            let source = format!("changed_files() {text} 3");
            assert!(rendered(&source).contains(symbol), "failed on {text}");
        }
    }

    #[test]
    fn constants_are_deduplicated() {
        let parsed = parse(r#"diff_touches("src/**") OR diff_touches("src/**")"#).unwrap();
        assert_eq!(parsed.constants.len(), 1);
    }

    #[test]
    fn an_exit_code_without_a_comparison_is_rejected_with_a_useful_message() {
        let error = parse("exit(pytest)").unwrap_err().to_string();
        assert!(error.contains("needs a comparison"), "got: {error}");
    }

    #[test]
    fn comparing_a_yes_or_no_question_is_rejected() {
        let error = parse(r#"diff_touches("a") == 1"#).unwrap_err().to_string();
        assert!(error.contains("yes-or-no"), "got: {error}");
    }

    #[test]
    fn unknown_functions_name_what_is_available() {
        let error = parse("tests_pass()").unwrap_err().to_string();
        assert!(error.contains("diff_touches"), "got: {error}");
    }

    #[test]
    fn malformed_proofs_are_rejected_rather_than_half_read() {
        for source in [
            "",
            "exit(",
            "exit(pytest) ==",
            "exit(pytest) == abc",
            r#"diff_touches("unterminated"#,
            "file_exists(a) AND",
            "(file_exists(a)",
            "file_exists(a) trailing",
        ] {
            assert!(parse(source).is_err(), "should have been rejected: {source:?}");
        }
    }

    #[test]
    fn negative_comparands_are_allowed() {
        assert!(parse("exit(cmd) != -1").is_ok());
    }

    #[test]
    fn a_parse_error_points_at_the_problem() {
        let error = parse("file_exists(a) AND bogus()").unwrap_err().to_string();
        assert!(error.contains('^'), "the message should carry a caret: {error}");
    }
}
