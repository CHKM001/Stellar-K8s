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

//! In-memory LRU cache for compiled contract modules.
//!
//! Lookups are keyed by [`CacheKey`], which pairs a contract id with the ledger
//! sequence the compilation was built for. Advancing the ledger therefore
//! invalidates previous compilations implicitly: a request for a newer ledger
//! misses and recompiles, while requests for the same ledger stay warm.
//!
//! Memory is bounded three ways so sustained API load cannot leak:
//!
//! 1. **LRU capacity** — at most [`CacheConfig::capacity`] artifacts resident.
//! 2. **Byte budget** — the summed source size of resident artifacts never
//!    exceeds [`CacheConfig::max_stored_bytes`].
//! 3. **Stale sweep** — a background collector ([`WasmCache::spawn_collector`])
//!    evicts entries trailing the newest observed ledger by more than
//!    [`CacheConfig::max_ledger_lag`].

use std::{
    fmt, io,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, Sender},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use lru::LruCache;

use super::compiler::{CompileError, CompiledModule};

/// Smallest permitted background sweep interval, guarding against a hot loop.
const MIN_SWEEP_INTERVAL: Duration = Duration::from_millis(1);

/// Identity of one cached compilation.
///
/// The ledger sequence is part of the key on purpose: when contract state moves
/// forward, simulations address a new key and the previous compilation stops
/// being served.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    contract_id: String,
    ledger_sequence: u64,
}

impl CacheKey {
    /// Build a key for `contract_id` as observed at `ledger_sequence`.
    pub fn new(contract_id: impl Into<String>, ledger_sequence: u64) -> Self {
        Self {
            contract_id: contract_id.into(),
            ledger_sequence,
        }
    }

    /// Contract the key addresses.
    pub fn contract_id(&self) -> &str {
        &self.contract_id
    }

    /// Ledger sequence the compilation is bound to.
    pub fn ledger_sequence(&self) -> u64 {
        self.ledger_sequence
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.contract_id, self.ledger_sequence)
    }
}

/// Tuning knobs for [`WasmCache`].
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Maximum number of compiled modules kept resident.
    pub capacity: usize,
    /// Upper bound on the summed source bytecode size of resident modules.
    pub max_stored_bytes: usize,
    /// How far behind the newest observed ledger an entry may lag before the
    /// collector evicts it.
    pub max_ledger_lag: u64,
    /// How often the background collector sweeps for stale entries.
    pub sweep_interval: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            capacity: 128,
            max_stored_bytes: 64 * 1024 * 1024,
            max_ledger_lag: 64,
            sweep_interval: Duration::from_secs(1),
        }
    }
}

impl CacheConfig {
    /// Keep at most `capacity` compiled modules resident.
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Bound the summed source size of resident modules to `max_stored_bytes`.
    pub fn with_max_stored_bytes(mut self, max_stored_bytes: usize) -> Self {
        self.max_stored_bytes = max_stored_bytes;
        self
    }

    /// Evict entries trailing the newest ledger by more than `max_ledger_lag`.
    pub fn with_max_ledger_lag(mut self, max_ledger_lag: u64) -> Self {
        self.max_ledger_lag = max_ledger_lag;
        self
    }

    /// Sweep for stale entries every `sweep_interval`.
    pub fn with_sweep_interval(mut self, sweep_interval: Duration) -> Self {
        self.sweep_interval = sweep_interval;
        self
    }
}

/// Errors returned by cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The miss-path compilation failed; nothing was stored.
    #[error(transparent)]
    Compile(#[from] CompileError),
    /// The artifact does not belong to the key it was filed under.
    ///
    /// This is the isolation boundary: a module built for one contract or
    /// ledger is never stored — and therefore never served — under another.
    #[error(
        "isolation violation: artifact for {found_contract} (ledger {found_ledger}) \
         does not match key {expected_contract} (ledger {expected_ledger})"
    )]
    IsolationViolation {
        /// Contract the key addresses.
        expected_contract: String,
        /// Ledger the key addresses.
        expected_ledger: u64,
        /// Contract the artifact was compiled for.
        found_contract: String,
        /// Ledger the artifact was compiled for.
        found_ledger: u64,
    },
}

/// Point-in-time snapshot of cache counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups served from the cache.
    pub hits: u64,
    /// Lookups that had to compile.
    pub misses: u64,
    /// Currently resident entries.
    pub entries: usize,
    /// Configured entry capacity.
    pub capacity: usize,
    /// Summed source bytecode size of resident entries.
    pub stored_bytes: usize,
    /// Entries dropped to stay within capacity.
    pub lru_evictions: u64,
    /// Entries dropped by the stale sweep.
    pub stale_evictions: u64,
    /// Entries dropped to stay within the byte budget.
    pub budget_evictions: u64,
    /// Newest ledger sequence observed by [`WasmCache::observe_ledger`].
    pub latest_ledger: u64,
}

impl CacheStats {
    /// Fraction of lookups served from the cache, or `0.0` with no lookups.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits.saturating_add(self.misses);
        if total == 0 {
            return 0.0;
        }
        self.hits as f64 / total as f64
    }
}

/// Shared storage for [`WasmCache`]; private so invariants stay in one place.
type ModuleLru = LruCache<CacheKey, Arc<CompiledModule>>;

/// In-memory LRU of compiled contract modules.
///
/// All operations are safe to call concurrently from request handlers.
pub struct WasmCache {
    modules: Mutex<ModuleLru>,
    config: CacheConfig,
    latest_ledger: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    lru_evictions: AtomicU64,
    stale_evictions: AtomicU64,
    budget_evictions: AtomicU64,
    stored_bytes: AtomicUsize,
}

impl WasmCache {
    /// Create a cache bounded by `config`.
    ///
    /// A `capacity` of zero is clamped to one so the cache always has a usable
    /// slot; use [`CacheConfig::max_stored_bytes`] of zero to hold nothing.
    pub fn new(config: CacheConfig) -> Self {
        let capacity = NonZeroUsize::new(config.capacity.max(1)).expect("capacity is at least 1");
        Self {
            modules: Mutex::new(LruCache::new(capacity)),
            config,
            latest_ledger: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            lru_evictions: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            budget_evictions: AtomicU64::new(0),
            stored_bytes: AtomicUsize::new(0),
        }
    }

    /// The configuration this cache was built with.
    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    /// Number of resident entries.
    pub fn len(&self) -> usize {
        self.lock_modules().len()
    }

    /// Whether the cache currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up a compiled module, recording a hit or miss.
    ///
    /// The artifact returned is re-checked against the key before it is
    /// handed out; a mismatch is treated as a miss and the entry is dropped so
    /// a stale binding can never cross a contract boundary.
    pub fn get(&self, key: &CacheKey) -> Option<Arc<CompiledModule>> {
        let mut modules = self.lock_modules();
        let found = modules.get(key).cloned();
        match found {
            Some(module) if matches_key(key, &module) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(module)
            }
            Some(_) => {
                // Defensive: never serve an artifact filed under the wrong key.
                modules.pop(key);
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Store a compiled module under `key`.
    ///
    /// Returns [`CacheError::IsolationViolation`] — storing nothing — when the
    /// artifact was compiled for a different contract or ledger than the key
    /// addresses.
    pub fn insert(&self, key: CacheKey, module: Arc<CompiledModule>) -> Result<(), CacheError> {
        if !matches_key(&key, &module) {
            return Err(CacheError::IsolationViolation {
                expected_contract: key.contract_id().to_string(),
                expected_ledger: key.ledger_sequence(),
                found_contract: module.contract_id().to_string(),
                found_ledger: module.ledger_sequence(),
            });
        }

        let size = module.wasm_size();
        let mut modules = self.lock_modules();

        // A full cache without this key evicts its least-recently-used entry
        // on insert; account for that victim before it disappears.
        if modules.len() >= modules.cap().get() && modules.peek(&key).is_none() {
            if let Some((_, victim)) = modules.peek_lru() {
                self.release_bytes(victim.wasm_size());
                self.lru_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }

        if let Some(replaced) = modules.put(key, module) {
            self.release_bytes(replaced.wasm_size());
        }
        self.stored_bytes.fetch_add(size, Ordering::Relaxed);
        self.enforce_byte_budget(&mut modules);
        Ok(())
    }

    /// Return the cached compilation for `key`, compiling it on a miss.
    ///
    /// This is the hot path for `simulateTransaction`: the first call for a
    /// `(contract, ledger)` pair compiles, every later call is a lookup plus a
    /// fresh isolated instance.
    pub fn get_or_compile<F>(
        &self,
        key: &CacheKey,
        compile: F,
    ) -> Result<Arc<CompiledModule>, CacheError>
    where
        F: FnOnce() -> Result<Arc<CompiledModule>, CompileError>,
    {
        if let Some(module) = self.get(key) {
            return Ok(module);
        }
        let module = compile()?;
        self.insert(key.clone(), Arc::clone(&module))?;
        Ok(module)
    }

    /// Record the newest ledger sequence seen by the gateway.
    ///
    /// The background collector uses this watermark to decide which entries
    /// have fallen out of date. Observations never move backwards.
    pub fn observe_ledger(&self, ledger_sequence: u64) {
        self.latest_ledger
            .fetch_max(ledger_sequence, Ordering::Relaxed);
    }

    /// Newest ledger sequence recorded via [`WasmCache::observe_ledger`].
    pub fn latest_ledger(&self) -> u64 {
        self.latest_ledger.load(Ordering::Relaxed)
    }

    /// Evict entries trailing the newest observed ledger by more than
    /// [`CacheConfig::max_ledger_lag`], returning how many were removed.
    ///
    /// Until a ledger has been observed the cache has no watermark, so the
    /// sweep is a no-op rather than a guess.
    pub fn sweep(&self) -> usize {
        let latest = self.latest_ledger.load(Ordering::Relaxed);
        if latest == 0 {
            return 0;
        }
        let max_lag = self.config.max_ledger_lag;
        let mut modules = self.lock_modules();
        let stale: Vec<CacheKey> = modules
            .iter()
            .filter(|(key, _)| latest.saturating_sub(key.ledger_sequence()) > max_lag)
            .map(|(key, _)| key.clone())
            .collect();

        for key in &stale {
            if let Some(evicted) = modules.pop(key) {
                self.release_bytes(evicted.wasm_size());
                self.stale_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        stale.len()
    }

    /// Start a background thread that sweeps stale entries every
    /// [`CacheConfig::sweep_interval`].
    ///
    /// The returned handle stops the thread — and waits for it to exit — when
    /// [`CollectorHandle::stop`] is called or the handle is dropped, so no
    /// worker outlives its cache.
    pub fn spawn_collector(self: Arc<Self>) -> io::Result<CollectorHandle> {
        let interval = self.config.sweep_interval.max(MIN_SWEEP_INTERVAL);
        let (tx, rx) = mpsc::channel::<()>();
        let cache = self;
        let worker = thread::Builder::new()
            .name("wasm-cache-collector".to_string())
            .spawn(move || loop {
                match rx.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        cache.sweep();
                    }
                }
            })?;

        Ok(CollectorHandle {
            sender: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Remove every entry and reset the byte accounting.
    ///
    /// Cumulative counters (hits, misses, evictions) are kept so monitoring
    /// windows stay continuous.
    pub fn clear(&self) {
        let mut modules = self.lock_modules();
        modules.clear();
        self.stored_bytes.store(0, Ordering::Relaxed);
    }

    /// Snapshot the cache counters.
    pub fn stats(&self) -> CacheStats {
        let modules = self.lock_modules();
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: modules.len(),
            capacity: modules.cap().get(),
            stored_bytes: self.stored_bytes.load(Ordering::Relaxed),
            lru_evictions: self.lru_evictions.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            budget_evictions: self.budget_evictions.load(Ordering::Relaxed),
            latest_ledger: self.latest_ledger.load(Ordering::Relaxed),
        }
    }

    /// Take the LRU lock, recovering from a poisoned lock rather than
    /// propagating a panic into request handlers.
    fn lock_modules(&self) -> MutexGuard<'_, ModuleLru> {
        self.modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drop LRU entries until the byte budget is satisfied.
    fn enforce_byte_budget(&self, modules: &mut ModuleLru) {
        while self.stored_bytes.load(Ordering::Relaxed) > self.config.max_stored_bytes {
            let Some((_, evicted)) = modules.pop_lru() else {
                break;
            };
            self.release_bytes(evicted.wasm_size());
            self.budget_evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Subtract `bytes` from the resident total without underflowing.
    fn release_bytes(&self, bytes: usize) {
        let _ = self
            .stored_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_sub(bytes))
            });
    }
}

/// Whether `module` was compiled for exactly `key`'s contract and ledger.
fn matches_key(key: &CacheKey, module: &CompiledModule) -> bool {
    module.contract_id() == key.contract_id() && module.ledger_sequence() == key.ledger_sequence()
}

/// Handle for a [`WasmCache`] background collector thread.
///
/// Stopping is idempotent; dropping the handle stops the collector so tests
/// and shutdown paths cannot leak the worker.
pub struct CollectorHandle {
    sender: Mutex<Option<Sender<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl CollectorHandle {
    /// Signal the collector to stop and wait for its thread to exit.
    pub fn stop(&self) {
        let sender = self.sender.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        let worker = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }

    /// Whether the collector thread is still alive.
    pub fn is_running(&self) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }
}

impl Drop for CollectorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::compiler::WasmCompiler;

    /// Header-only module: a valid compilation with near-zero compile cost,
    /// which keeps stress tests fast while exercising real compiler artifacts.
    fn empty_module() -> Vec<u8> {
        wat::parse_str("(module)").expect("valid wat")
    }

    /// Compile a module tagged for exactly `key`.
    fn compiled(key: &CacheKey) -> Result<Arc<CompiledModule>, CompileError> {
        static COMPILER: std::sync::OnceLock<WasmCompiler> = std::sync::OnceLock::new();
        let compiler = COMPILER.get_or_init(WasmCompiler::new);
        compiler.compile_contract(key.contract_id(), key.ledger_sequence(), &empty_module())
    }

    fn key(contract: &str, ledger: u64) -> CacheKey {
        CacheKey::new(contract, ledger)
    }

    #[test]
    fn miss_then_hit_reuses_the_same_artifact() {
        let cache = WasmCache::new(CacheConfig::default().with_capacity(4));
        let key = key("CONE", 10);

        let first = cache
            .get_or_compile(&key, || compiled(&key))
            .expect("compiles");
        let second = cache
            .get_or_compile(&key, || panic!("cache must serve the warm entry"))
            .expect("hit");

        assert!(Arc::ptr_eq(&first, &second));
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.entries, 1);
        assert!((stats.hit_rate() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn miss_path_propagates_compilation_failure() {
        let cache = WasmCache::new(CacheConfig::default());
        let key = key("CBROKEN", 1);
        let error = cache
            .get_or_compile(&key, || Err(CompileError::Empty))
            .expect_err("compilation failure surfaces");
        assert!(matches!(error, CacheError::Compile(CompileError::Empty)));
        assert!(cache.is_empty());
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn ledger_sequence_isolates_entries() {
        let cache = WasmCache::new(CacheConfig::default().with_capacity(8));
        let old = key("CCONTRACT", 100);
        let new = key("CCONTRACT", 101);

        let old_module = cache.get_or_compile(&old, || compiled(&old)).expect("old");
        let new_module = cache.get_or_compile(&new, || compiled(&new)).expect("new");

        assert!(!Arc::ptr_eq(&old_module, &new_module));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().misses, 2);
        // Both bindings stay independently addressable.
        assert!(cache.get(&old).is_some());
        assert!(cache.get(&new).is_some());
    }

    #[test]
    fn contract_ids_never_share_entries() {
        let cache = WasmCache::new(CacheConfig::default().with_capacity(8));
        let left = key("CLEFT", 5);
        let right = key("CRIGHT", 5);

        let left_module = cache
            .get_or_compile(&left, || compiled(&left))
            .expect("left");
        let right_module = cache
            .get_or_compile(&right, || compiled(&right))
            .expect("right");

        assert!(!Arc::ptr_eq(&left_module, &right_module));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(
            cache.get(&left).expect("left still cached").contract_id(),
            "CLEFT"
        );
        assert_eq!(
            cache.get(&right).expect("right still cached").contract_id(),
            "CRIGHT"
        );
    }

    #[test]
    fn insert_rejects_artifacts_built_for_another_key() {
        let cache = WasmCache::new(CacheConfig::default());
        let artifact_for = key("COWNER", 1);
        let wrong_key = key("COTHER", 1);

        let error = cache
            .insert(wrong_key, compiled(&artifact_for).expect("compiles"))
            .expect_err("mismatched artifact must be rejected");

        assert!(matches!(error, CacheError::IsolationViolation { .. }));
        assert!(cache.is_empty(), "rejected artifacts must not be stored");
    }

    #[test]
    fn lru_evicts_least_recently_used_entry_at_capacity() {
        let cache = WasmCache::new(CacheConfig::default().with_capacity(2));
        let first = key("CFIRST", 1);
        let second = key("CSECOND", 1);
        let third = key("CTHIRD", 1);

        cache
            .get_or_compile(&first, || compiled(&first))
            .expect("first");
        cache
            .get_or_compile(&second, || compiled(&second))
            .expect("second");
        // Promote `first` so `second` becomes least-recently used.
        cache.get(&first).expect("promotes first");
        cache
            .get_or_compile(&third, || compiled(&third))
            .expect("third");

        let stats = cache.stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.lru_evictions, 1);
        assert!(cache.get(&first).is_some(), "recently used entry survives");
        assert!(cache.get(&second).is_none(), "stale entry was evicted");
        assert!(cache.get(&third).is_some());
    }

    #[test]
    fn byte_budget_evicts_lru_entries() {
        let module_size = empty_module().len();
        assert!(module_size > 0);
        let cache = WasmCache::new(
            CacheConfig::default()
                .with_capacity(64)
                .with_max_stored_bytes(module_size * 4),
        );

        for ledger in 0..50u64 {
            let key = key("CBUDGET", ledger);
            cache
                .get_or_compile(&key, || compiled(&key))
                .expect("insert");
        }

        let stats = cache.stats();
        assert!(
            stats.stored_bytes <= module_size * 4,
            "byte budget must hold: {} > {}",
            stats.stored_bytes,
            module_size * 4
        );
        assert_eq!(stats.entries, 4);
        assert!(stats.budget_evictions >= 46);
    }

    #[test]
    fn sweep_evicts_only_entries_behind_the_watermark() {
        let cache = WasmCache::new(
            CacheConfig::default()
                .with_capacity(16)
                .with_max_ledger_lag(10),
        );
        let stale = key("CCONTRACT", 100);
        let fresh = key("CCONTRACT", 1_000);
        cache
            .get_or_compile(&stale, || compiled(&stale))
            .expect("stale");
        cache
            .get_or_compile(&fresh, || compiled(&fresh))
            .expect("fresh");

        // No watermark yet: sweeping must not guess.
        assert_eq!(cache.sweep(), 0);
        assert_eq!(cache.stats().entries, 2);

        cache.observe_ledger(1_000);
        assert_eq!(cache.sweep(), 1);
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.stale_evictions, 1);
        assert!(cache.get(&fresh).is_some(), "entry within lag survives");
        assert!(cache.get(&stale).is_none(), "stale entry was retired");
    }

    #[test]
    fn observe_ledger_never_moves_backwards() {
        let cache = WasmCache::new(CacheConfig::default());
        cache.observe_ledger(500);
        cache.observe_ledger(400);
        assert_eq!(cache.latest_ledger(), 500);
    }

    #[test]
    fn sustained_load_respects_capacity_and_byte_budget() {
        const ENTRIES: u64 = 10_000;
        const CAPACITY: usize = 64;
        let module_size = empty_module().len();
        let cache = WasmCache::new(
            CacheConfig::default()
                .with_capacity(CAPACITY)
                .with_max_stored_bytes(module_size * CAPACITY),
        );

        for ledger in 0..ENTRIES {
            let key = key("CLOAD", ledger);
            cache
                .get_or_compile(&key, || compiled(&key))
                .expect("insert");
        }

        let stats = cache.stats();
        assert_eq!(stats.entries, CAPACITY, "LRU capacity must be respected");
        assert!(
            stats.stored_bytes <= module_size * CAPACITY,
            "byte budget must hold under sustained load"
        );
        assert_eq!(stats.misses, ENTRIES);
        assert_eq!(stats.hits, 0);
        assert!(stats.lru_evictions >= ENTRIES - CAPACITY as u64);
        assert!(cache.get(&key("CLOAD", ENTRIES - 1)).is_some());
        assert!(cache.get(&key("CLOAD", 0)).is_none());
    }

    #[test]
    fn background_collector_retires_stale_entries_and_stops() {
        let cache = Arc::new(WasmCache::new(
            CacheConfig::default()
                .with_capacity(8)
                .with_max_ledger_lag(5)
                .with_sweep_interval(Duration::from_millis(10)),
        ));
        let stale = key("CCONTRACT", 1);
        cache
            .get_or_compile(&stale, || compiled(&stale))
            .expect("stale");
        cache.observe_ledger(1_000);

        let handle = cache.clone().spawn_collector().expect("collector spawns");
        assert!(handle.is_running());

        // The collector sweeps on its own cadence; poll briefly for the result.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cache.is_empty() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(cache.is_empty(), "collector must evict the stale entry");
        assert_eq!(cache.stats().stale_evictions, 1);

        handle.stop();
        assert!(!handle.is_running());
    }

    #[test]
    fn collector_handle_stops_on_drop() {
        let cache = Arc::new(WasmCache::new(
            CacheConfig::default().with_sweep_interval(Duration::from_millis(10)),
        ));
        let handle = cache.clone().spawn_collector().expect("collector spawns");
        assert!(handle.is_running());
        drop(handle);

        // The worker is gone with the handle and the cache stays usable.
        let key = key("CCONTRACT", 1);
        cache
            .get_or_compile(&key, || compiled(&key))
            .expect("cache still usable");
    }

    #[test]
    fn clear_resets_entries_and_byte_accounting() {
        let cache = WasmCache::new(CacheConfig::default().with_capacity(4));
        let key = key("CCONTRACT", 3);
        cache
            .get_or_compile(&key, || compiled(&key))
            .expect("insert");
        assert!(!cache.is_empty());

        cache.clear();
        assert!(cache.is_empty());
        let stats = cache.stats();
        assert_eq!(stats.stored_bytes, 0);
        assert_eq!(stats.misses, 1, "cumulative counters survive clear");
    }

    #[test]
    fn zero_capacity_still_yields_a_usable_slot() {
        let cache = WasmCache::new(CacheConfig::default().with_capacity(0));
        let key = key("CCONTRACT", 1);
        cache
            .get_or_compile(&key, || compiled(&key))
            .expect("insert");
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().capacity, 1);
    }

    #[test]
    fn key_display_shows_contract_and_ledger() {
        assert_eq!(
            key("CABC", 42).to_string(),
            "CABC@42",
            "keys render predictably for logs"
        );
    }
}
