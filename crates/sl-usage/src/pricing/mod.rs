//! Pricing catalogs, model-name matching, and the cost formula for Claude Code
//! usage records.
//!
//! This is a Claude-scoped port of ccusage's general multi-provider pricing
//! engine (<https://github.com/ccusage/ccusage/blob/8841f921/rust/crates/ccusage-core/src/pricing.rs> and
//! `cost.rs`): the shapes and matching/tiering algorithms are ported, but the
//! DeepSeek time-dependent schedule, the cross-provider model-alias table,
//! user pricing overrides, `long_context_threshold`/per-request tiering, and
//! the mutex-guarded lookup cache are all intentionally left out (see the
//! task brief's "What NOT to build"). We only ever look up Claude model
//! names, at our scale a plain `HashMap` lookup needs no cache.
//!
//! ## Embedded catalogs
//!
//! Refreshed on 2026-11-09 (for Codex's `gpt-6-astra` / `gpt-6.1-sol`); the
//! hand-added Claude 5.5 entries of earlier are upstream now and no longer
//! supplemented.
//!
//! - `models-dev-pricing.json`: verbatim copy of ccusage's flattened
//!   models.dev snapshot (`{ "<model-id>": ModelsDevModel, ... }`, 3218
//!   entries), from `rust/crates/ccusage-pricing-snapshots/src/` at ccusage
//!   commit `c782234083efada49302b2421458b6f7b622ef43`. Its per-model
//!   `exactOnly` flag is read directly off each entry, and a `context` entry
//!   of `cost.tiers` becomes the model's [`LongPrompt`] list (Claude Haiku
//!   5.5's 100k band, OpenAI's 272k one).
//! - `fast-multiplier-overrides.json`: ccusage's, from `ccusage-core` at the
//!   same commit. Read for Codex's priority tier only -- see
//!   [`PricingMap::fast_multiplier`]; Claude's fast mode folds a `-fast` suffix
//!   into the model id, whose own catalog entry supplies its rate.
//! - `codex-auto-review-fallbacks.json`: ccusage's, from
//!   `rust/adapters/codex/src/` at the same commit. Codex's reviewer runs as
//!   `codex-auto-review`, which no catalog prices; as in ccusage it is priced as
//!   the newest model listed as released on or before the record's (UTC) day
//!   -- see [`PricingMap::priced_model`].
//! - `models-dev-catalog-rules.json`: copied in verbatim for provenance, from
//!   the same commit; kept beside the others but not embedded, as nothing
//!   reads it (no asset-priced models in scope).
//! - `litellm-pricing.json`: a **pinned** snapshot of LiteLLM's
//!   `model_prices_and_context_window.json`, fetched once and committed (not
//!   fetched at build or run time):
//!   - Source: `https://raw.githubusercontent.com/BerriAI/litellm/<sha>/model_prices_and_context_window.json`
//!   - Pinned commit: `6a232c90e0865aa4c01f252986a4bfb1a4075795` (tip of
//!     `BerriAI/litellm`'s default branch at fetch time, resolved via
//!     `git ls-remote https://github.com/BerriAI/litellm HEAD`)
//!   - Fetch date: 2026-11-09

use rustc_hash::FxHashMap;
use serde::Deserialize;
use serde_json::Value;

const MODELS_DEV_PRICING_JSON: &str = include_str!("models-dev-pricing.json");
const FAST_MULTIPLIER_OVERRIDES_JSON: &str = include_str!("fast-multiplier-overrides.json");
const CODEX_AUTO_REVIEW_FALLBACKS_JSON: &str = include_str!("codex-auto-review-fallbacks.json");

/// The model id Codex's reviewer writes, priced through
/// [`PricingMap::priced_model`].
const CODEX_AUTO_REVIEW: &str = "codex-auto-review";
/// What it is priced as before the first listed release, as in ccusage.
const CODEX_AUTO_REVIEW_DEFAULT: &str = "gpt-5";

/// One entry of `codex-auto-review-fallbacks.json`.
#[derive(Debug, Deserialize)]
struct AutoReviewFallback {
    /// `YYYY-MM-DD`.
    #[serde(rename = "releasedOn")]
    released_on: String,
    model: String,
}
const LITELLM_PRICING_JSON: &str = include_str!("litellm-pricing.json");

/// Token count above which a bucket's `_above_200k` rate (when the resolved
/// [`Pricing`] has one) applies to the remainder. Matches LiteLLM's
/// `*_above_200k_tokens` field semantics; we don't implement the
/// models.dev/Codex-style per-request `long_context_threshold` (out of
/// scope, see module docs).
const TIER_THRESHOLD_TOKENS: u64 = 200_000;

/// 1-hour cache-creation tokens bill at `input * this`, ignoring the
/// model's published `cache_create` (5-minute) rate entirely.
const CACHE_CREATE_1H_INPUT_MULTIPLIER: f64 = 2.0;

/// An 8-digit `YYYYMMDD` date suffix is the one numeric-version-suffix shape
/// the fuzzy matcher still accepts as a boundary; see
/// [`suffix_starts_with_numeric_model_version`].
const MODEL_DATE_SUFFIX_DIGITS: usize = 8;

/// Unified per-token pricing for one model, resolved from whichever catalog
/// answered the lookup.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    /// 5-minute cache-creation rate ($/token).
    pub cache_create: f64,
    pub cache_read: f64,
    pub input_above_200k: Option<f64>,
    pub output_above_200k: Option<f64>,
    pub cache_create_above_200k: Option<f64>,
    pub cache_read_above_200k: Option<f64>,
    /// A model priced by prompt length: see [`LongPrompt`].
    pub long_prompt: Option<LongPrompt>,
}

/// The price list a request switches to when its *prompt* is long -- Claude
/// Haiku 5.5's pricing: "a request whose prompt is over 100,000 tokens pays
/// higher prices", the prompt counting every input token including cache reads
/// and writes, each request priced on its own.
///
/// Unlike the `*_above_200k` rates, which bill only the tokens past the
/// threshold, this replaces the whole price list for the request: every
/// bucket, output included, bills at these rates once the prompt is over
/// `threshold` and at the plain ones otherwise. Rates are $/token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LongPrompt {
    /// A prompt of more than this many tokens pays the rates below.
    pub threshold: u64,
    pub input: f64,
    pub output: f64,
    /// 5-minute cache-creation rate. The 1-hour one is `input * 2`, as ever.
    pub cache_create: f64,
    pub cache_read: f64,
}

impl LongPrompt {
    /// Builds one from raw per-token rates, deriving an unpublished cache rate
    /// from `input` the way [`Pricing::from_rates`] does.
    fn from_rates(threshold: u64, input: f64, output: f64, cache_create: Option<f64>, cache_read: Option<f64>) -> Self {
        Self { threshold, input, output, cache_create: cache_create.unwrap_or(input * 1.25), cache_read: cache_read.unwrap_or(input * 0.1) }
    }
}

impl Pricing {
    /// Builds a `Pricing` from a catalog entry's raw per-token rates,
    /// deriving unpublished `cache_create`/`cache_read` rates once (not
    /// per-cost-calculation-call) when a catalog doesn't specify them.
    #[expect(clippy::too_many_arguments, reason = "mirrors Pricing's own field list 1:1; a wrapper struct would just duplicate it")]
    fn from_rates(
        input: f64,
        output: f64,
        cache_create: Option<f64>,
        cache_read: Option<f64>,
        input_above_200k: Option<f64>,
        output_above_200k: Option<f64>,
        cache_create_above_200k: Option<f64>,
        cache_read_above_200k: Option<f64>,
    ) -> Self {
        Self {
            input,
            output,
            cache_create: cache_create.unwrap_or(input * 1.25),
            cache_read: cache_read.unwrap_or(input * 0.1),
            input_above_200k,
            output_above_200k,
            cache_create_above_200k,
            cache_read_above_200k,
            long_prompt: None,
        }
    }

    fn with_long_prompt(self, long_prompt: Option<LongPrompt>) -> Self {
        Self { long_prompt, ..self }
    }
}

/// Raw token-count inputs to the cost formula. The 5m/1h cache-creation
/// split matters because the two bucket bill at different rates -- see
/// [`calculate_cost`].
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageTokens {
    pub input: u64,
    pub output: u64,
    pub cache_create_5m: u64,
    pub cache_create_1h: u64,
    pub cache_read: u64,
}

impl UsageTokens {
    /// The request's prompt length: every input token, cache reads and cache
    /// writes included (everything but the output).
    pub fn prompt_len(&self) -> u64 {
        self.input.saturating_add(self.cache_create_5m).saturating_add(self.cache_create_1h).saturating_add(self.cache_read)
    }

    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_create_5m)
            .saturating_add(self.cache_create_1h)
            .saturating_add(self.cache_read)
    }
}

/// `tokens * base` when no `above` rate is published for this bucket;
/// otherwise the first `threshold` tokens bill at `base` and the remainder at
/// `above`. Ported from ccusage's `tiered_cost` (`cost.rs`), minus the
/// per-request `long_context_threshold` two-stage tiering (out of scope).
fn tiered_cost(tokens: u64, base: f64, above: Option<f64>, threshold: u64) -> f64 {
    if tokens == 0 {
        return 0.0;
    }
    if let Some(above) = above
        && tokens > threshold
    {
        return (threshold as f64 * base) + ((tokens - threshold) as f64 * above);
    }
    tokens as f64 * base
}

/// Sums the five independently-tiered cost buckets for one usage event's
/// resolved `Pricing` (or, for a model with a [`LongPrompt`] list and a prompt
/// over its threshold, prices every bucket at that list). 1-hour cache-creation tokens bill at `input * 2.0`
/// (and `input_above_200k * 2.0` past the threshold) -- the model's
/// published `cache_create` rate is for 5-minute creation only and is never
/// used for the 1h bucket.
pub fn calculate_cost(pricing: Pricing, tokens: &UsageTokens) -> f64 {
    // A long prompt swaps the whole price list, not just the excess tokens.
    if let Some(long) = pricing.long_prompt
        && tokens.prompt_len() > long.threshold
    {
        return tokens.cache_create_5m as f64 * long.cache_create
            + tokens.cache_create_1h as f64 * long.input * CACHE_CREATE_1H_INPUT_MULTIPLIER
            + tokens.cache_read as f64 * long.cache_read
            + tokens.input as f64 * long.input
            + tokens.output as f64 * long.output;
    }
    let cache_create_1h_base = pricing.input * CACHE_CREATE_1H_INPUT_MULTIPLIER;
    let cache_create_1h_above = pricing.input_above_200k.map(|rate| rate * CACHE_CREATE_1H_INPUT_MULTIPLIER);

    tiered_cost(tokens.cache_create_5m, pricing.cache_create, pricing.cache_create_above_200k, TIER_THRESHOLD_TOKENS)
        + tiered_cost(tokens.cache_create_1h, cache_create_1h_base, cache_create_1h_above, TIER_THRESHOLD_TOKENS)
        + tiered_cost(tokens.cache_read, pricing.cache_read, pricing.cache_read_above_200k, TIER_THRESHOLD_TOKENS)
        + tiered_cost(tokens.input, pricing.input, pricing.input_above_200k, TIER_THRESHOLD_TOKENS)
        + tiered_cost(tokens.output, pricing.output, pricing.output_above_200k, TIER_THRESHOLD_TOKENS)
}

/// Resolves `model`'s pricing via `pricing_map` and computes its cost,
/// returning `(cost, missing_pricing)`.
///
/// A zero-total-token usage is never flagged `missing_pricing` regardless of
/// lookup result (there is nothing to price). A nonzero-token usage with no
/// model (e.g. a dropped `<synthetic>` model) or an unresolvable model is
/// priced at `0.0` and flagged.
pub fn cost_for_usage(pricing_map: &PricingMap, model: Option<&str>, tokens: &UsageTokens) -> (f64, bool) {
    if tokens.total() == 0 {
        return (0.0, false);
    }
    let Some(model) = model else {
        return (0.0, true);
    };
    match pricing_map.find(model) {
        Some(pricing) => (calculate_cost(pricing, tokens), false),
        None => (0.0, true),
    }
}

// -- models.dev catalog entry shape ------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    cost: Option<ModelsDevCost>,
    #[serde(rename = "exactOnly", default)]
    exact_only: bool,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCost {
    /// Dollars per MILLION tokens; divided by 1_000_000 when converted to
    /// [`Pricing`]'s per-token rates.
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    /// The "cache create" (5m) rate.
    cache_write: Option<f64>,
    /// Our own supplement, not models.dev's: the price list for a prompt over
    /// `threshold` tokens, for a model priced by prompt length. Wins over
    /// `tiers` when both are present.
    #[serde(rename = "longPrompt", default)]
    long_prompt: Option<ModelsDevLongPrompt>,
    /// models.dev's own long-context band, as ccusage's snapshot keeps it: at
    /// most one `{"tier": {"type": "context", "size": N}, ...rates}`. ccusage
    /// prices it as a whole-request switch -- a prompt over `size` tokens pays
    /// these rates for every token -- which is exactly [`LongPrompt`].
    #[serde(default)]
    tiers: Vec<ModelsDevTier>,
}

/// `cost.longPrompt`, in dollars per MILLION tokens like the rest of the entry.
#[derive(Debug, Clone, Copy, Deserialize)]
struct ModelsDevLongPrompt {
    threshold: u64,
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

/// One entry of `cost.tiers`, in dollars per MILLION tokens.
#[derive(Debug, Deserialize)]
struct ModelsDevTier {
    tier: ModelsDevTierBound,
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevTierBound {
    #[serde(rename = "type")]
    kind: String,
    size: u64,
}

impl ModelsDevCost {
    /// The long-prompt list, per million tokens: our `longPrompt`, else the
    /// first `context` tier that states both an input and an output rate.
    fn long_prompt(&self) -> Option<ModelsDevLongPrompt> {
        if let Some(long) = self.long_prompt {
            return Some(long);
        }
        self.tiers.iter().filter(|tier| tier.tier.kind == "context").find_map(|tier| {
            Some(ModelsDevLongPrompt {
                threshold: tier.tier.size,
                input: tier.input?,
                output: tier.output?,
                cache_read: tier.cache_read,
                cache_write: tier.cache_write,
            })
        })
    }
}

/// Usable only when both `cost.input` and `cost.output` are present and at
/// least one is nonzero. Matches `models_dev_usable_cost` in the reference
/// source, minus its catalog-rules modality check (out of scope -- see
/// module docs).
fn models_dev_usable_cost(model: &ModelsDevModel) -> Option<(f64, f64)> {
    let cost = model.cost.as_ref()?;
    let input = cost.input?;
    let output = cost.output?;
    (input != 0.0 || output != 0.0).then_some((input, output))
}

fn per_million(value: f64) -> f64 {
    value / 1_000_000.0
}

fn parse_models_dev(json: &str) -> FxHashMap<String, PricingEntry> {
    let Ok(raw) = serde_json::from_str::<FxHashMap<String, ModelsDevModel>>(json) else {
        return FxHashMap::default();
    };
    raw.into_iter()
        .filter_map(|(id, model)| {
            let (input, output) = models_dev_usable_cost(&model)?;
            let cost = model.cost.as_ref();
            let pricing = Pricing::from_rates(
                per_million(input),
                per_million(output),
                cost.and_then(|cost| cost.cache_write).map(per_million),
                cost.and_then(|cost| cost.cache_read).map(per_million),
                None,
                None,
                None,
                None,
            )
            .with_long_prompt(cost.and_then(ModelsDevCost::long_prompt).map(|long| {
                LongPrompt::from_rates(
                    long.threshold,
                    per_million(long.input),
                    per_million(long.output),
                    long.cache_write.map(per_million),
                    long.cache_read.map(per_million),
                )
            }));
            Some((id, PricingEntry { pricing, exact_only: model.exact_only }))
        })
        .collect()
}

// -- LiteLLM catalog entry shape ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LiteLlmPricing {
    /// Already per-token; no `/1_000_000` conversion, unlike models.dev.
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    input_cost_per_token_above_200k_tokens: Option<f64>,
    output_cost_per_token_above_200k_tokens: Option<f64>,
    cache_creation_input_token_cost_above_200k_tokens: Option<f64>,
    cache_read_input_token_cost_above_200k_tokens: Option<f64>,
    // A prompt over 100k tokens pays these for the whole request (Claude
    // Haiku 5.5); see `LongPrompt`.
    input_cost_per_token_above_100k_tokens: Option<f64>,
    output_cost_per_token_above_100k_tokens: Option<f64>,
    cache_creation_input_token_cost_above_100k_tokens: Option<f64>,
    cache_read_input_token_cost_above_100k_tokens: Option<f64>,
    // OpenAI's long-context band (GPT-5.4 on): a prompt over 272k tokens pays
    // these for the whole request, as ccusage's Codex adapter prices it.
    input_cost_per_token_above_272k_tokens: Option<f64>,
    output_cost_per_token_above_272k_tokens: Option<f64>,
    cache_creation_input_token_cost_above_272k_tokens: Option<f64>,
    cache_read_input_token_cost_above_272k_tokens: Option<f64>,
}

impl LiteLlmPricing {
    /// The whole-request list: the 100k band (Claude Haiku 5.5), else the 272k
    /// one (OpenAI).
    fn long_prompt(&self) -> Option<LongPrompt> {
        let bands = [
            (
                LITELLM_LONG_PROMPT_THRESHOLD,
                self.input_cost_per_token_above_100k_tokens,
                self.output_cost_per_token_above_100k_tokens,
                self.cache_creation_input_token_cost_above_100k_tokens,
                self.cache_read_input_token_cost_above_100k_tokens,
            ),
            (
                LITELLM_LONG_CONTEXT_THRESHOLD,
                self.input_cost_per_token_above_272k_tokens,
                self.output_cost_per_token_above_272k_tokens,
                self.cache_creation_input_token_cost_above_272k_tokens,
                self.cache_read_input_token_cost_above_272k_tokens,
            ),
        ];
        bands.into_iter().find_map(|(threshold, input, output, cache_create, cache_read)| {
            Some(LongPrompt::from_rates(threshold, input?, output?, cache_create, cache_read))
        })
    }
}

/// The threshold LiteLLM's `*_above_100k_tokens` fields are stated against.
const LITELLM_LONG_PROMPT_THRESHOLD: u64 = 100_000;

/// The threshold LiteLLM's `*_above_272k_tokens` fields are stated against.
const LITELLM_LONG_CONTEXT_THRESHOLD: u64 = 272_000;

/// Parses the raw upstream `HashMap<String, Value>` shape, skipping any
/// value that fails to deserialize or is missing either required cost field
/// (matches the reference's `.zip(...)` gate). The upstream file's
/// non-model entries (e.g. `"sample_spec"`) are not special-cased by key
/// name -- see the module-level note in the task report about why that
/// particular key isn't actually filtered by this gate in the currently
/// pinned snapshot, and why that's harmless here.
fn parse_litellm(json: &str) -> FxHashMap<String, PricingEntry> {
    let Ok(raw) = serde_json::from_str::<FxHashMap<String, Value>>(json) else {
        return FxHashMap::default();
    };
    raw.into_iter()
        .filter_map(|(id, value)| {
            let entry: LiteLlmPricing = serde_json::from_value(value).ok()?;
            let input = entry.input_cost_per_token?;
            let output = entry.output_cost_per_token?;
            let pricing = Pricing::from_rates(
                input,
                output,
                entry.cache_creation_input_token_cost,
                entry.cache_read_input_token_cost,
                entry.input_cost_per_token_above_200k_tokens,
                entry.output_cost_per_token_above_200k_tokens,
                entry.cache_creation_input_token_cost_above_200k_tokens,
                entry.cache_read_input_token_cost_above_200k_tokens,
            )
            .with_long_prompt(entry.long_prompt());
            // LiteLLM has no `exactOnly` concept upstream, so every LiteLLM
            // key is fuzzy-eligible.
            Some((id, PricingEntry { pricing, exact_only: false }))
        })
        .collect()
}

// -- Model-name matching ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct PricingEntry {
    pricing: Pricing,
    exact_only: bool,
}

/// Two independent catalogs (LiteLLM, models.dev) of model id -> pricing,
/// each with per-entry `exact_only` gating, and a `find` that resolves a
/// Claude Code model id against them.
pub struct PricingMap {
    litellm: FxHashMap<String, PricingEntry>,
    models_dev: FxHashMap<String, PricingEntry>,
    fast: FastMultipliers,
    /// Newest release first.
    auto_review: Vec<AutoReviewFallback>,
    fingerprint: u64,
    /// What [`find`](Self::find) answered for each name no catalog has
    /// exactly: that answer is a scan of both catalogs, and a transcript
    /// names the same few models on every line.
    scanned: std::sync::Mutex<FxHashMap<String, Option<Pricing>>>,
}

/// `fast-multiplier-overrides.json`: what a request on the priority ("fast")
/// service tier pays, as a multiple of the standard price. Read only for
/// Codex, whose records carry the tier separately; a Claude fast-mode record
/// names its own `-fast` model id instead, priced by the catalogs directly.
#[derive(Debug, Default, Deserialize)]
struct FastMultipliers {
    #[serde(default)]
    exact: FxHashMap<String, f64>,
    #[serde(default)]
    normalized_prefix: FxHashMap<String, f64>,
}

impl FastMultipliers {
    /// Ported from ccusage's `FastMultiplierOverrides::multiplier_for`, minus
    /// its alias table: the exact id, else a `/`- or `:`-separated part of the
    /// `.`/`@`-normalized id that starts with a listed prefix.
    fn multiplier_for(&self, model: &str) -> Option<f64> {
        if let Some(multiplier) = self.exact.get(model) {
            return Some(*multiplier);
        }
        let normalized = model.replace(['.', '@'], "-");
        normalized.split(['/', ':']).find_map(|part| {
            self.normalized_prefix.iter().find_map(|(base, multiplier)| {
                (part == base || part.strip_prefix(base.as_str()).is_some_and(|rest| rest.starts_with('-'))).then_some(*multiplier)
            })
        })
    }
}

/// Bumped whenever [`calculate_cost`] or [`cost_for_usage`] changes what they
/// compute from the same catalogs, so a stored cost computed by the old code is
/// recognised as stale. A catalog change needs no bump: the catalogs are part
/// of [`PricingMap::fingerprint`] already.
const COST_FORMULA_VERSION: u32 = 1;

impl PricingMap {
    /// Builds the map from the embedded catalogs. Cheap enough to call once
    /// and reuse -- no need to memoize the embedded JSON parse.
    pub fn new() -> Self {
        let fingerprint = crate::stable_hash(&[
            &COST_FORMULA_VERSION.to_le_bytes(),
            LITELLM_PRICING_JSON.as_bytes(),
            MODELS_DEV_PRICING_JSON.as_bytes(),
            FAST_MULTIPLIER_OVERRIDES_JSON.as_bytes(),
            CODEX_AUTO_REVIEW_FALLBACKS_JSON.as_bytes(),
        ]);
        let mut auto_review: Vec<AutoReviewFallback> = serde_json::from_str(CODEX_AUTO_REVIEW_FALLBACKS_JSON).unwrap_or_default();
        auto_review.sort_by(|a, b| b.released_on.cmp(&a.released_on));
        Self {
            litellm: parse_litellm(LITELLM_PRICING_JSON),
            models_dev: parse_models_dev(MODELS_DEV_PRICING_JSON),
            fast: serde_json::from_str(FAST_MULTIPLIER_OVERRIDES_JSON).unwrap_or_default(),
            auto_review,
            fingerprint,
            scanned: std::sync::Mutex::default(),
        }
    }

    /// The model a record of `model` at `timestamp_ms` is priced as: itself,
    /// except Codex's reviewer, priced as the newest model released on or
    /// before the record's UTC day (ccusage's `codex_log_model_fallback`).
    pub fn priced_model<'a>(&'a self, model: &'a str, timestamp_ms: i64) -> &'a str {
        if model != CODEX_AUTO_REVIEW {
            return model;
        }
        let Some(day) = chrono::DateTime::from_timestamp_millis(timestamp_ms).map(|at| at.format("%Y-%m-%d").to_string()) else {
            return CODEX_AUTO_REVIEW_DEFAULT;
        };
        self.auto_review.iter().find(|fallback| day.as_str() >= fallback.released_on.as_str()).map_or(CODEX_AUTO_REVIEW_DEFAULT, |fallback| fallback.model.as_str())
    }

    /// What a priority-tier request for `model` pays, as a multiple of its
    /// standard price: `1.0` when nothing is listed, as in ccusage.
    pub fn fast_multiplier(&self, model: &str) -> f64 {
        self.fast.multiplier_for(model).unwrap_or(1.0)
    }

    /// Identifies what this map would charge: the embedded catalogs and the
    /// version of the cost formula. Stable across runs and builds, so a cost
    /// stored under one fingerprint is known to be current, or not, under
    /// another.
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// Resolves `model`'s pricing.
    ///
    /// Step 1 (exact match) and step 2 (fuzzy scan) each run across *both*
    /// catalogs, LiteLLM first, before falling through to the next step --
    /// not "try LiteLLM's exact+fuzzy entirely, then models.dev's
    /// entirely". That ordering matters for the exact-only gate below: if
    /// LiteLLM's fuzzy scan ran before models.dev's *exact* match had a
    /// chance, a plain LiteLLM `claude-opus-5` entry could fuzzy-match a
    /// query of `claude-opus-5-fast` and shadow models.dev's real
    /// `exact_only` tier entry for it (LiteLLM has no `exact_only` concept
    /// of its own to prevent that). Trying every catalog's exact match
    /// first closes that hole.
    pub fn find(&self, model: &str) -> Option<Pricing> {
        if let Some(entry) = self.litellm.get(model) {
            return Some(entry.pricing);
        }
        if let Some(entry) = self.models_dev.get(model) {
            return Some(entry.pricing);
        }
        let mut scanned = self.scanned.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pricing) = scanned.get(model) {
            return *pricing;
        }
        let pricing = self.scan(model);
        scanned.insert(model.to_owned(), pricing);
        pricing
    }

    /// [`find`](Self::find) past its exact matches.
    fn scan(&self, model: &str) -> Option<Pricing> {

        // Exact-only gating applies across both catalogs too: a model name
        // that spells (as written, or under `.`/`@` -> `-` normalization) an
        // exact_only entry in *either* catalog must not be answered by a
        // fuzzy match elsewhere -- that's the other half of "only step 1
        // reaches an exact_only entry".
        let normalized_model = normalized_pricing_key(model);
        if is_exact_only_match(&self.litellm, model, normalized_model.as_ref())
            || is_exact_only_match(&self.models_dev, model, normalized_model.as_ref())
        {
            return None;
        }

        fuzzy_find(&self.litellm, model, normalized_model.as_ref()).or_else(|| fuzzy_find(&self.models_dev, model, normalized_model.as_ref()))
    }
}

impl Default for PricingMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether any `exact_only` entry in `catalog` is named by `model`, as
/// written or under separator normalization.
fn is_exact_only_match(catalog: &FxHashMap<String, PricingEntry>, model: &str, normalized_model: &str) -> bool {
    catalog.iter().filter(|(_, entry)| entry.exact_only).any(|(candidate, _)| candidate == model || normalized_pricing_key(candidate) == normalized_model)
}

/// Fuzzy bidirectional substring scan over one catalog, skipping exact-only
/// entries entirely (an exact-only entry is reachable only by naming it
/// exactly, per [`PricingMap::find`]'s step 1). Among all matches, the
/// longest matching key wins (most specific).
fn fuzzy_find(catalog: &FxHashMap<String, PricingEntry>, model: &str, normalized_model: &str) -> Option<Pricing> {
    catalog
        .iter()
        .filter(|(_, entry)| !entry.exact_only)
        .filter(|(candidate, _)| pricing_key_matches(candidate, model, normalized_model))
        .max_by(|(left, _), (right, _)| left.len().cmp(&right.len()).then_with(|| right.cmp(left)))
        .map(|(_, entry)| entry.pricing)
}

/// Matches a catalog key against a query model id, trying both the raw
/// strings and a normalized-separator (`.`/`@` -> `-`) variant. Ported from
/// ccusage's `pricing_key_matches`.
fn pricing_key_matches(candidate: &str, model: &str, normalized_model: &str) -> bool {
    if contains_pricing_key(model, candidate) || contains_pricing_key(candidate, model) {
        return true;
    }
    let normalized_candidate = normalized_pricing_key(candidate);
    contains_pricing_key(normalized_model, normalized_candidate.as_ref()) || contains_pricing_key(normalized_candidate.as_ref(), normalized_model)
}

/// Whether `key` occurs in `value` as a substring at non-alphanumeric
/// boundaries on both sides (start-of-string/end-of-string count as
/// boundaries). Ported from ccusage's `contains_pricing_key`.
fn contains_pricing_key(value: &str, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    value.match_indices(key).any(|(index, _)| {
        let before = index.checked_sub(1).and_then(|before| value.as_bytes().get(before)).copied();
        let Some(suffix) = value.get(index + key.len()..) else { return false };
        before.is_none_or(is_pricing_key_boundary) && suffix_allows_pricing_key_match(key, suffix)
    })
}

fn is_pricing_key_boundary(byte: u8) -> bool {
    !byte.is_ascii_alphanumeric()
}

/// Whether the byte(s) right after a matched `key` still count as a valid
/// boundary. A non-alphanumeric separator is a boundary in general, *except*
/// when it introduces a numeric-version suffix that isn't a clean 8-digit
/// `YYYYMMDD` date (see [`suffix_starts_with_numeric_model_version`]) -- that
/// specific shape is rejected so `claude-opus-4` cannot match an entry keyed
/// `claude-opus-4-5`, while a real transcript id's date suffix
/// (`claude-sonnet-4-5-20250929` against a `claude-sonnet-4-5` key) is still
/// accepted.
fn suffix_allows_pricing_key_match(key: &str, suffix: &str) -> bool {
    let Some(separator) = suffix.as_bytes().first().copied() else {
        return true;
    };
    if !is_pricing_key_boundary(separator) {
        return false;
    }
    !suffix_starts_with_numeric_model_version(key, suffix)
}

/// True for a `-`/`.`-prefixed run of ASCII digits right after a
/// digit-ending `key` -- *unless* that run is exactly 8 digits (a date) and
/// is itself followed by a boundary or end-of-string, in which case it's
/// false (the date-suffix case is not treated as an unresolvable numeric
/// version and so does not reject the match in
/// [`suffix_allows_pricing_key_match`]).
///
/// This is the boundary-rule ambiguity the task brief flagged: read directly
/// off this ported function (see the task report for the worked-through
/// derivation), not paraphrased.
fn suffix_starts_with_numeric_model_version(key: &str, suffix: &str) -> bool {
    if !key.as_bytes().last().is_some_and(u8::is_ascii_digit) {
        return false;
    }
    if !matches!(suffix.as_bytes().first(), Some(b'-' | b'.')) {
        return false;
    }

    let Some(rest) = suffix.get(1..) else { return false };
    let digit_len = rest.as_bytes().iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digit_len == 0 {
        return false;
    }
    let after_digits = rest.as_bytes().get(digit_len).copied();
    !(digit_len == MODEL_DATE_SUFFIX_DIGITS && after_digits.is_none_or(is_pricing_key_boundary))
}

/// Normalizes `.`/`@` separators to `-` for the fuzzy scan (not used for the
/// exact-match step). Ported from ccusage's `normalized_pricing_key`.
fn normalized_pricing_key(value: &str) -> std::borrow::Cow<'_, str> {
    if value.contains(['.', '@']) {
        std::borrow::Cow::Owned(value.replace(['.', '@'], "-"))
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_pricing_map(litellm: &[(&str, Pricing, bool)], models_dev: &[(&str, Pricing, bool)]) -> PricingMap {
        let build = |entries: &[(&str, Pricing, bool)]| {
            entries.iter().map(|&(id, pricing, exact_only)| (id.to_string(), PricingEntry { pricing, exact_only })).collect()
        };
        PricingMap { litellm: build(litellm), models_dev: build(models_dev), fast: FastMultipliers::default(), auto_review: Vec::new(), fingerprint: 0, scanned: std::sync::Mutex::default() }
    }

    fn flat_pricing(input: f64, output: f64) -> Pricing {
        Pricing::from_rates(input, output, None, None, None, None, None, None)
    }

    /// A name no catalog has exactly is scanned for once; asking again
    /// gives the same answer, found or not, from what the scan left.
    #[test]
    fn a_scanned_name_is_answered_the_same_without_scanning_again() {
        let map = synthetic_pricing_map(&[("claude-sonnet-4-5", flat_pricing(3.0, 15.0), false)], &[]);
        let fuzzy = map.find("anthropic/claude-sonnet-4-5");
        assert_eq!(fuzzy, Some(flat_pricing(3.0, 15.0)));
        assert_eq!(map.find("anthropic/claude-sonnet-4-5"), fuzzy);
        assert_eq!(map.find("gpt-unknown"), None);
        assert_eq!(map.find("gpt-unknown"), None);
        assert_eq!(map.find("claude-sonnet-4-5"), fuzzy, "an exact name is not kept: the catalog has it");
        let scanned = map.scanned.lock().unwrap();
        assert_eq!(scanned.len(), 2, "one entry per scanned name: {:?}", scanned.keys().collect::<Vec<_>>());
        assert_eq!(scanned.get("gpt-unknown"), Some(&None), "a miss is kept too");
    }

    // -- ccusage's own cost fixture (task brief Verification §3) --------------------------------

    #[test]
    fn ccusage_cost_fixture_matches_expected_total() {
        // input=1.0, output=10.0, cache_create=1.25, cache_read=0.1 ($/token,
        // explicit, no derivation); 5m:10, 1h:20, cache_read:30, all well
        // under 200k so no tiering kicks in; zero input/output tokens so the
        // total is exactly the cache-only sum the plan states.
        let pricing = Pricing::from_rates(1.0, 10.0, Some(1.25), Some(0.1), None, None, None, None);
        let tokens = UsageTokens { input: 0, output: 0, cache_create_5m: 10, cache_create_1h: 20, cache_read: 30 };
        let cost = calculate_cost(pricing, &tokens);
        assert_eq!(cost, 55.5, "10*1.25 + 20*(1.0*2.0) + 30*0.1 == 55.5");
    }

    // -- Unpublished-rate derivation -------------------------------------------------------------

    #[test]
    fn unpublished_cache_rates_derive_from_input_rate() {
        let pricing = Pricing::from_rates(2.0, 8.0, None, None, None, None, None, None);
        assert_eq!(pricing.cache_create, 2.0 * 1.25);
        assert_eq!(pricing.cache_read, 2.0 * 0.1);
    }

    #[test]
    fn published_cache_rates_are_not_overridden_by_derivation() {
        let pricing = Pricing::from_rates(2.0, 8.0, Some(9.0), Some(0.5), None, None, None, None);
        assert_eq!(pricing.cache_create, 9.0);
        assert_eq!(pricing.cache_read, 0.5);
    }

    // -- Model-matching boundary rule: the flagged ambiguity, pinned directly ------------------
    //
    // Read straight off `suffix_starts_with_numeric_model_version` /
    // `suffix_allows_pricing_key_match`: a numeric-version suffix (like the
    // "-5" in "claude-opus-4-5") breaks the boundary and REJECTS the match;
    // the one exception is a clean 8-digit YYYYMMDD date suffix, which is
    // still ACCEPTED. See the module doc comment on
    // `suffix_starts_with_numeric_model_version` and the task report for the
    // full derivation.

    #[test]
    fn short_numeric_suffix_rejects_the_match() {
        assert!(!contains_pricing_key("claude-opus-4-5", "claude-opus-4"), "claude-opus-4 must not match claude-opus-4-5");
    }

    #[test]
    fn eight_digit_date_suffix_accepts_the_match() {
        assert!(
            contains_pricing_key("claude-sonnet-4-5-20250929", "claude-sonnet-4-5"),
            "an 8-digit YYYYMMDD date suffix is still a valid boundary"
        );
    }

    #[test]
    fn seven_digit_suffix_rejects_the_match() {
        // Not 8 digits -> falls into the general numeric-suffix rejection,
        // same as the single-digit case.
        assert!(!contains_pricing_key("claude-sonnet-4-5-2025092", "claude-sonnet-4-5"));
    }

    #[test]
    fn nine_digit_suffix_rejects_the_match() {
        // Not exactly 8 digits either -> rejected.
        assert!(!contains_pricing_key("claude-sonnet-4-5-202509299", "claude-sonnet-4-5"));
    }

    #[test]
    fn eight_digits_followed_by_more_alnum_rejects_the_match() {
        // The 8-digit run isn't itself followed by a boundary (there's a
        // 9th digit continuing it), so it isn't a clean date suffix.
        assert!(!contains_pricing_key("claude-sonnet-4-5-202509291", "claude-sonnet-4-5"));
    }

    #[test]
    fn non_numeric_suffix_after_digit_ending_key_accepts_the_match() {
        // "-fast" is a boundary separator, not a numeric-version suffix at
        // all, so it's unaffected by the exception and accepted normally.
        assert!(contains_pricing_key("claude-opus-5-fast", "claude-opus-5"));
    }

    // -- Model matching against the real embedded models.dev catalog --------------------------

    #[test]
    fn opus_4_exact_key_is_not_corrupted_by_a_longer_sibling_key() {
        let map = PricingMap::new();
        let pricing = map.find("claude-opus-4").expect("claude-opus-4 is a real models.dev entry");
        // $15/$75 per MTok, not claude-opus-4-5's $5/$25.
        assert!((pricing.input - 15.0 / 1_000_000.0).abs() < 1e-12);
        assert!((pricing.output - 75.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn date_suffixed_transcript_model_id_resolves_to_base_pricing_via_exact_key() {
        // `claude-sonnet-4-5-20250929` happens to itself be a literal
        // (exact_only) key in the embedded snapshot, with pricing identical
        // to the base `claude-sonnet-4-5` entry, so this also exercises the
        // exact-match path directly.
        let map = PricingMap::new();
        let pricing = map.find("claude-sonnet-4-5-20250929").expect("real transcript model id must resolve");
        assert!((pricing.input - 3.0 / 1_000_000.0).abs() < 1e-12);
        assert!((pricing.output - 15.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn non_literal_date_suffixed_model_id_resolves_via_real_fuzzy_scan() {
        // A future date not present as a literal key in the embedded
        // snapshot -- this genuinely exercises the fuzzy path (not just the
        // exact-match short-circuit) against the real catalog data.
        let map = PricingMap::new();
        assert!(!map.models_dev.contains_key("claude-sonnet-4-5-20301231"), "test assumption: this id must not be a literal catalog key");
        let pricing = map.find("claude-sonnet-4-5-20301231").expect("must fuzzy-match claude-sonnet-4-5");
        assert!((pricing.input - 3.0 / 1_000_000.0).abs() < 1e-12);
        assert!((pricing.output - 15.0 / 1_000_000.0).abs() < 1e-12);
    }

    // -- Exact-only gating ----------------------------------------------------------------------

    #[test]
    fn exact_only_variant_never_wins_a_plain_query() {
        let map = PricingMap::new();
        let pricing = map.find("claude-opus-5").expect("claude-opus-5 is a real models.dev entry");
        assert!((pricing.input - 5.0 / 1_000_000.0).abs() < 1e-12, "must resolve to the base $5/MTok entry");
        assert!((pricing.output - 25.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn exact_only_variant_resolves_to_its_own_pricing_when_queried_exactly() {
        let map = PricingMap::new();
        let pricing = map.find("claude-opus-5-fast").expect("claude-opus-5-fast is a real exact_only models.dev entry");
        assert!((pricing.input - 10.0 / 1_000_000.0).abs() < 1e-12, "must resolve to its own $10/MTok entry, not the base $5");
        assert!((pricing.output - 50.0 / 1_000_000.0).abs() < 1e-12);
    }

    /// The 5.5-generation ids were once absent from the catalogs and priced as
    /// `$0`; a base key like `claude-opus-5` must not stand in for them (its
    /// rate is different), so each needs its own entry.
    #[test]
    fn current_model_ids_resolve_to_their_own_rates() {
        let map = PricingMap::new();
        for (model, input, output) in [
            ("claude-sonnet-5-5", 2.0, 10.0),
            ("claude-opus-5-5", 4.0, 20.0),
            ("claude-fable-5-1", 10.0, 50.0),
        ] {
            let pricing = map.find(model).unwrap_or_else(|| panic!("{model} must be priced"));
            assert!((pricing.input - input / 1_000_000.0).abs() < 1e-12, "{model} input");
            assert!((pricing.output - output / 1_000_000.0).abs() < 1e-12, "{model} output");
        }
    }

    // -- Claude Haiku 5.5: priced by prompt length ----------------------------------------------

    /// Per-token rates for Haiku 5.5 from Anthropic's pricing page ($/MTok):
    /// 0.10 in / 0.50 out / 0.125 cache write / 0.01 cache read, and for a
    /// prompt over 100,000 tokens 0.50 / 2.50 / 0.625 / 0.05.
    #[test]
    fn haiku_5_5_resolves_with_its_base_and_long_prompt_rates() {
        let map = PricingMap::new();
        for model in ["claude-haiku-5-5", "anthropic/claude-haiku-5-5"] {
            let pricing = map.find(model).unwrap_or_else(|| panic!("{model} must be priced"));
            let per_m = |rate: f64| rate * 1_000_000.0;
            assert!((per_m(pricing.input) - 0.10).abs() < 1e-9, "{model} input");
            assert!((per_m(pricing.output) - 0.50).abs() < 1e-9, "{model} output");
            assert!((per_m(pricing.cache_create) - 0.125).abs() < 1e-9, "{model} cache write");
            assert!((per_m(pricing.cache_read) - 0.01).abs() < 1e-9, "{model} cache read");
            let long = pricing.long_prompt.unwrap_or_else(|| panic!("{model} is priced by prompt length"));
            assert_eq!(long.threshold, 100_000);
            assert!((per_m(long.input) - 0.50).abs() < 1e-9, "{model} long input");
            assert!((per_m(long.output) - 2.50).abs() < 1e-9, "{model} long output");
            assert!((per_m(long.cache_create) - 0.625).abs() < 1e-9, "{model} long cache write");
            assert!((per_m(long.cache_read) - 0.05).abs() < 1e-9, "{model} long cache read");
        }
        // The older Haiku, and the other 5.5-generation models, have no such tier.
        for model in ["claude-haiku-4-5-20251001", "claude-sonnet-5-5", "claude-opus-5-5", "claude-fable-5-1"] {
            assert!(map.find(model).unwrap().long_prompt.is_none(), "{model} is not priced by prompt length");
        }
    }

    fn haiku_5_5() -> Pricing {
        PricingMap::new().find("claude-haiku-5-5").unwrap()
    }

    // -- Codex's models -----------------------------------------------------------------------

    /// models.dev's `openai` provider and LiteLLM agree on these ($/MTok):
    /// astra 10 / 50 / 1 cache read, sol 2 / 10 / 0.1, and a prompt over
    /// 272k tokens pays astra 20 / 75 / 2, sol 4 / 15 / 0.2 for every token.
    #[test]
    fn gpt_6_models_resolve_with_their_272k_long_prompt_lists() {
        let map = PricingMap::new();
        let per_m = |rate: f64| rate * 1_000_000.0;
        for (model, base, long) in [("gpt-6-astra", (10.0, 50.0, 1.0), (20.0, 75.0, 2.0)), ("gpt-6.1-sol", (2.0, 10.0, 0.1), (4.0, 15.0, 0.2))] {
            let pricing = map.find(model).unwrap_or_else(|| panic!("{model} must be priced"));
            assert!((per_m(pricing.input) - base.0).abs() < 1e-9, "{model} input");
            assert!((per_m(pricing.output) - base.1).abs() < 1e-9, "{model} output");
            assert!((per_m(pricing.cache_read) - base.2).abs() < 1e-9, "{model} cache read");
            let list = pricing.long_prompt.unwrap_or_else(|| panic!("{model} has a long-context band"));
            assert_eq!(list.threshold, 272_000, "{model}");
            assert!((per_m(list.input) - long.0).abs() < 1e-9, "{model} long input");
            assert!((per_m(list.output) - long.1).abs() < 1e-9, "{model} long output");
            assert!((per_m(list.cache_read) - long.2).abs() < 1e-9, "{model} long cache read");
        }
    }

    #[test]
    fn a_codex_prompt_over_272k_reprices_the_whole_request() {
        let astra = PricingMap::new().find("gpt-6-astra").unwrap();
        let short = UsageTokens { input: 72_000, output: 1_000, cache_read: 200_000, ..UsageTokens::default() };
        let long = UsageTokens { input: 72_001, ..short };
        assert!((calculate_cost(astra, &short) - (72_000.0 * 10.0 + 1_000.0 * 50.0 + 200_000.0 * 1.0) / 1e6).abs() < 1e-9);
        assert!((calculate_cost(astra, &long) - (72_001.0 * 20.0 + 1_000.0 * 75.0 + 200_000.0 * 2.0) / 1e6).abs() < 1e-9);
    }

    #[test]
    fn models_dev_context_tiers_become_the_long_prompt_list() {
        let parsed = parse_models_dev(
            r#"{"m": {"cost": {"input": 1, "output": 2, "tiers": [{"input": 3, "output": 4, "cache_read": 0.5, "tier": {"type": "context", "size": 1000}}]}},
                "n": {"cost": {"input": 1, "output": 2, "tiers": [{"input": 3, "output": 4, "tier": {"type": "elsewhere", "size": 1000}}]}}}"#,
        );
        let long = parsed["m"].pricing.long_prompt.expect("a context tier is read");
        assert_eq!(long.threshold, 1000);
        assert!((long.input - 3e-6).abs() < 1e-15 && (long.output - 4e-6).abs() < 1e-15 && (long.cache_read - 0.5e-6).abs() < 1e-15);
        assert!(parsed["n"].pricing.long_prompt.is_none(), "only a context tier is a prompt-length band");
    }

    #[test]
    fn codex_auto_review_is_priced_as_the_newest_model_released_by_its_day() {
        let map = PricingMap::new();
        let at = |day: &str| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").unwrap().and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp_millis();
        assert_eq!(map.priced_model("codex-auto-review", at("2026-11-09")), "gpt-5.6-luna");
        assert_eq!(map.priced_model("codex-auto-review", at("2026-07-30")), "gpt-5.6-luna", "from its release day");
        assert_eq!(map.priced_model("codex-auto-review", at("2026-07-29")), "gpt-5.4");
        assert_eq!(map.priced_model("codex-auto-review", at("2025-01-01")), "gpt-5", "before any listed release");
        assert_eq!(map.priced_model("gpt-6-astra", at("2026-11-09")), "gpt-6-astra", "every other model is itself");
        assert!(map.find("gpt-5.6-luna").is_some(), "and the fallback is priced");
    }

    #[test]
    fn the_priority_tier_multiplier_comes_from_the_overrides() {
        let map = PricingMap::new();
        assert_eq!(map.fast_multiplier("gpt-6-astra"), 2.0);
        assert_eq!(map.fast_multiplier("gpt-5.5"), 2.5);
        assert_eq!(map.fast_multiplier("codex-auto-review"), 1.0, "nothing listed pays the standard price");
    }

    #[test]
    fn a_prompt_at_or_under_the_threshold_pays_the_base_rates() {
        // 40k fresh + 50k cached + 10k written = exactly 100,000 prompt tokens.
        let tokens = UsageTokens { input: 40_000, output: 8_000, cache_create_5m: 6_000, cache_create_1h: 4_000, cache_read: 50_000 };
        assert_eq!(tokens.prompt_len(), 100_000);
        let expected = (40_000.0 * 0.10 + 8_000.0 * 0.50 + 6_000.0 * 0.125 + 4_000.0 * 0.20 + 50_000.0 * 0.01) / 1e6;
        assert!((calculate_cost(haiku_5_5(), &tokens) - expected).abs() < 1e-12);
    }

    #[test]
    fn a_prompt_over_the_threshold_pays_the_long_rates_for_every_token_including_output() {
        // One token over: the whole request reprices, not just the excess.
        let tokens = UsageTokens { input: 40_001, output: 8_000, cache_create_5m: 6_000, cache_create_1h: 4_000, cache_read: 50_000 };
        assert_eq!(tokens.prompt_len(), 100_001);
        // 1h cache writes bill at twice the (long) input rate.
        let expected = (40_001.0 * 0.50 + 8_000.0 * 2.50 + 6_000.0 * 0.625 + 4_000.0 * 1.00 + 50_000.0 * 0.05) / 1e6;
        assert!((calculate_cost(haiku_5_5(), &tokens) - expected).abs() < 1e-12);
    }

    /// Output tokens are not part of the prompt: a huge output cannot tip a
    /// short prompt over the line.
    #[test]
    fn output_tokens_do_not_count_towards_the_prompt_length() {
        let tokens = UsageTokens { input: 1_000, output: 500_000, cache_create_5m: 0, cache_create_1h: 0, cache_read: 0 };
        let expected = (1_000.0 * 0.10 + 500_000.0 * 0.50) / 1e6;
        assert!((calculate_cost(haiku_5_5(), &tokens) - expected).abs() < 1e-12);
    }

    #[test]
    fn haiku_5_5_usage_is_costed_and_not_flagged_as_missing_pricing() {
        let map = PricingMap::new();
        let tokens = UsageTokens { input: 10, output: 1_000, cache_create_5m: 0, cache_create_1h: 0, cache_read: 200_000 };
        let (cost, missing) = cost_for_usage(&map, Some("claude-haiku-5-5"), &tokens);
        assert!(!missing);
        // Prompt of 200,010 tokens: long rates throughout.
        assert!((cost - (10.0 * 0.50 + 1_000.0 * 2.50 + 200_000.0 * 0.05) / 1e6).abs() < 1e-12, "cost {cost}");
    }

    /// LiteLLM answers before models.dev, so a refreshed LiteLLM snapshot that
    /// lists Haiku 5.5 must carry the tier through too, or it would quietly
    /// underprice what models.dev prices right.
    #[test]
    fn litellm_above_100k_fields_become_a_long_prompt_tier() {
        let parsed = parse_litellm(
            r#"{"claude-haiku-5-5": {
                "input_cost_per_token": 1e-7, "output_cost_per_token": 5e-7,
                "cache_creation_input_token_cost": 1.25e-7, "cache_read_input_token_cost": 1e-8,
                "input_cost_per_token_above_100k_tokens": 5e-7, "output_cost_per_token_above_100k_tokens": 2.5e-6,
                "cache_creation_input_token_cost_above_100k_tokens": 6.25e-7, "cache_read_input_token_cost_above_100k_tokens": 5e-8
            }, "plain": {"input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6}}"#,
        );
        let long = parsed["claude-haiku-5-5"].pricing.long_prompt.expect("tier parsed");
        assert_eq!(long.threshold, 100_000);
        assert_eq!((long.input, long.output, long.cache_create, long.cache_read), (5e-7, 2.5e-6, 6.25e-7, 5e-8));
        assert!(parsed["plain"].pricing.long_prompt.is_none(), "no tier fields, no tier");
    }

    // -- 200k tiering -----------------------------------------------------------------------------

    #[test]
    fn bucket_with_above_200k_rate_splits_at_the_threshold() {
        let cost = tiered_cost(250_000, 1.0, Some(2.0), 200_000);
        assert_eq!(cost, 200_000.0 * 1.0 + 50_000.0 * 2.0);
    }

    #[test]
    fn bucket_without_above_200k_rate_charges_base_for_every_token() {
        let cost = tiered_cost(250_000, 1.0, None, 200_000);
        assert_eq!(cost, 250_000.0 * 1.0);
    }

    #[test]
    fn calculate_cost_tiers_each_bucket_independently() {
        let pricing = Pricing {
            input: 1.0,
            output: 2.0,
            cache_create: 1.25,
            cache_read: 0.1,
            input_above_200k: Some(0.5),
            output_above_200k: None,
            cache_create_above_200k: Some(0.6),
            cache_read_above_200k: None,
            long_prompt: None,
        };
        let tokens = UsageTokens { input: 250_000, output: 250_000, cache_create_5m: 250_000, cache_create_1h: 0, cache_read: 250_000 };
        let cost = calculate_cost(pricing, &tokens);
        let expected = (200_000.0 * 1.0 + 50_000.0 * 0.5) // input: tiered
            + (250_000.0 * 2.0) // output: no above rate, base for all
            + (200_000.0 * 1.25 + 50_000.0 * 0.6) // cache_create 5m: tiered
            + (250_000.0 * 0.1); // cache_read: no above rate, base for all
        assert_eq!(cost, expected);
    }

    // -- Catalog precedence -----------------------------------------------------------------------

    #[test]
    fn litellm_rate_wins_when_both_catalogs_have_the_model() {
        let litellm_pricing = flat_pricing(1.0, 2.0);
        let models_dev_pricing = flat_pricing(9.0, 9.0);
        let map = synthetic_pricing_map(&[("dual-catalog-model", litellm_pricing, false)], &[("dual-catalog-model", models_dev_pricing, false)]);
        let resolved = map.find("dual-catalog-model").unwrap();
        assert_eq!(resolved.input, 1.0);
        assert_eq!(resolved.output, 2.0);
    }

    #[test]
    fn models_dev_answers_when_litellm_has_no_usable_entry() {
        let models_dev_pricing = flat_pricing(9.0, 9.0);
        let map = synthetic_pricing_map(&[], &[("models-dev-only-model", models_dev_pricing, false)]);
        let resolved = map.find("models-dev-only-model").unwrap();
        assert_eq!(resolved.input, 9.0);
    }

    #[test]
    fn real_catalogs_disagree_on_a_bedrock_claude_id_and_litellm_wins() {
        // A real Claude model id present in both embedded catalogs with
        // genuinely different published rates as of the pinned snapshots:
        // LiteLLM $5.5/MTok input vs models.dev $5/MTok input.
        let map = PricingMap::new();
        let model = "us.anthropic.claude-opus-4-5-20251101-v1:0";
        assert!(map.litellm.contains_key(model) && map.models_dev.contains_key(model), "test assumption: both catalogs carry this id");
        let pricing = map.find(model).unwrap();
        assert!((pricing.input - 5.5 / 1_000_000.0).abs() < 1e-12, "LiteLLM's rate must win, not models.dev's $5/MTok");
    }

    // -- Missing pricing --------------------------------------------------------------------------

    #[test]
    fn unknown_model_with_nonzero_tokens_is_flagged_missing() {
        let map = synthetic_pricing_map(&[], &[]);
        let tokens = UsageTokens { input: 10, ..UsageTokens::default() };
        let (cost, missing) = cost_for_usage(&map, Some("totally-unknown-model"), &tokens);
        assert_eq!(cost, 0.0);
        assert!(missing);
    }

    #[test]
    fn known_model_with_zero_tokens_is_not_flagged_missing() {
        let map = synthetic_pricing_map(&[], &[]);
        let tokens = UsageTokens::default();
        let (cost, missing) = cost_for_usage(&map, Some("totally-unknown-model"), &tokens);
        assert_eq!(cost, 0.0);
        assert!(!missing, "zero-token entries are never evidence of missing pricing");
    }

    #[test]
    fn no_model_with_nonzero_tokens_is_flagged_missing() {
        let map = synthetic_pricing_map(&[], &[]);
        let tokens = UsageTokens { input: 5, ..UsageTokens::default() };
        let (cost, missing) = cost_for_usage(&map, None, &tokens);
        assert_eq!(cost, 0.0);
        assert!(missing);
    }

    #[test]
    fn known_model_with_nonzero_tokens_computes_cost_and_is_not_flagged() {
        let pricing = flat_pricing(1.0, 2.0);
        let map = synthetic_pricing_map(&[("known-model", pricing, false)], &[]);
        let tokens = UsageTokens { input: 10, output: 10, ..UsageTokens::default() };
        let (cost, missing) = cost_for_usage(&map, Some("known-model"), &tokens);
        assert_eq!(cost, 10.0 * 1.0 + 10.0 * 2.0);
        assert!(!missing);
    }
}
