//! Model price table — fetches per-token USD rates so the Tokens tab can show
//! API-equivalent cost (what the recorded usage *would* cost at pay-as-you-go
//! API rates; clauth users are on subscription plans, so this reads as "value
//! extracted", not a bill).
//!
//! # Source
//!
//! pydantic's genai-prices v2 dataset (`prices/new_data/v2/data.json` on
//! `main`): provider-generated price pages distilled into one machine-readable
//! file. Each entry is a model with a match clause (which ids route to it) and
//! one or more price entries, optionally constraint-gated by start date or
//! time of day — so dated price cuts and peak/off-peak schedules resolve
//! exactly instead of being approximated by a flat table. Only first-party
//! providers are kept; resellers (OpenRouter, AWS, Azure, Fireworks, Together,
//! Novita, OVH, Hugging Face's hosted providers, …) are dropped, so a bare id
//! never prices through a reseller's markup.
//!
//! # Design (mirrors `status.rs`)
//!
//! TUI-free: owns the data model, the HTTP fetch, the distill step, and the
//! on-disk cache, but never touches ratatui. A background thread cold-loads the
//! disk cache (so cost renders instantly and offline once primed), then fetches
//! the live feed and refreshes on a slow cadence — prices change rarely. The UI
//! thread reads [`PricingEvent`]s and holds the latest [`PriceTable`]; no shared
//! lock crosses the thread boundary, only the channel does.
//!
//! Every successful fetch appends a snapshot to the table's history (skipped
//! when the distilled models are byte-identical to the last snapshot's, capped
//! at [`HISTORY_CAP`] snapshots), so a past day re-prices at the rates live on
//! that day. Snapshot selection for a query date: the newest snapshot with
//! `captured <= date`; a date older than every snapshot uses the oldest one.
//!
//! # Cost basis
//!
//! Cost is computed **per model** and summed — never via a blended rate, since
//! family rates differ up to 10× (Opus $5/$25 vs Haiku $1/$5 per 1M). It always
//! counts cache tokens (they cost real money on the API), independent of the
//! Tokens tab's `count_cache` display toggle. Models with no matching rate
//! (unknown / unpriced providers) contribute nothing and are surfaced as such.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::poll::{first_delay, run_polling_loop};
use crate::profile::{atomic_write_600, clauth_dir};
use crate::tokens::{ModelTokens, today_date};
use crate::usage::now_ms;

#[cfg(test)]
use std::collections::HashMap;

/// Live price feed (genai-prices v2 generated data, fetched from GitHub `main`).
const FEED_URL: &str =
    "https://raw.githubusercontent.com/pydantic/genai-prices/main/prices/new_data/v2/data.json";

/// Background refresh cadence. Prices move rarely, so this is deliberately slow;
/// a manual refresh signal short-circuits the wait.
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// HTTP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// HTTP response-receive timeout.
const RECV_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard cap on the response body. The real feed is ~365 KiB; 8 MiB is generous
/// headroom while still bounding a hostile / runaway response.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// Snapshot history cap: the newest 180 fetches survive, older ones drop.
const HISTORY_CAP: usize = 180;

/// First-party providers distilled into the table. Every other provider id in
/// the feed resells another vendor's models (OpenRouter, AWS Bedrock, Azure,
/// Fireworks, Together, Novita, OVHcloud, Hugging Face's hosted providers,
/// Modal, Avian, Doubleword); keeping them would let a bare id price through a
/// reseller's markup.
const FIRST_PARTY_PROVIDERS: &[&str] = &[
    "anthropic",
    "deepseek",
    "zai",
    "zhipuai",
    "minimax",
    "moonshotai",
    "x-ai",
    "openai",
    "google",
    "mistral",
    "groq",
    "cohere",
    "cerebras",
    "perplexity",
    "voyageai",
];

// ── Data model ──────────────────────────────────────────────────────────────

/// Per-token USD rates for one model — a RESOLVED flat rate (the outcome of
/// picking the active [`PriceEntry`]). `cache_write` is the 5-minute-TTL
/// creation rate (the common case; the 1-hour rate is not modeled — the hourly
/// axis has no TTL data). Missing upstream fields (e.g. a provider with no
/// cache-write rate) default to `0.0`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct ModelRate {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_write: f64,
}

/// When a [`PriceEntry`] applies, mirroring genai-prices' constraints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Constraint {
    /// Active from this calendar date on, inclusive.
    StartDate(String), // "YYYY-MM-DD"
    /// Active inside this daily interval; see [`window_contains`] for the
    /// hour-granularity semantics.
    TimeWindow { start: String, end: String }, // "HH:MM[:SS]Z"
}

/// One price row: the four per-token rates plus an optional constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PriceEntry {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_write: f64,
    pub(crate) constraint: Option<Constraint>,
}

impl PriceEntry {
    /// Whether this entry is the pick at `(date, hour)`: unconstrained entries
    /// are always active; a constraint must hold. A constraint whose time
    /// strings do not parse is never active, so entry selection falls through
    /// to the unconstrained fallback instead of failing the whole table.
    fn active(&self, date: &str, hour: u8) -> bool {
        match &self.constraint {
            None => true,
            Some(Constraint::StartDate(start)) => date >= start.as_str(),
            Some(Constraint::TimeWindow { start, end }) => window_contains(start, end, hour),
        }
    }
}

/// One model's match clause, mirroring genai-prices `MatchLogic`. String
/// patterns are stored LOWERCASED at distill time and evaluated against the
/// lowercased model id — case-insensitive either way, like upstream (which
/// lowercases both sides per call). Regex patterns are stored verbatim and
/// searched against the lowercased id, also like upstream (which lowercases the
/// ref before dispatching to `ClauseRegex`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum MatchClause {
    Equals(String),
    StartsWith(String),
    Contains(String),
    EndsWith(String),
    Regex(String),
    Or(Vec<MatchClause>),
    And(Vec<MatchClause>),
}

impl MatchClause {
    /// Evaluated against the lowercased, bracket-stripped model id.
    fn matches(&self, lowered: &str) -> bool {
        match self {
            Self::Equals(p) => lowered == p,
            Self::StartsWith(p) => lowered.starts_with(p.as_str()),
            Self::Contains(p) => lowered.contains(p.as_str()),
            Self::EndsWith(p) => lowered.ends_with(p.as_str()),
            Self::Regex(pattern) => {
                // Compiled per evaluation: regex clauses are rare (date-stamp
                // variants), so caching buys nothing. An unparseable pattern
                // (which upstream's pydantic validation rejects at build time)
                // simply never matches.
                regex::Regex::new(pattern).is_ok_and(|re| re.is_match(lowered))
            }
            Self::Or(clauses) => clauses.iter().any(|c| c.matches(lowered)),
            Self::And(clauses) => clauses.iter().all(|c| c.matches(lowered)),
        }
    }
}

/// One distilled model: how to recognize it (match clause) plus its price
/// entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PricedModel {
    pub(crate) id: String,
    #[serde(rename = "match")]
    pub(crate) match_: MatchClause,
    pub(crate) prices: Vec<PriceEntry>,
}

/// One fetch's distilled table: the capture date plus the models live then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RateSnapshot {
    /// Capture date as "YYYY-MM-DD".
    pub(crate) captured: String,
    pub(crate) models: Vec<PricedModel>,
}

/// Per-hour token buckets behind [`PriceTable::cost_day`]. Pricing-local for
/// now — slice B moves this onto the hourly axis and re-exports it from
/// `crate::tokens`.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)] // slice B contract: the hourly axis consumes this next
pub(crate) struct HourTokens {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_create: u64,
}

/// Resolved price table: the newest snapshot's models, the full snapshot
/// history (oldest first), and the wall-clock time the feed was fetched (for a
/// freshness badge).
#[derive(Debug, Clone)]
pub(crate) struct PriceTable {
    /// Latest snapshot's models — the working set for "today" queries.
    models: Vec<PricedModel>,
    /// Oldest-first snapshot history; [`PriceTable::snapshot_for`] picks the
    /// one applicable to a query date.
    history: Vec<RateSnapshot>,
    pub(crate) fetched_at_ms: u64,
}

#[cfg(test)]
impl PriceTable {
    /// Literal table for tests outside this module — keeps the internals
    /// private (lookups stay funneled through `rate`/`cost`/`total_cost`).
    /// Each flat rate becomes one unconstrained price entry behind an exact
    /// match, so every date and hour resolves to it.
    pub(crate) fn from_rates(rates: HashMap<String, ModelRate>) -> Self {
        let models = rates
            .into_iter()
            .map(|(id, r)| PricedModel {
                id: id.clone(),
                match_: MatchClause::Equals(id.to_lowercase()),
                prices: vec![PriceEntry {
                    input: r.input,
                    output: r.output,
                    cache_read: r.cache_read,
                    cache_write: r.cache_write,
                    constraint: None,
                }],
            })
            .collect();
        Self::capture(models, today_date(), 0, Vec::new())
    }
}

impl PriceTable {
    /// Fold a successful fetch into a table: stamp `fetched_at_ms`, append a
    /// snapshot dated `captured` ONLY when the distilled models differ from the
    /// last snapshot's (serialize-and-compare — a byte-identical refetch does
    /// not grow the history), and cap the history at [`HISTORY_CAP`], dropping
    /// the oldest snapshots.
    pub(crate) fn capture(
        models: Vec<PricedModel>,
        captured: String,
        fetched_at_ms: u64,
        mut history: Vec<RateSnapshot>,
    ) -> Self {
        // Serialization of these types cannot fail; on the off chance it does,
        // treating the table as changed only appends one extra snapshot.
        let changed = match serde_json::to_string(&models).ok().zip(
            history
                .last()
                .and_then(|s| serde_json::to_string(&s.models).ok()),
        ) {
            Some((fresh, last)) => fresh != last,
            None => true,
        };
        if changed {
            history.push(RateSnapshot {
                captured,
                models: models.clone(),
            });
            if history.len() > HISTORY_CAP {
                history.drain(..history.len() - HISTORY_CAP);
            }
        }
        Self {
            models,
            history,
            fetched_at_ms,
        }
    }

    /// Rate for a model id at `(date, hour)`, mirroring the upstream reference
    /// resolver (`genai_prices/types.py`):
    ///
    /// 1. The id is bracket-stripped (a trailing `[<digits>k|m]` context
    ///    suffix, case-insensitive) and lowercased.
    /// 2. The first [`PricedModel`] (in distilled order) whose match clause
    ///    holds wins — upstream's `is_match` on the lowercased ref.
    /// 3. Its entries are tried in REVERSE order; the first whose constraint
    ///    is `None` or active is the pick, falling back to `prices[0]` when
    ///    nothing matches.
    ///
    /// Rates come from the snapshot live on `date` (see
    /// [`PriceTable::snapshot_for`]); `None` when no model matches.
    pub(crate) fn rate_at(&self, model: &str, date: &str, hour: u8) -> Option<ModelRate> {
        let lowered = strip_bracket_suffix(model).to_lowercase();
        let priced = self
            .models_for(date)?
            .iter()
            .find(|m| m.match_.matches(&lowered))?;
        let entry = priced
            .prices
            .iter()
            .rev()
            .find(|e| e.active(date, hour))
            .or_else(|| priced.prices.first())?;
        Some(ModelRate {
            input: entry.input,
            output: entry.output,
            cache_read: entry.cache_read,
            cache_write: entry.cache_write,
        })
    }

    /// The models applicable to `date`: the newest snapshot with `captured <=
    /// date` (served straight from `models`, the newest snapshot's working
    /// set); a date older than every snapshot uses the oldest one.
    fn models_for(&self, date: &str) -> Option<&[PricedModel]> {
        if self
            .history
            .last()
            .is_some_and(|s| s.captured.as_str() <= date)
        {
            return Some(&self.models);
        }
        self.history
            .iter()
            .rev()
            .find(|s| s.captured.as_str() <= date)
            .map(|s| s.models.as_slice())
            .or_else(|| self.history.first().map(|s| s.models.as_slice()))
    }

    /// API-equivalent cost in USD for one model's recorded tokens at
    /// `(date, hour)`. `None` when no rate matches (unknown / unpriced model).
    /// Counts all four token buckets.
    pub(crate) fn cost_at(&self, m: &ModelTokens, date: &str, hour: u8) -> Option<f64> {
        let r = self.rate_at(&m.model, date, hour)?;
        Some(
            m.input as f64 * r.input
                + m.output as f64 * r.output
                + m.cache_read as f64 * r.cache_read
                + m.cache_create as f64 * r.cache_write,
        )
    }

    /// Cost of one model across a full day of hourly token buckets, pricing
    /// each hour at its own `(date, hour)` rate (peak/off-peak). `None` when
    /// the model has no matching rate — a model's match clause is
    /// time-independent, so a match at hour 0 guarantees one at every hour.
    #[allow(dead_code)] // slice B contract: the hourly axis calls this next
    pub(crate) fn cost_day(
        &self,
        model: &str,
        date: &str,
        hours: &[HourTokens; 24],
    ) -> Option<f64> {
        let mut total = 0.0;
        for (hour, h) in hours.iter().enumerate() {
            let r = self.rate_at(model, date, hour as u8)?;
            total += h.input as f64 * r.input
                + h.output as f64 * r.output
                + h.cache_read as f64 * r.cache_read
                + h.cache_create as f64 * r.cache_write;
        }
        Some(total)
    }

    // ── Migration adapters (removed in slice C) ─────────────────────────────
    // Flat "today" lookups: the Tokens tab and sessions surface still call
    // these; slice C wires their callers to the dated API directly.

    /// Today's rate at hour 0. See [`PriceTable::rate_at`].
    pub(crate) fn rate(&self, model: &str) -> Option<ModelRate> {
        self.rate_at(model, &today_date(), 0)
    }

    /// Today's cost at hour 0. See [`PriceTable::cost_at`].
    pub(crate) fn cost(&self, m: &ModelTokens) -> Option<f64> {
        self.cost_at(m, &today_date(), 0)
    }

    /// Summed cost over a slice of models. Returns `(priced_total_usd,
    /// unpriced_count)` — `unpriced_count` is how many had nonzero tokens but no
    /// matching rate, so the UI can flag that the figure is a floor.
    pub(crate) fn total_cost(&self, models: &[ModelTokens]) -> (f64, usize) {
        let mut total = 0.0;
        let mut unpriced = 0usize;
        for m in models {
            match self.cost(m) {
                Some(c) => total += c,
                None if m.total() > 0 => unpriced += 1,
                None => {}
            }
        }
        (total, unpriced)
    }
}

// ── Resolution helpers ──────────────────────────────────────────────────────

/// Strip a trailing `[<digits>k|m]` context suffix (case-insensitive):
/// `deepseek-v4-pro[1m]` → `deepseek-v4-pro`. Anything else — no closing
/// bracket, no digits, an unknown unit letter — is left alone, so an id with a
/// bracketed segment that is NOT a context suffix still matches its clauses on
/// the full string.
fn strip_bracket_suffix(id: &str) -> &str {
    let Some(body) = id.strip_suffix(']') else {
        return id;
    };
    let Some((head, unit)) = body.rsplit_once('[') else {
        return id;
    };
    let Some(digits) = unit.strip_suffix(['k', 'K', 'm', 'M']) else {
        return id;
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return id;
    }
    head
}

/// Half-open daily window at HOUR granularity: active when
/// `(start_h, start_m) <= (hour, 0) < (end_h, end_m)` — the upstream
/// `TimeOfDateConstraint.active` sampled at the hour's start. Exact for
/// whole-hour windows; the deepseek-chat/reasoner `:30` boundaries are
/// half-mispriced by construction (hour 00 prices the whole hour off-peak
/// though its second half is peak, hour 16 the whole hour peak though its
/// second half is off-peak), accepted per the settled design. Unparseable
/// times make the window never active.
fn window_contains(start: &str, end: &str, hour: u8) -> bool {
    let Some((sh, sm)) = parse_hhmm(start) else {
        return false;
    };
    let Some((eh, em)) = parse_hhmm(end) else {
        return false;
    };
    (sh, sm) <= (hour, 0) && (hour, 0) < (eh, em)
}

/// Parse the `HH:MM` prefix of a `"HH:MM[:SS]Z"` string (seconds optional).
fn parse_hhmm(s: &str) -> Option<(u8, u8)> {
    let (hh, mm) = s.split_once(':')?;
    Some((hh.parse().ok()?, mm.get(..2)?.parse().ok()?))
}

// ── Background thread ────────────────────────────────────────────────────────

/// Events emitted by the background pricing worker.
pub(crate) enum PricingEvent {
    /// A fresh or cached table is available.
    Loaded(Box<PriceTable>),
    /// A fetch failed and no cache was available. UI keeps showing `—`.
    Failed,
}

/// Spawn the pricing worker. On start it cold-loads the disk cache (so cost
/// renders instantly and offline once primed), then fetches the live feed once
/// the cache has aged past the cadence and loops on it — the 24h table survives a
/// relaunch instead of being re-downloaded; a `()` on `refresh_rx` triggers an
/// immediate refetch. Exits when the refresh channel disconnects (TUI shutdown).
///
/// Mirrors `status::spawn`: a plain `std::thread`, a ureq agent with short
/// timeouts, and the cache path resolved on the calling thread before detaching
/// (so the worker never re-resolves `home_dir()`, which would race a test's
/// `HOME_OVERRIDE`).
pub(crate) fn spawn(tx: Sender<PricingEvent>, refresh_rx: Receiver<()>) {
    let Some(cache_file) = cache_path() else {
        return;
    };
    std::thread::spawn(move || {
        // Cold-fill from cache first so the first paint can price immediately.
        let mut cached_at_ms = None;
        if let Some(table) = load_cache(&cache_file) {
            cached_at_ms = Some(table.fetched_at_ms);
            let _ = tx.send(PricingEvent::Loaded(Box::new(table)));
        }

        let first = first_delay(cached_at_ms, now_ms(), REFRESH_INTERVAL);
        let mut stale_cleaned = false;
        run_polling_loop(&refresh_rx, first, REFRESH_INTERVAL, || {
            run_fetch(&tx, &cache_file, &mut stale_cleaned)
        });
    });
}

/// One fetch attempt. On success: distill, fold into the snapshot history,
/// cache, send `Loaded`. On failure: fall back to the cache when one exists
/// (`Loaded`); only when nothing is cached do we surface `Failed`.
fn run_fetch(tx: &Sender<PricingEvent>, cache_file: &Path, stale_cleaned: &mut bool) {
    match fetch_models() {
        Ok(models) => {
            let history = load_cache(cache_file)
                .map(|t| t.history)
                .unwrap_or_default();
            let table = PriceTable::capture(models, today_date(), now_ms(), history);
            save_cache(cache_file, &table);
            delete_stale_cache_once(cache_file, stale_cleaned);
            let _ = tx.send(PricingEvent::Loaded(Box::new(table)));
        }
        Err(_) => match load_cache(cache_file) {
            Some(table) => {
                let _ = tx.send(PricingEvent::Loaded(Box::new(table)));
            }
            None => {
                let _ = tx.send(PricingEvent::Failed);
            }
        },
    }
}

/// One-time best-effort removal of the LiteLLM-era `price_cache.json`, run
/// after the first successful save of the new cache. The new table never reads
/// the old file, so this is pure cleanup; errors (including NotFound, the
/// expected steady state) are ignored on purpose. The flag is set BEFORE the
/// delete so a reappearing file is never re-deleted.
fn delete_stale_cache_once(cache_file: &Path, done: &mut bool) {
    if *done {
        return;
    }
    *done = true;
    if let Some(stale) = cache_file.parent().map(|d| d.join("price_cache.json")) {
        let _ = std::fs::remove_file(stale);
    }
}

/// Fetch and distill the live feed. The body is capped at [`MAX_BODY_BYTES`].
fn fetch_models() -> anyhow::Result<Vec<PricedModel>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RECV_TIMEOUT))
        .build()
        .into();

    let reader = agent
        .get(FEED_URL)
        .header("User-Agent", "clauth-pricing")
        .call()
        .map_err(anyhow::Error::from)?
        .into_body()
        .into_reader();
    // +1 so a body exactly at the cap still trips the over-limit check.
    let mut capped = reader.take(MAX_BODY_BYTES + 1);

    let mut bytes = Vec::new();
    capped
        .read_to_end(&mut bytes)
        .map_err(anyhow::Error::from)?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        anyhow::bail!("price feed exceeded {MAX_BODY_BYTES} byte cap");
    }
    let json = String::from_utf8(bytes).map_err(anyhow::Error::from)?;
    distill(&json)
}

// ── Distill ──────────────────────────────────────────────────────────────────

/// Parse the genai-prices v2 JSON into distilled [`PricedModel`]s. Tolerant at
/// every level: malformed providers, models, and price entries are skipped; the
/// fetch fails only when ZERO models survive (an empty table would price
/// nothing and look like a healthy load). Only [`FIRST_PARTY_PROVIDERS`] are
/// kept — resellers are dropped here, so no lookup path can land on a
/// reseller's markup.
fn distill(json: &str) -> anyhow::Result<Vec<PricedModel>> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(anyhow::Error::from)?;
    let providers = root
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("price feed root is not a JSON array"))?;

    let mut models = Vec::new();
    for provider in providers {
        let Ok(provider) = serde_json::from_value::<RawProvider>(provider.clone()) else {
            continue; // malformed provider — skip, don't fail the feed
        };
        if !FIRST_PARTY_PROVIDERS.contains(&provider.id.as_str()) {
            continue;
        }
        for model in provider.models {
            let Ok(model) = serde_json::from_value::<RawModel>(model) else {
                continue; // malformed model — skip, don't fail the provider
            };
            if let Some(priced) = model.into_priced() {
                models.push(priced);
            }
        }
    }
    if models.is_empty() {
        anyhow::bail!("price feed distilled to zero priced models");
    }
    Ok(models)
}

/// Feed-level provider row. `models` stays raw JSON so one malformed model
/// skips itself instead of sinking its whole provider. The feed's
/// `fallback_model_providers` chain is intentionally NOT modeled: it exists
/// only on azure (a dropped reseller) and google (→ anthropic), and the flat
/// cross-provider distilled list already scans every kept provider's models,
/// so the chain is structurally redundant — upstream uses it for provider
/// attribution, which clauth does not track.
#[derive(Deserialize)]
struct RawProvider {
    id: String,
    #[serde(default)]
    models: Vec<serde_json::Value>,
}

/// One model row. Every field the resolver needs is required; a model missing
/// any of them is skipped by the caller.
#[derive(Deserialize)]
struct RawModel {
    id: String,
    #[serde(rename = "match")]
    match_: RawClause,
    prices: RawPrices,
}

/// The two serializations of `prices`: a bare object for flat models, an array
/// of conditional entries for constrained ones. The array stays raw JSON so one
/// malformed entry skips itself instead of sinking its whole model.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawPrices {
    Flat(RawPriceSet),
    Conditional(Vec<serde_json::Value>),
}

#[derive(Deserialize)]
struct RawConditional {
    #[serde(default)]
    constraint: Option<RawConstraint>,
    prices: RawPriceSet,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawConstraint {
    StartDate {
        start_date: String,
    },
    TimeWindow {
        start_time: String,
        end_time: String,
    },
}

/// The four token rates; a missing/null field is `None` (→ 0.0). A field of
/// any other shape fails the whole model, which the caller then skips.
#[derive(Deserialize)]
struct RawPriceSet {
    #[serde(default)]
    input_mtok: Option<RawPrice>,
    #[serde(default)]
    output_mtok: Option<RawPrice>,
    #[serde(default)]
    cache_read_mtok: Option<RawPrice>,
    #[serde(default)]
    cache_write_mtok: Option<RawPrice>,
}

/// `_mtok` fields are USD per million tokens — either a flat number or a
/// `{base, tiers}` ladder. clauth has no per-request context-window input, so a
/// tiered field resolves to its `base` (the rate below the tier threshold;
/// documented approximation).
#[derive(Deserialize)]
#[serde(untagged)]
enum RawPrice {
    Number(f64),
    Tiered { base: f64 },
}

/// The match logic; variants mirror the schema. `or`/`and` nest arbitrarily.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawClause {
    Equals { equals: String },
    StartsWith { starts_with: String },
    Contains { contains: String },
    EndsWith { ends_with: String },
    Regex { regex: String },
    Or { or: Vec<RawClause> },
    And { and: Vec<RawClause> },
}

impl RawModel {
    fn into_priced(self) -> Option<PricedModel> {
        let prices = match self.prices {
            RawPrices::Flat(set) => vec![set.into_entry(None)],
            RawPrices::Conditional(entries) => entries
                .into_iter()
                .filter_map(|e| serde_json::from_value::<RawConditional>(e).ok())
                .map(|e| {
                    e.prices
                        .into_entry(e.constraint.map(RawConstraint::into_constraint))
                })
                .collect(),
        };
        // A model with no input AND no output rate anywhere (per-request /
        // web-search pricing) cannot price tokens; keeping it would render a
        // $0 "priced" row instead of an unpriced dash.
        if !prices.iter().any(|e| e.input != 0.0 || e.output != 0.0) {
            return None;
        }
        Some(PricedModel {
            id: self.id,
            match_: self.match_.into_clause(),
            prices,
        })
    }
}

impl RawPriceSet {
    fn into_entry(self, constraint: Option<Constraint>) -> PriceEntry {
        PriceEntry {
            input: to_per_token(self.input_mtok),
            output: to_per_token(self.output_mtok),
            cache_read: to_per_token(self.cache_read_mtok),
            cache_write: to_per_token(self.cache_write_mtok),
            constraint,
        }
    }
}

/// USD-per-million → per-token.
fn to_per_token(price: Option<RawPrice>) -> f64 {
    match price {
        Some(RawPrice::Number(mtok)) => mtok / 1e6,
        Some(RawPrice::Tiered { base }) => base / 1e6,
        None => 0.0,
    }
}

impl RawClause {
    /// String patterns are lowercased here so evaluation stays allocation-free
    /// and case-insensitive (upstream lowercases per call; storing pre-lowered
    /// is equivalent).
    fn into_clause(self) -> MatchClause {
        match self {
            Self::Equals { equals } => MatchClause::Equals(equals.to_lowercase()),
            Self::StartsWith { starts_with } => MatchClause::StartsWith(starts_with.to_lowercase()),
            Self::Contains { contains } => MatchClause::Contains(contains.to_lowercase()),
            Self::EndsWith { ends_with } => MatchClause::EndsWith(ends_with.to_lowercase()),
            Self::Regex { regex } => MatchClause::Regex(regex),
            Self::Or { or } => MatchClause::Or(or.into_iter().map(Self::into_clause).collect()),
            Self::And { and } => MatchClause::And(and.into_iter().map(Self::into_clause).collect()),
        }
    }
}

impl RawConstraint {
    fn into_constraint(self) -> Constraint {
        match self {
            Self::StartDate { start_date } => Constraint::StartDate(start_date),
            Self::TimeWindow {
                start_time,
                end_time,
            } => Constraint::TimeWindow {
                start: start_time,
                end: end_time,
            },
        }
    }
}

// ── Disk cache ───────────────────────────────────────────────────────────────

/// On-disk cache shape: the fetch time plus the snapshot history.
#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    fetched_at_ms: u64,
    #[serde(default)]
    history: Vec<RateSnapshot>,
}

/// `~/.clauth/genai_price_cache.json`. Resolved ONCE at spawn time and passed
/// into the worker so the detached thread never re-resolves `home_dir()` later.
fn cache_path() -> Option<PathBuf> {
    clauth_dir().ok().map(|d| d.join("genai_price_cache.json"))
}

/// Synchronous one-shot read of the on-disk price cache, off the background
/// [`spawn`] channel — the CLI sessions surface needs a `PriceTable` on the main
/// thread without standing up the worker. `None` when the cache is absent or
/// unparseable; never fetches, so a cold cache simply prices nothing.
pub(crate) fn load_cached() -> Option<PriceTable> {
    load_cache(&cache_path()?)
}

/// Load the cache if it exists and parses; `None` on any miss/error (a stale or
/// reshaped cache is silently treated as no cache). A cache with an empty
/// snapshot history is also rejected — without at least one snapshot nothing
/// can resolve.
fn load_cache(path: &Path) -> Option<PriceTable> {
    let bytes = std::fs::read_to_string(path).ok()?;
    let cache: CacheFile = serde_json::from_str(&bytes).ok()?;
    let models = cache.history.last()?.models.clone();
    Some(PriceTable {
        models,
        history: cache.history,
        fetched_at_ms: cache.fetched_at_ms,
    })
}

/// Persist the cache best-effort (atomic tmp + rename). Errors are swallowed.
fn save_cache(path: &Path, table: &PriceTable) {
    let cache = CacheFile {
        fetched_at_ms: table.fetched_at_ms,
        history: table.history.clone(),
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = atomic_write_600(path, json);
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../tests/inline/pricing.rs"]
mod tests;
