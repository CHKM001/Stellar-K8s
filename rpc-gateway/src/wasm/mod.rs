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

//! WebAssembly compilation and caching primitives for the RPC gateway.
//!
//! Two halves make up the layer:
//!
//! - [`compiler`]: validates contract bytecode and JIT-compiles it once per
//!   `(contract_id, ledger_sequence)` binding.
//! - [`cache`]: an in-memory LRU that keeps compiled artifacts warm, bounds
//!   memory with a byte budget, and retires stale bindings in the background.

pub mod cache;
pub mod compiler;

pub use cache::{CacheConfig, CacheError, CacheKey, CacheStats, CollectorHandle, WasmCache};
pub use compiler::{CompileError, CompiledModule, WasmCompiler};
