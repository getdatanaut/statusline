//! The one aggregation the cache itself needs: a day/model fold, derived in
//! memory from cached records rather than persisted alongside them.
//!
//! The full aggregation layer (weekly Monday-bucketing, sessions, 5-hour
//! blocks) is a later task and consumes `UsageCache::all_records()`; this
//! module exists so there is exactly one on-disk source of truth (`records`)
//! and every bucketed view is a pure function of it.

use std::collections::HashMap;

use crate::record::UsageRecord;

/// Model key used for records with no effective model (a `<synthetic>` model,
/// or none at all -- see [`UsageRecord::model`]).
pub const UNKNOWN_MODEL: &str = "unknown";

/// Token counts, cost and message count for one (day, model) bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelAggregate {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_5m_tokens: u64,
    pub cache_creation_1h_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
    /// Number of records folded into this bucket.
    pub messages: u64,
}

impl ModelAggregate {
    /// Sum of every token bucket.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_5m_tokens)
            .saturating_add(self.cache_creation_1h_tokens)
            .saturating_add(self.cache_read_tokens)
    }

    fn add(&mut self, record: &UsageRecord) {
        self.input_tokens = self.input_tokens.saturating_add(record.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(record.output_tokens);
        self.cache_creation_5m_tokens = self
            .cache_creation_5m_tokens
            .saturating_add(record.cache_creation_5m_tokens);
        self.cache_creation_1h_tokens = self
            .cache_creation_1h_tokens
            .saturating_add(record.cache_creation_1h_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(record.cache_read_tokens);
        self.cost += record.cost;
        self.messages = self.messages.saturating_add(1);
    }
}

/// Fold records into `date (YYYY-MM-DD, local) -> model -> aggregate`.
///
/// Accepts anything iterable over borrowed records, so it works equally on a
/// `&Vec<UsageRecord>` and on `UsageCache::all_records()`.
pub fn daily_model_aggregates<'a>(
    records: impl IntoIterator<Item = &'a UsageRecord>,
) -> HashMap<String, HashMap<String, ModelAggregate>> {
    let mut by_date: HashMap<String, HashMap<String, ModelAggregate>> = HashMap::new();
    for record in records {
        let model = record.model.as_deref().unwrap_or(UNKNOWN_MODEL);
        by_date
            .entry(record.date.clone())
            .or_default()
            .entry(model.to_string())
            .or_default()
            .add(record);
    }
    by_date
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(date: &str, model: Option<&str>, input: u64, cost: f64) -> UsageRecord {
        UsageRecord {
            timestamp_ms: 0,
            date: date.to_string(),
            session_id: "sess".to_string(),
            project_path: "proj".to_string(),
            input_tokens: input,
            output_tokens: 1,
            cache_creation_5m_tokens: 2,
            cache_creation_1h_tokens: 3,
            cache_read_tokens: 4,
            model: model.map(str::to_string),
            cost,
            ..UsageRecord::default()
        }
    }

    #[test]
    fn folds_by_date_then_model() {
        let records = vec![
            record("2024-01-15", Some("claude-sonnet-4"), 10, 1.0),
            record("2024-01-15", Some("claude-sonnet-4"), 5, 0.5),
            record("2024-01-15", Some("claude-opus-4"), 7, 2.0),
            record("2024-01-16", Some("claude-sonnet-4"), 1, 0.25),
        ];
        let aggregates = daily_model_aggregates(&records);

        assert_eq!(aggregates.len(), 2);
        let day = &aggregates["2024-01-15"];
        assert_eq!(day.len(), 2);
        assert_eq!(day["claude-sonnet-4"].input_tokens, 15);
        assert_eq!(day["claude-sonnet-4"].messages, 2);
        assert!((day["claude-sonnet-4"].cost - 1.5).abs() < 1e-12);
        assert_eq!(day["claude-opus-4"].input_tokens, 7);
        assert_eq!(aggregates["2024-01-16"]["claude-sonnet-4"].messages, 1);
    }

    #[test]
    fn records_without_a_model_bucket_under_unknown() {
        let records = vec![record("2024-01-15", None, 3, 0.0)];
        let aggregates = daily_model_aggregates(&records);
        assert_eq!(aggregates["2024-01-15"][UNKNOWN_MODEL].input_tokens, 3);
    }

    #[test]
    fn total_tokens_sums_every_bucket() {
        let records = vec![record("2024-01-15", Some("m"), 10, 0.0)];
        let aggregates = daily_model_aggregates(&records);
        // 10 input + 1 output + 2 cc5m + 3 cc1h + 4 cache read
        assert_eq!(aggregates["2024-01-15"]["m"].total_tokens(), 20);
    }
}
