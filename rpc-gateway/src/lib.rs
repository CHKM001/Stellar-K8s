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

#![deny(missing_docs)]

//! WebAssembly module caching layer for the Soroban RPC gateway.
//!
//! Recompiling contract bytecode on every `simulateTransaction` call dominates
//! RPC latency. This crate keeps JIT-compiled Wasmtime modules warm in an
//! in-memory LRU cache so that repeated simulations of the same contract at the
//! same ledger are served without touching the compiler.
//!
//! # Design
//!
//! - [`wasm::compiler`] validates and compiles bytecode, and wraps each
//!   compilation in a [`wasm::CompiledModule`] tagged with the contract id and
//!   ledger sequence it belongs to.
//! - [`wasm::cache`] stores those artifacts under a [`wasm::CacheKey`] of
//!   `(contract_id, ledger_sequence)` so any state update naturally retires the
//!   previous compilation.
//! - Entries are bounded three ways: a fixed LRU capacity, a total-byte budget,
//!   and a background collector that evicts entries trailing the newest
//!   observed ledger by more than a configurable lag.
//!
//! # Isolation
//!
//! Cached artifacts are compiled code only. Every simulation instantiates a
//! fresh `Store`, so the linear memories and globals of separate contract
//! instances never overlap, and the cache refuses to store or return an
//! artifact whose contract id or ledger sequence does not match the requested
//! key.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use stellar_rpc_gateway::wasm::{CacheConfig, CacheKey, WasmCache, WasmCompiler};
//!
//! let compiler = WasmCompiler::new();
//! let cache = Arc::new(WasmCache::new(CacheConfig::default()));
//! let key = CacheKey::new("CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC", 42);
//! let wasm = wat::parse_str(
//!     r#"(module (func (export "add") (param i32 i32) (result i32)
//!            (i32.add (local.get 0) (local.get 1))))"#,
//! )
//! .expect("valid wat");
//!
//! let module = cache
//!     .get_or_compile(&key, || {
//!         compiler.compile_contract(key.contract_id(), key.ledger_sequence(), &wasm)
//!     })
//!     .expect("compilation succeeds");
//!
//! assert_eq!(module.contract_id(), key.contract_id());
//! let stats = cache.stats();
//! assert_eq!(stats.misses, 1);
//! assert_eq!(stats.entries, 1);
//! ```

pub mod wasm;

pub use wasm::{
    cache::{CacheConfig, CacheError, CacheKey, CacheStats, CollectorHandle, WasmCache},
    compiler::{CompileError, CompiledModule, WasmCompiler},
};
