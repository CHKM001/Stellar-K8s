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

//! 10,000-iteration `simulateTransaction` benchmark for the WASM cache.
//!
//! Runs the same complex contract simulation twice:
//!
//! 1. **Cold** — every call compiles the contract from bytecode, which is what
//!    an uncached RPC gateway pays per request.
//! 2. **Warm** — the gateway keeps the artifact in the LRU cache, so only the
//!    first call compiles and the remaining 9,999 are cache hits.
//!
//! The benchmark fails (non-zero exit) unless the warm path is at least 80%
//! faster than the cold path, which is the acceptance threshold for this
//! feature. Run it with `cargo bench -p stellar-rpc-gateway`.

use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use stellar_rpc_gateway::wasm::{
    CacheConfig, CacheKey, CacheStats, CompiledModule, WasmCache, WasmCompiler,
};

/// Sequential simulations per phase, matching the acceptance criterion.
const ITERATIONS: usize = 10_000;
/// Required latency reduction once the cache is warm.
const MINIMUM_REDUCTION: f64 = 0.80;
/// Contract the simulation runs against.
const CONTRACT_ID: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
/// Ledger the simulation is pinned to.
const LEDGER_SEQUENCE: u64 = 5_000_000;
/// Trade parameters for one simulated swap route.
const TRADE: (i32, i32, i32, i32) = (1_000_000, 50_000_000, 30_000_000, 3);

fn main() {
    let wasm = wat::parse_str(include_str!("../testdata/complex-contract.wat"))
        .expect("fixture is valid wat");
    let compiler = WasmCompiler::new();
    let key = CacheKey::new(CONTRACT_ID, LEDGER_SEQUENCE);

    let cold = run_cold(&compiler, &key, &wasm);
    let (warm, stats) = run_warm(&compiler, &key, &wasm);
    let reduction = 1.0 - (warm.as_secs_f64() / cold.as_secs_f64());
    print_report(cold, warm, reduction, stats);

    assert!(
        stats.hits == (ITERATIONS - 1) as u64 && stats.misses == 1,
        "warm phase must compile exactly once: {stats:?}"
    );
    assert!(
        reduction >= MINIMUM_REDUCTION,
        "cache must cut simulation latency by at least {:.0}%, got {:.1}%",
        MINIMUM_REDUCTION * 100.0,
        reduction * 100.0
    );
}

/// Re-compile and simulate `ITERATIONS` times with no cache in the way.
fn run_cold(compiler: &WasmCompiler, key: &CacheKey, wasm: &[u8]) -> Duration {
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let module = compiler
            .compile_contract(key.contract_id(), key.ledger_sequence(), wasm)
            .expect("contract compiles");
        black_box(simulate(&module));
    }
    start.elapsed()
}

/// Simulate `ITERATIONS` times through the cache: one miss, then hits.
fn run_warm(compiler: &WasmCompiler, key: &CacheKey, wasm: &[u8]) -> (Duration, CacheStats) {
    let cache = Arc::new(WasmCache::new(CacheConfig::default()));
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let module = cache
            .get_or_compile(key, || {
                compiler.compile_contract(key.contract_id(), key.ledger_sequence(), wasm)
            })
            .expect("contract compiles");
        black_box(simulate(&module));
    }
    (start.elapsed(), cache.stats())
}

/// One isolated simulation: fresh store, fresh instance, one contract call.
fn simulate(module: &CompiledModule) -> i64 {
    let (mut store, instance) = module.instantiate().expect("fresh isolated instance");
    let entrypoint = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "simulate")
        .expect("simulate export");
    let value = entrypoint
        .call(&mut store, black_box(TRADE))
        .expect("simulation runs");
    i64::from(value)
}

fn print_report(cold: Duration, warm: Duration, reduction: f64, stats: CacheStats) {
    let cold_avg = cold / ITERATIONS as u32;
    let warm_avg = warm / ITERATIONS as u32;
    println!("simulateTransaction x{ITERATIONS} (complex contract fixture)");
    println!("  cold (compile per call): {cold:?} total, {cold_avg:?} avg");
    println!("  warm (cached modules):   {warm:?} total, {warm_avg:?} avg");
    println!(
        "  latency reduction:       {:.1}% (minimum {:.0}%)",
        reduction * 100.0,
        MINIMUM_REDUCTION * 100.0
    );
    println!(
        "  cache: hits={} misses={} entries={} stored_bytes={} hit_rate={:.4}",
        stats.hits,
        stats.misses,
        stats.entries,
        stats.stored_bytes,
        stats.hit_rate()
    );
}
