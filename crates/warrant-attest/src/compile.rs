//! Sealing a proof into WebAssembly.
//!
//! The emitted module is deliberately tiny: no memory, no globals, no tables,
//! no loops. It is boolean control flow over four host calls, whose arguments
//! are indices into a constant table carried in a custom section. That shape
//! buys three things at once — the module is deterministic, its bytes are a
//! complete description of what it checks, and there is nothing inside it
//! capable of computing a different answer than the one it was declared with.
//!
//! Conjunction and disjunction short-circuit. That is not an optimisation
//! detail: the left operand of an `AND` is frequently a test suite, and a
//! proof that ran every command regardless of the first result would multiply
//! the cost of the necessity search by the number of clauses.

use wasm_encoder::{
    BlockType, CodeSection, CustomSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, ImportSection, Instruction, Module, TypeSection, ValType,
};
use wasmparser::{Parser as WasmParser, Payload};

use crate::ast::{CmpOp, Expr, Value};
use crate::error::{AttestError, Result};
use crate::parse::Parsed;

/// Import module name every host function is drawn from.
pub const HOST_MODULE: &str = "warrant";

/// Custom section holding the JSON constant table.
pub const CONSTANTS_SECTION: &str = "warrant.constants";

/// Custom section holding the proof as it was written.
pub const SOURCE_SECTION: &str = "warrant.source";

/// The exported entry point.
pub const ENTRY_POINT: &str = "evaluate";

/// Host function indices, fixed by the order of the import section.
pub(crate) const FN_EXIT_CODE: u32 = 0;
pub(crate) const FN_DIFF_TOUCHES: u32 = 1;
pub(crate) const FN_FILE_EXISTS: u32 = 2;
pub(crate) const FN_CHANGED_FILES: u32 = 3;

/// Compile a parsed proof to a self-contained module.
pub fn compile(parsed: &Parsed, source: &str) -> Result<Vec<u8>> {
    let mut types = TypeSection::new();
    // 0: (i32) -> i32, for the host functions that take a constant index.
    types.ty().function([ValType::I32], [ValType::I32]);
    // 1: () -> i32, for `changed_files` and for `evaluate` itself.
    types.ty().function([], [ValType::I32]);

    let mut imports = ImportSection::new();
    imports.import(HOST_MODULE, "exit_code", EntityType::Function(0));
    imports.import(HOST_MODULE, "diff_touches", EntityType::Function(0));
    imports.import(HOST_MODULE, "file_exists", EntityType::Function(0));
    imports.import(HOST_MODULE, "changed_files", EntityType::Function(1));

    let mut functions = FunctionSection::new();
    functions.function(1);

    let entry_index = 4; // four imports precede the one defined function
    let mut exports = ExportSection::new();
    exports.export(ENTRY_POINT, ExportKind::Func, entry_index);

    let mut body = Function::new([]);
    emit(&mut body, &parsed.expr);
    body.instruction(&Instruction::End);
    let mut code = CodeSection::new();
    code.function(&body);

    let constants = serde_json::to_vec(&parsed.constants)
        .map_err(|e| AttestError::Compile(format!("encoding the constant table: {e}")))?;

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&exports);
    module.section(&code);
    module.section(&CustomSection { name: CONSTANTS_SECTION.into(), data: constants.into() });
    module.section(&CustomSection { name: SOURCE_SECTION.into(), data: source.as_bytes().into() });

    Ok(module.finish())
}

fn emit(function: &mut Function, expr: &Expr) {
    match expr {
        Expr::Truth(value) => {
            emit_value(function, value);
            // Normalise to 0 or 1 rather than trusting the host to have done
            // it. The rest of the emitted code assumes booleans are exactly
            // 0 or 1, and that assumption should not span a trust boundary.
            function.instruction(&Instruction::I32Const(0));
            function.instruction(&Instruction::I32Ne);
        }
        Expr::Compare { left, op, right } => {
            emit_value(function, left);
            function.instruction(&Instruction::I32Const(*right));
            function.instruction(&match op {
                CmpOp::Eq => Instruction::I32Eq,
                CmpOp::Ne => Instruction::I32Ne,
                CmpOp::Lt => Instruction::I32LtS,
                CmpOp::Le => Instruction::I32LeS,
                CmpOp::Gt => Instruction::I32GtS,
                CmpOp::Ge => Instruction::I32GeS,
            });
        }
        Expr::Not(inner) => {
            emit(function, inner);
            function.instruction(&Instruction::I32Eqz);
        }
        Expr::And(left, right) => {
            emit(function, left);
            function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
            emit(function, right);
            function.instruction(&Instruction::Else);
            function.instruction(&Instruction::I32Const(0));
            function.instruction(&Instruction::End);
        }
        Expr::Or(left, right) => {
            emit(function, left);
            function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::Else);
            emit(function, right);
            function.instruction(&Instruction::End);
        }
    }
}

fn emit_value(function: &mut Function, value: &Value) {
    match value {
        Value::ExitCode(idx) => {
            function.instruction(&Instruction::I32Const(*idx as i32));
            function.instruction(&Instruction::Call(FN_EXIT_CODE));
        }
        Value::DiffTouches(idx) => {
            function.instruction(&Instruction::I32Const(*idx as i32));
            function.instruction(&Instruction::Call(FN_DIFF_TOUCHES));
        }
        Value::FileExists(idx) => {
            function.instruction(&Instruction::I32Const(*idx as i32));
            function.instruction(&Instruction::Call(FN_FILE_EXISTS));
        }
        Value::ChangedFiles => {
            function.instruction(&Instruction::Call(FN_CHANGED_FILES));
        }
    }
}

/// Read the constant table back out of a sealed module.
///
/// This is what makes a proof third-party verifiable: given only the bytes
/// recorded in the ledger, anyone can recover exactly which commands and
/// patterns it checks, without trusting the record's description of it.
pub fn read_constants(wasm: &[u8]) -> Result<Vec<String>> {
    match read_custom_section(wasm, CONSTANTS_SECTION)? {
        Some(data) => serde_json::from_slice(&data)
            .map_err(|e| AttestError::NotAProof(format!("its constant table is malformed: {e}"))),
        None => Err(AttestError::NotAProof("it has no constant table".into())),
    }
}

/// Read the proof text back out of a sealed module.
pub fn read_source(wasm: &[u8]) -> Result<String> {
    match read_custom_section(wasm, SOURCE_SECTION)? {
        Some(data) => String::from_utf8(data)
            .map_err(|_| AttestError::NotAProof("its source section is not text".into())),
        None => Err(AttestError::NotAProof("it has no source section".into())),
    }
}

fn read_custom_section(wasm: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    for payload in WasmParser::new(0).parse_all(wasm) {
        let payload = payload
            .map_err(|e| AttestError::NotAProof(format!("it is not valid WebAssembly: {e}")))?;
        if let Payload::CustomSection(reader) = payload
            && reader.name() == name
        {
            return Ok(Some(reader.data().to_vec()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn sealed(source: &str) -> Vec<u8> {
        let parsed = parse(source).unwrap();
        compile(&parsed, source).unwrap()
    }

    #[test]
    fn the_module_starts_with_the_wasm_magic_number() {
        let wasm = sealed("exit(pytest) == 0");
        assert_eq!(&wasm[..4], b"\0asm");
        assert_eq!(&wasm[4..8], &[1, 0, 0, 0], "binary format version 1");
    }

    #[test]
    fn the_constant_table_survives_the_round_trip() {
        let wasm = sealed(r#"exit(pytest -q) == 0 AND diff_touches("src/**")"#);
        assert_eq!(read_constants(&wasm).unwrap(), ["pytest -q", "src/**"]);
    }

    #[test]
    fn the_source_survives_the_round_trip() {
        let source = r#"exit(cargo test) == 0 AND NOT diff_touches("tests/**")"#;
        let wasm = sealed(source);
        assert_eq!(read_source(&wasm).unwrap(), source);
    }

    #[test]
    fn compilation_is_deterministic() {
        let source = r#"exit(pytest) == 0 AND diff_touches("a/**")"#;
        assert_eq!(sealed(source), sealed(source), "identical proofs must seal identically");
    }

    #[test]
    fn different_proofs_seal_to_different_modules() {
        assert_ne!(sealed("exit(a) == 0"), sealed("exit(b) == 0"));
        assert_ne!(sealed("exit(a) == 0"), sealed("exit(a) == 1"));
        assert_ne!(sealed("exit(a) == 0"), sealed("exit(a) != 0"));
    }

    #[test]
    fn a_module_that_is_not_a_proof_is_rejected_clearly() {
        assert!(matches!(read_constants(b"not wasm at all"), Err(AttestError::NotAProof(_))));
        // Valid WebAssembly, but without Warrant's sections.
        let empty = Module::new().finish();
        assert!(matches!(read_constants(&empty), Err(AttestError::NotAProof(_))));
    }

    #[test]
    fn the_module_is_small_enough_to_store_with_every_claim() {
        let wasm = sealed(
            r#"exit(pytest tests/auth -k expired) == 0
               AND diff_touches("src/auth/**")
               AND NOT diff_touches("tests/**")"#,
        );
        assert!(wasm.len() < 1024, "a sealed proof was {} bytes", wasm.len());
    }
}
