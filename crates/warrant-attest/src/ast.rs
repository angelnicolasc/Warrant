//! What a proof says, before it is sealed.
//!
//! The AST exists for about a millisecond: a proof is parsed, type-checked,
//! compiled to WebAssembly and hashed, and from then on it is bytes. Nothing
//! downstream reasons about this type, which is deliberate — the thing that
//! gets run must be the thing that was hashed.

use serde::{Deserialize, Serialize};

/// Index into a proof's constant table.
pub type ConstIdx = u32;

/// A quantity a proof can talk about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    /// Exit status of a command run against the candidate tree.
    ExitCode(ConstIdx),
    /// Whether the candidate diff touches a path pattern.
    DiffTouches(ConstIdx),
    /// Whether a path exists in the candidate tree.
    FileExists(ConstIdx),
    /// How many files the candidate diff changes.
    ChangedFiles,
}

/// Whether a value is a number or already a truth value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// Compares against an integer.
    Integer,
    /// Usable directly as a condition.
    Boolean,
}

impl Value {
    /// The type this value produces.
    pub fn value_type(&self) -> ValueType {
        match self {
            Value::ExitCode(_) | Value::ChangedFiles => ValueType::Integer,
            Value::DiffTouches(_) | Value::FileExists(_) => ValueType::Boolean,
        }
    }

    /// The name it is written with.
    pub fn function_name(&self) -> &'static str {
        match self {
            Value::ExitCode(_) => "exit",
            Value::DiffTouches(_) => "diff_touches",
            Value::FileExists(_) => "file_exists",
            Value::ChangedFiles => "changed_files",
        }
    }
}

/// A comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

impl CmpOp {
    /// How it is written.
    pub fn symbol(&self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// A proof expression.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Expr {
    /// Short-circuiting conjunction.
    And(Box<Expr>, Box<Expr>),
    /// Short-circuiting disjunction.
    Or(Box<Expr>, Box<Expr>),
    /// Negation.
    Not(Box<Expr>),
    /// An integer-valued term compared against a literal.
    Compare {
        /// Left-hand term.
        left: Value,
        /// The comparison.
        op: CmpOp,
        /// Right-hand literal.
        right: i32,
    },
    /// A boolean-valued term used directly.
    Truth(Value),
}

impl Expr {
    /// Render the expression back to source, for receipts and error messages.
    ///
    /// Round-tripping matters: a receipt shows a human what was actually
    /// checked, and it must not be a paraphrase.
    pub fn render(&self, constants: &[String]) -> String {
        match self {
            Expr::And(a, b) => format!("({} AND {})", a.render(constants), b.render(constants)),
            Expr::Or(a, b) => format!("({} OR {})", a.render(constants), b.render(constants)),
            Expr::Not(e) => format!("NOT {}", e.render(constants)),
            Expr::Compare { left, op, right } => {
                format!("{} {} {right}", render_value(*left, constants), op.symbol())
            }
            Expr::Truth(value) => render_value(*value, constants),
        }
    }

    /// Every command this proof may run, in the order they appear.
    pub fn commands(&self, constants: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(&mut |expr| {
            if let Expr::Compare { left: Value::ExitCode(idx), .. }
            | Expr::Truth(Value::ExitCode(idx)) = expr
                && let Some(cmd) = constants.get(*idx as usize)
            {
                out.push(cmd.clone());
            }
        });
        out
    }

    fn walk(&self, visit: &mut impl FnMut(&Expr)) {
        visit(self);
        match self {
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.walk(visit);
                b.walk(visit);
            }
            Expr::Not(e) => e.walk(visit),
            Expr::Compare { .. } | Expr::Truth(_) => {}
        }
    }
}

fn render_value(value: Value, constants: &[String]) -> String {
    let arg = |idx: ConstIdx| constants.get(idx as usize).cloned().unwrap_or_default();
    match value {
        Value::ExitCode(idx) => format!("exit({})", arg(idx)),
        Value::DiffTouches(idx) => format!("diff_touches({:?})", arg(idx)),
        Value::FileExists(idx) => format!("file_exists({:?})", arg(idx)),
        Value::ChangedFiles => "changed_files()".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_names_every_operand() {
        let constants = vec!["pytest -q".to_string(), "tests/**".to_string()];
        let expr = Expr::And(
            Box::new(Expr::Compare { left: Value::ExitCode(0), op: CmpOp::Eq, right: 0 }),
            Box::new(Expr::Not(Box::new(Expr::Truth(Value::DiffTouches(1))))),
        );
        assert_eq!(
            expr.render(&constants),
            r#"(exit(pytest -q) == 0 AND NOT diff_touches("tests/**"))"#
        );
    }

    #[test]
    fn commands_are_collected_in_source_order() {
        let constants = vec!["first".to_string(), "second".to_string()];
        let expr = Expr::And(
            Box::new(Expr::Compare { left: Value::ExitCode(0), op: CmpOp::Eq, right: 0 }),
            Box::new(Expr::Compare { left: Value::ExitCode(1), op: CmpOp::Eq, right: 0 }),
        );
        assert_eq!(expr.commands(&constants), ["first", "second"]);
    }

    #[test]
    fn value_types_separate_numbers_from_conditions() {
        assert_eq!(Value::ExitCode(0).value_type(), ValueType::Integer);
        assert_eq!(Value::ChangedFiles.value_type(), ValueType::Integer);
        assert_eq!(Value::DiffTouches(0).value_type(), ValueType::Boolean);
        assert_eq!(Value::FileExists(0).value_type(), ValueType::Boolean);
    }
}
