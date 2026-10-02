//! Counting and percentiles.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Unix time in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Counters by name (errors by code, closes by code, …), shared between tasks.
#[derive(Clone, Default)]
pub struct Tally(Arc<Mutex<BTreeMap<String, u64>>>);

impl Tally {
    /// Count one `key`.
    pub fn add(&self, key: impl Into<String>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner).entry(key.into()).or_insert(0) += 1;
    }

    /// The counts.
    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The total.
    pub fn total(&self) -> u64 {
        self.snapshot().values().sum()
    }
}

/// Latency samples in microseconds, shared between tasks.
#[derive(Clone, Default)]
pub struct Samples(Arc<Mutex<Vec<u64>>>);

impl Samples {
    /// Add one sample.
    pub fn add(&self, micros: u64) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(micros);
    }

    /// Add many samples.
    pub fn extend(&self, micros: &[u64]) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(micros);
    }

    /// The number of samples.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// p50 / p90 / p99 / max in milliseconds (JSON), or `null` without samples.
    pub fn summary(&self) -> Value {
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if all.is_empty() {
            return Value::Null;
        }
        all.sort_unstable();
        let at = |q: f64| {
            let index = ((all.len() as f64 - 1.0) * q).round() as usize;
            ms(all[index.min(all.len() - 1)])
        };
        json!({
            "count": all.len(),
            "p50_ms": at(0.50),
            "p90_ms": at(0.90),
            "p99_ms": at(0.99),
            "max_ms": ms(all[all.len() - 1]),
        })
    }
}

fn ms(micros: u64) -> f64 {
    (micros as f64 / 10.0).round() / 100.0
}

/// Print the result: a readable block and one `RESULT {json}` line for reports.
pub fn report(scenario: &str, result: Value) {
    let line = json!({ "scenario": scenario, "result": result });
    if let Ok(pretty) = serde_json::to_string_pretty(&line) {
        println!("{pretty}");
    }
    println!("RESULT {line}");
}
