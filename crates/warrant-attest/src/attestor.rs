//! Running a sealed proof.
//!
//! A fresh store per evaluation, no WASI, no network, no filesystem, no
//! ledger handle, no clock. The module's entire universe is the four host
//! functions in [`crate::env`], and its entire output is one bit.

use std::sync::Arc;

use warrant_core::{ReceiptRef, Verdict};
use wasmtime::{Caller, Config, Engine, Linker, Module, Store};

use crate::compile::{ENTRY_POINT, HOST_MODULE};
use crate::env::ProbeEnvironment;
use crate::error::{AttestError, Result};
use crate::predicate::Predicate;

/// Instructions a proof may execute before it is cut off.
///
/// Generous: a sealed proof has no loops, so a well-formed one uses a few
/// hundred. The budget exists to bound a module that was not produced by this
/// compiler.
const DEFAULT_FUEL: u64 = 1_000_000;

/// How many commands a single evaluation may run.
const DEFAULT_MAX_COMMANDS: u32 = 64;

/// The sealed proof runner.
pub struct Attestor {
    engine: Engine,
    fuel: u64,
    max_commands: u32,
}

/// Host state for one evaluation.
///
/// Owned rather than borrowed: the runtime requires store data to be
/// `'static`, which is why [`ProbeEnvironment`] is shared through an `Arc`
/// and takes `&self`.
struct HostState {
    env: Arc<dyn ProbeEnvironment>,
    constants: Vec<String>,
    failure: Option<AttestError>,
    commands_run: u32,
    max_commands: u32,
}

impl HostState {
    /// Look up a constant, recording a failure if the index is out of range.
    ///
    /// An out-of-range index means the module was not produced by this
    /// compiler. It is refused rather than defaulted, because a proof that
    /// silently reads an empty string is a proof that silently passes.
    fn constant(&mut self, function: &'static str, idx: i32) -> Option<String> {
        match usize::try_from(idx).ok().and_then(|i| self.constants.get(i)) {
            Some(value) => Some(value.clone()),
            None => {
                self.fail(AttestError::Environment {
                    function,
                    reason: format!("constant {idx} is not in the proof's table"),
                });
                None
            }
        }
    }

    fn fail(&mut self, error: AttestError) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }

    fn halted(&self) -> bool {
        self.failure.is_some()
    }
}

impl Attestor {
    /// Build an attestor.
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config)
            .map_err(|e| AttestError::Runtime(format!("configuring the proof runtime: {e}")))?;
        Ok(Attestor { engine, fuel: DEFAULT_FUEL, max_commands: DEFAULT_MAX_COMMANDS })
    }

    /// Cap how many commands one evaluation may run.
    pub fn with_max_commands(mut self, max: u32) -> Self {
        self.max_commands = max;
        self
    }

    /// Cap how much a proof may execute.
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    /// Evaluate a proof against an environment.
    ///
    /// Returns the bit, or an error. Those are genuinely different outcomes:
    /// a proof that could not be evaluated has not said "no", and reporting
    /// it as `false` would turn an infrastructure failure into a finding.
    pub fn evaluate(&self, predicate: &Predicate, env: Arc<dyn ProbeEnvironment>) -> Result<bool> {
        let module = Module::new(&self.engine, predicate.wasm())
            .map_err(|e| AttestError::NotAProof(format!("it did not load: {e}")))?;

        let state = HostState {
            env,
            constants: predicate.constants().to_vec(),
            failure: None,
            commands_run: 0,
            max_commands: self.max_commands,
        };
        let mut store = Store::new(&self.engine, state);
        store
            .set_fuel(self.fuel)
            .map_err(|e| AttestError::Runtime(format!("setting the proof budget: {e}")))?;

        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        register_host_functions(&mut linker)?;

        let instance = linker.instantiate(&mut store, &module).map_err(|e| {
            AttestError::NotAProof(format!("it does not match the proof interface: {e}"))
        })?;
        let entry = instance.get_typed_func::<(), i32>(&mut store, ENTRY_POINT).map_err(|e| {
            AttestError::NotAProof(format!("it has no `{ENTRY_POINT}` export: {e}"))
        })?;

        let outcome = entry.call(&mut store, ());

        // A host-side failure is the more informative diagnosis, so it wins
        // over whatever the runtime reported.
        if let Some(failure) = store.into_data().failure {
            return Err(failure);
        }

        match outcome {
            Ok(bit) => Ok(bit != 0),
            Err(e) if e.to_string().contains("fuel") => Err(AttestError::BudgetExhausted),
            Err(e) => Err(AttestError::Runtime(e.to_string())),
        }
    }

    /// Evaluate and package the result as a verdict.
    ///
    /// `receipt` is the address the caller has already written the evidence
    /// to. The verdict carries that address and nothing else — no score, no
    /// coverage, no explanation.
    pub fn discharge(
        &self,
        predicate: &Predicate,
        env: Arc<dyn ProbeEnvironment>,
        receipt: ReceiptRef,
    ) -> Result<Verdict> {
        Ok(if self.evaluate(predicate, env)? {
            Verdict::Warranted { receipt }
        } else {
            Verdict::Unproven
        })
    }
}

fn register_host_functions(linker: &mut Linker<HostState>) -> Result<()> {
    let wire =
        |e: wasmtime::Error| AttestError::Runtime(format!("registering host functions: {e}"));

    linker
        .func_wrap(HOST_MODULE, "exit_code", |mut caller: Caller<'_, HostState>, idx: i32| {
            let state = caller.data_mut();
            if state.halted() {
                return 0;
            }
            if state.commands_run >= state.max_commands {
                let max = state.max_commands;
                state.fail(AttestError::Environment {
                    function: "exit",
                    reason: format!("a proof may run at most {max} commands"),
                });
                return 0;
            }
            let Some(command) = state.constant("exit", idx) else {
                return 0;
            };
            state.commands_run += 1;
            let env = Arc::clone(&state.env);
            match env.exit_code(&command) {
                Ok(code) => code,
                Err(e) => {
                    state.fail(e);
                    0
                }
            }
        })
        .map_err(wire)?;

    linker
        .func_wrap(HOST_MODULE, "diff_touches", |mut caller: Caller<'_, HostState>, idx: i32| {
            let state = caller.data_mut();
            if state.halted() {
                return 0;
            }
            let Some(pattern) = state.constant("diff_touches", idx) else {
                return 0;
            };
            let env = Arc::clone(&state.env);
            match env.diff_touches(&pattern) {
                Ok(hit) => i32::from(hit),
                Err(e) => {
                    state.fail(e);
                    0
                }
            }
        })
        .map_err(wire)?;

    linker
        .func_wrap(HOST_MODULE, "file_exists", |mut caller: Caller<'_, HostState>, idx: i32| {
            let state = caller.data_mut();
            if state.halted() {
                return 0;
            }
            let Some(path) = state.constant("file_exists", idx) else {
                return 0;
            };
            let env = Arc::clone(&state.env);
            match env.file_exists(&path) {
                Ok(hit) => i32::from(hit),
                Err(e) => {
                    state.fail(e);
                    0
                }
            }
        })
        .map_err(wire)?;

    linker
        .func_wrap(HOST_MODULE, "changed_files", |mut caller: Caller<'_, HostState>| {
            let state = caller.data_mut();
            if state.halted() {
                return 0;
            }
            let env = Arc::clone(&state.env);
            match env.changed_files() {
                Ok(count) => count,
                Err(e) => {
                    state.fail(e);
                    0
                }
            }
        })
        .map_err(wire)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ScriptedEnvironment;

    fn run(source: &str, env: Arc<ScriptedEnvironment>) -> bool {
        let proof = Predicate::compile(source).unwrap();
        Attestor::new().unwrap().evaluate(&proof, env).unwrap()
    }

    #[test]
    fn a_passing_command_discharges_the_proof() {
        let env = Arc::new(ScriptedEnvironment::new().with_exit("pytest", 0));
        assert!(run("exit(pytest) == 0", env));
    }

    #[test]
    fn a_failing_command_does_not() {
        let env = Arc::new(ScriptedEnvironment::new().with_exit("pytest", 1));
        assert!(!run("exit(pytest) == 0", env));
    }

    #[test]
    fn every_comparison_evaluates_correctly() {
        let cases = [
            ("exit(c) == 3", true),
            ("exit(c) != 3", false),
            ("exit(c) < 4", true),
            ("exit(c) < 3", false),
            ("exit(c) <= 3", true),
            ("exit(c) > 2", true),
            ("exit(c) > 3", false),
            ("exit(c) >= 3", true),
        ];
        for (source, expected) in cases {
            let env = Arc::new(ScriptedEnvironment::new().with_exit("c", 3));
            assert_eq!(run(source, env), expected, "failed on {source}");
        }
    }

    #[test]
    fn boolean_structure_evaluates_correctly() {
        let truth_table = [
            (true, true, true, true),
            (true, false, false, true),
            (false, true, false, true),
            (false, false, false, false),
        ];
        for (a, b, expect_and, expect_or) in truth_table {
            let mut present = Vec::new();
            if a {
                present.push("a");
            }
            if b {
                present.push("b");
            }
            let env = Arc::new(ScriptedEnvironment::new().with_files(present.clone()));
            assert_eq!(run("file_exists(a) AND file_exists(b)", env), expect_and, "AND on {a},{b}");

            let env = Arc::new(ScriptedEnvironment::new().with_files(present));
            assert_eq!(run("file_exists(a) OR file_exists(b)", env), expect_or, "OR on {a},{b}");
        }
    }

    #[test]
    fn negation_evaluates_correctly() {
        let env = Arc::new(ScriptedEnvironment::new().with_changed(["tests/test_x.py"]));
        assert!(!run(r#"NOT diff_touches("tests/**")"#, env));

        let env = Arc::new(ScriptedEnvironment::new().with_changed(["src/x.py"]));
        assert!(run(r#"NOT diff_touches("tests/**")"#, env));
    }

    /// The clause that makes the README's example interesting: a claim that
    /// forbids itself from depending on edits to the tests judging it.
    #[test]
    fn a_proof_can_forbid_itself_from_leaning_on_test_edits() {
        let source =
            r#"exit(pytest) == 0 AND diff_touches("src/**") AND NOT diff_touches("tests/**")"#;

        let honest = Arc::new(
            ScriptedEnvironment::new().with_exit("pytest", 0).with_changed(["src/api/upload.py"]),
        );
        assert!(run(source, honest));

        let laundered = Arc::new(
            ScriptedEnvironment::new()
                .with_exit("pytest", 0)
                .with_changed(["src/api/upload.py", "tests/test_upload.py"]),
        );
        assert!(
            !run(source, laundered),
            "a green suite bought with a test edit must not discharge the claim"
        );
    }

    #[test]
    fn conjunction_short_circuits_before_running_the_suite() {
        let env =
            Arc::new(ScriptedEnvironment::new().with_changed(["src/x.py"]).with_exit("pytest", 0));
        assert!(!run(r#"diff_touches("nothing/**") AND exit(pytest) == 0"#, Arc::clone(&env)));
        assert_eq!(env.calls(), ["diff_touches(nothing/**)"], "the suite must never have run");
    }

    #[test]
    fn disjunction_short_circuits_too() {
        let env =
            Arc::new(ScriptedEnvironment::new().with_changed(["src/x.py"]).with_exit("pytest", 0));
        assert!(run(r#"diff_touches("src/**") OR exit(pytest) == 0"#, Arc::clone(&env)));
        assert_eq!(env.calls(), ["diff_touches(src/**)"]);
    }

    #[test]
    fn changed_file_counts_are_readable() {
        for (source, expected) in [
            ("changed_files() == 3", true),
            ("changed_files() > 0", true),
            ("changed_files() > 3", false),
        ] {
            let env = Arc::new(ScriptedEnvironment::new().with_changed(["a", "b", "c"]));
            assert_eq!(run(source, env), expected, "failed on {source}");
        }
    }

    #[test]
    fn a_verdict_is_produced_without_a_score() {
        let proof = Predicate::compile("exit(pytest) == 0").unwrap();
        let receipt = ReceiptRef::derive(&[b"evidence"]);
        let attestor = Attestor::new().unwrap();

        let passing = Arc::new(ScriptedEnvironment::new().with_exit("pytest", 0));
        assert_eq!(
            attestor.discharge(&proof, passing, receipt).unwrap(),
            Verdict::Warranted { receipt }
        );

        let failing = Arc::new(ScriptedEnvironment::new().with_exit("pytest", 9));
        assert_eq!(attestor.discharge(&proof, failing, receipt).unwrap(), Verdict::Unproven);
    }

    #[test]
    fn the_command_cap_is_enforced() {
        let proof = Predicate::compile("exit(a) == 0 AND exit(b) == 0 AND exit(c) == 0").unwrap();
        let attestor = Attestor::new().unwrap().with_max_commands(2);
        let env = Arc::new(ScriptedEnvironment::new().with_default_exit(0));

        let error = attestor.evaluate(&proof, env).unwrap_err();
        assert!(error.to_string().contains("at most 2 commands"), "got: {error}");
    }

    /// A module that is not one of ours must be refused, not run.
    #[test]
    fn an_arbitrary_module_cannot_masquerade_as_a_proof() {
        let bogus = wasm_encoder::Module::new().finish();
        assert!(Predicate::from_wasm(bogus).is_err());
    }

    #[test]
    fn an_environment_failure_is_an_error_not_a_false_verdict() {
        let proof = Predicate::compile(r#"diff_touches("[")"#).unwrap();
        let env = Arc::new(ScriptedEnvironment::new().with_changed(["a.py"]));
        assert!(matches!(
            Attestor::new().unwrap().evaluate(&proof, env),
            Err(AttestError::BadPattern { .. })
        ));
    }
}
