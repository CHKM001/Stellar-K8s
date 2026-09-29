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

//! Latency verification for the WASM cache.
//!
//! Replays `ITERATIONS` sequential `simulateTransaction` simulations against
//! the complex contract fixture — first with no cache (compile on every call),
//! then with the cache warm — and asserts the cached path is at least 80%
//! faster, the acceptance threshold for this feature.
//!
//! The authoritative 10,000-iteration release-mode run lives in
//! `cargo bench -p stellar-rpc-gateway`; this test keeps the same assertion
//! green on every `cargo test`.

use std::{sync::Arc, time::Instant};

use stellar_rpc_gateway::wasm::{CacheConfig, CacheKey, CompiledModule, WasmCache, WasmCompiler};

/// Sequential simulations replayed in each phase.
const ITERATIONS: usize = 10_000;
/// Required latency reduction once the cache is warm.
const MINIMUM_REDUCTION: f64 = 0.80;
/// Contract the simulation runs against.
const CONTRACT_ID: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
/// Ledger the simulation is pinned to.
const LEDGER_SEQUENCE: u64 = 5_000_000;
/// Trade parameters for one simulated swap route.
const TRADE: (i32, i32, i32, i32) = (1_000_000, 50_000_000, 30_000_000, 3);

#[test]
fn warm_cache_cuts_simulation_latency_by_at_least_80_percent() {
    let wasm = wat::parse_str(include_str!("../testdata/complex-contract.wat"))
        .expect("fixture is valid wat");
    let compiler = WasmCompiler::new();
    let key = CacheKey::new(CONTRACT_ID, LEDGER_SEQUENCE);

    let cold = {
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let module = compiler
                .compile_contract(key.contract_id(), key.ledger_sequence(), &wasm)
                .expect("contract compiles");
            std::hint::black_box(simulate(&module));
        }
        start.elapsed()
    };

    let cache = Arc::new(WasmCache::new(CacheConfig::default()));
    let warm = {
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let module = cache
                .get_or_compile(&key, || {
                    compiler.compile_contract(key.contract_id(), key.ledger_sequence(), &wasm)
                })
                .expect("contract compiles");
            std::hint::black_box(simulate(&module));
        }
        start.elapsed()
    };

    let reduction = 1.0 - (warm.as_secs_f64() / cold.as_secs_f64());
    let stats = cache.stats();
    println!(
        "cold={:?} warm={:?} reduction={:.1}% hits={} misses={}",
        cold,
        warm,
        reduction * 100.0,
        stats.hits,
        stats.misses
    );

    assert_eq!(stats.misses, 1, "the cache must compile exactly once");
    assert_eq!(
        stats.hits,
        (ITERATIONS - 1) as u64,
        "every later simulation must be served from the cache"
    );
    assert!(
        reduction >= MINIMUM_REDUCTION,
        "expected at least {:.0}% latency reduction after the initial cache miss, got {:.1}%",
        MINIMUM_REDUCTION * 100.0,
        reduction * 100.0
    );
}

#[test]
fn cached_artifacts_stay_isolated_across_concurrent_simulations() {
    let wasm = Arc::new(
        wat::parse_str(include_str!("../testdata/complex-contract.wat"))
            .expect("fixture is valid wat"),
    );
    let compiler = Arc::new(WasmCompiler::new());
    let cache = Arc::new(WasmCache::new(CacheConfig::default().with_capacity(4)));

    let workers: Vec<_> = (0..4u64)
        .map(|worker| {
            let cache = Arc::clone(&cache);
            let compiler = Arc::clone(&compiler);
            let wasm = Arc::clone(&wasm);
            std::thread::spawn(move || {
                let key = CacheKey::new(format!("CCONTRACT{worker}"), worker);
                for _ in 0..25 {
                    let module = cache
                        .get_or_compile(&key, || {
                            compiler.compile_contract(
                                key.contract_id(),
                                key.ledger_sequence(),
                                &wasm,
                            )
                        })
                        .expect("contract compiles");
                    assert_eq!(module.contract_id(), key.contract_id());
                    std::hint::black_box(simulate(&module));
                }
            })
        })
        .collect();

    for worker in workers {
        worker.join().expect("worker thread completes");
    }

    let stats = cache.stats();
    assert_eq!(stats.entries, 4, "each contract holds its own artifact");
    assert!(
        stats.entries <= stats.capacity,
        "the cache never exceeds its configured capacity"
    );
    for worker in 0..4u64 {
        let served = cache
            .get(&CacheKey::new(format!("CCONTRACT{worker}"), worker))
            .expect("artifact is cached");
        assert_eq!(served.contract_id(), format!("CCONTRACT{worker}"));
        assert_eq!(served.ledger_sequence(), worker);
    }
}

/// One isolated simulation: fresh store, fresh instance, one contract call.
fn simulate(module: &CompiledModule) -> i64 {
    let (mut store, instance) = module.instantiate().expect("fresh isolated instance");
    let entrypoint = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "simulate")
        .expect("simulate export");
    let value = entrypoint.call(&mut store, TRADE).expect("simulation runs");
    i64::from(value)
}
