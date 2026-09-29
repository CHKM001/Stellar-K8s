// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Wasmtime compilation front end for Soroban contract bytecode.
//!
//! Compilation is deliberately separated from caching: this module turns
//! bytecode into a [`CompiledModule`] and knows nothing about eviction. The
//! [`crate::wasm::cache`] module decides which compilations stay resident.

use std::{fmt, sync::Arc};

use wasmtime::{Config, Engine, Instance, Module, Store};

/// Magic number every WebAssembly binary starts with (`\0asm`).
const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6d];
/// Version 1 header bytes that follow the magic number.
const WASM_VERSION: [u8; 4] = [0x01, 0x00, 0x00, 0x00];

/// Errors produced while validating or compiling contract bytecode.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    /// The supplied bytecode is empty.
    #[error("wasm bytecode is empty")]
    Empty,
    /// The bytecode is not a version 1 WebAssembly binary.
    #[error("wasm bytecode is malformed: {0}")]
    Malformed(&'static str),
    /// Wasmtime rejected the bytecode while validating or compiling it.
    #[error("wasm compilation failed: {0}")]
    Engine(#[from] wasmtime::Error),
}

/// A JIT-compiled contract module bound to one `(contract_id, ledger)` pair.
///
/// The artifact holds compiled code only — never live instance state — so it is
/// safe to share across threads and requests. [`CompiledModule::instantiate`]
/// hands out a brand-new `Store` on every call, which is what keeps the memory
/// spaces of separate simulations from ever overlapping.
pub struct CompiledModule {
    contract_id: String,
    ledger_sequence: u64,
    wasm_size: usize,
    engine: Engine,
    module: Module,
}

impl CompiledModule {
    /// Contract the bytecode belongs to.
    pub fn contract_id(&self) -> &str {
        &self.contract_id
    }

    /// Ledger sequence this compilation is valid for.
    pub fn ledger_sequence(&self) -> u64 {
        self.ledger_sequence
    }

    /// Size in bytes of the source bytecode that produced this artifact.
    pub fn wasm_size(&self) -> usize {
        self.wasm_size
    }

    /// The underlying Wasmtime module, for callers that manage their own store.
    pub fn module(&self) -> &Module {
        &self.module
    }

    /// Instantiate the module with a fresh store.
    ///
    /// Each call allocates an isolated `Store`, so linear memories, tables, and
    /// globals of one instance are unreachable from every other instance —
    /// including instances created from this same artifact.
    pub fn instantiate(&self) -> Result<(Store<()>, Instance), wasmtime::Error> {
        let mut store = Store::new(&self.engine, ());
        let instance = Instance::new(&mut store, &self.module, &[])?;
        Ok((store, instance))
    }
}

impl fmt::Debug for CompiledModule {
    /// Describe the cache identity only.
    ///
    /// The engine and module are omitted: dumping JIT internals on a cache
    /// `expect`/`unwrap` failure would be unreadable and would couple the
    /// output to Wasmtime's private layout.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledModule")
            .field("contract_id", &self.contract_id)
            .field("ledger_sequence", &self.ledger_sequence)
            .field("wasm_size", &self.wasm_size)
            .finish_non_exhaustive()
    }
}

/// Compiles contract bytecode with a shared Wasmtime engine.
///
/// The engine is created once and reused for every compilation, so all
/// artifacts produced by one compiler share a single code cache and can be
/// instantiated interchangeably.
pub struct WasmCompiler {
    engine: Engine,
}

impl Default for WasmCompiler {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmCompiler {
    /// Create a compiler using Wasmtime's default (Cranelift) configuration.
    pub fn new() -> Self {
        Self::with_config(Config::new()).expect("default wasmtime engine configuration is valid")
    }

    /// Create a compiler from an explicit Wasmtime configuration.
    pub fn with_config(config: Config) -> Result<Self, wasmtime::Error> {
        Ok(Self {
            engine: Engine::new(&config)?,
        })
    }

    /// The engine shared by every artifact this compiler produces.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Validate the WebAssembly header and compile the bytecode.
    ///
    /// The cheap header check runs first so non-Wasm payloads are rejected
    /// before they reach validation, which keeps malformed requests from
    /// paying full compilation cost.
    pub fn compile(&self, wasm: &[u8]) -> Result<Module, CompileError> {
        validate_header(wasm)?;
        Module::new(&self.engine, wasm).map_err(CompileError::Engine)
    }

    /// Compile `wasm` and tag the artifact with its cache identity.
    ///
    /// The contract id and ledger sequence become part of the artifact so the
    /// cache can verify that what it stores always matches the key it is
    /// stored under.
    pub fn compile_contract(
        &self,
        contract_id: impl Into<String>,
        ledger_sequence: u64,
        wasm: &[u8],
    ) -> Result<Arc<CompiledModule>, CompileError> {
        let module = self.compile(wasm)?;
        Ok(Arc::new(CompiledModule {
            contract_id: contract_id.into(),
            ledger_sequence,
            wasm_size: wasm.len(),
            engine: self.engine.clone(),
            module,
        }))
    }
}

/// Check the 8-byte WebAssembly header without running the validator.
fn validate_header(wasm: &[u8]) -> Result<(), CompileError> {
    if wasm.is_empty() {
        return Err(CompileError::Empty);
    }
    if wasm.len() < WASM_MAGIC.len() + WASM_VERSION.len() {
        return Err(CompileError::Malformed(
            "bytecode shorter than the 8-byte wasm header",
        ));
    }
    if wasm[..4] != WASM_MAGIC {
        return Err(CompileError::Malformed("missing \\0asm magic number"));
    }
    if wasm[4..8] != WASM_VERSION {
        return Err(CompileError::Malformed("unsupported wasm version"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal module whose exported memory is the isolation surface.
    const MEMORY_WAT: &str = r#"(module
        (memory (export "memory") 1)
        (func (export "poke") (param $slot i32) (i32.store (local.get $slot) (i32.const 7)))
        (func (export "peek") (param $slot i32) (result i32) (i32.load (local.get $slot)))
    )"#;

    fn bytes(wat: &str) -> Vec<u8> {
        wat::parse_str(wat).expect("valid wat")
    }

    #[test]
    fn compiles_valid_bytecode() {
        let compiler = WasmCompiler::new();
        let module = compiler.compile(&bytes(MEMORY_WAT)).expect("compiles");
        assert!(module.exports().count() > 0);
    }

    #[test]
    fn accepts_header_only_empty_module() {
        let compiler = WasmCompiler::new();
        let empty = wat::parse_str("(module)").expect("valid wat");
        assert!(compiler.compile(&empty).is_ok());
    }

    #[test]
    fn rejects_empty_bytecode() {
        let compiler = WasmCompiler::new();
        assert!(matches!(compiler.compile(&[]), Err(CompileError::Empty)));
    }

    #[test]
    fn rejects_non_wasm_bytecode() {
        let compiler = WasmCompiler::new();
        let error = compiler.compile(b"not wasm at all").expect_err("must fail");
        assert!(matches!(error, CompileError::Malformed(_)));
    }

    #[test]
    fn rejects_truncated_header() {
        let compiler = WasmCompiler::new();
        let error = compiler.compile(&WASM_MAGIC).expect_err("must fail");
        assert!(matches!(error, CompileError::Malformed(_)));
    }

    #[test]
    fn rejects_unsupported_version() {
        let compiler = WasmCompiler::new();
        let mut bytecode = WASM_MAGIC.to_vec();
        bytecode.extend_from_slice(&[0x02, 0x00, 0x00, 0x00]);
        let error = compiler.compile(&bytecode).expect_err("must fail");
        assert!(matches!(error, CompileError::Malformed(_)));
    }

    #[test]
    fn compiled_artifact_reports_cache_identity() {
        let compiler = WasmCompiler::new();
        let bytecode = bytes(MEMORY_WAT);
        let artifact = compiler
            .compile_contract("CCONTRACT", 99, &bytecode)
            .expect("compiles");
        assert_eq!(artifact.contract_id(), "CCONTRACT");
        assert_eq!(artifact.ledger_sequence(), 99);
        assert_eq!(artifact.wasm_size(), bytecode.len());
    }

    #[test]
    fn instances_never_share_linear_memory() {
        let compiler = WasmCompiler::new();
        let artifact = compiler
            .compile_contract("CCONTRACT", 1, &bytes(MEMORY_WAT))
            .expect("compiles");

        let (mut first_store, first) = artifact.instantiate().expect("first instance");
        let first_memory = first
            .get_memory(&mut first_store, "memory")
            .expect("memory exported");
        first_memory
            .write(&mut first_store, 0, &[42u8])
            .expect("write");

        // A second simulation must start from clean memory even though it uses
        // the same compiled artifact.
        let (mut second_store, second) = artifact.instantiate().expect("second instance");
        let second_memory = second
            .get_memory(&mut second_store, "memory")
            .expect("memory exported");
        let mut probe = [0u8; 1];
        second_memory
            .read(&mut second_store, 0, &mut probe)
            .expect("read");
        assert_eq!(
            probe[0], 0,
            "second instance must not see first instance state"
        );

        // ...and the first instance keeps its own state.
        let mut original = [0u8; 1];
        first_memory
            .read(&mut first_store, 0, &mut original)
            .expect("read");
        assert_eq!(original[0], 42);
    }

    #[test]
    fn isolation_holds_for_the_shared_contract_fixture() {
        let compiler = WasmCompiler::new();
        let bytecode = bytes(include_str!("../../testdata/complex-contract.wat"));
        let artifact = compiler
            .compile_contract("CFIXTURE", 7, &bytecode)
            .expect("fixture compiles");

        let (mut left_store, left) = artifact.instantiate().expect("left");
        let settle = left
            .get_typed_func::<(i32, i32), ()>(&mut left_store, "settle")
            .expect("settle export");
        settle.call(&mut left_store, (0, 4242)).expect("settle");

        let (mut right_store, right) = artifact.instantiate().expect("right");
        let read = right
            .get_typed_func::<i32, i32>(&mut right_store, "read")
            .expect("read export");
        let value = read.call(&mut right_store, 0).expect("read");
        assert_eq!(value, 0, "contract state must not leak between instances");
    }
}
