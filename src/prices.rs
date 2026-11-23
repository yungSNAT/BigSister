//! Published prices for uncached input: a table bundled with the release, or a price
//! file the user gives with `--prices`. Neither makes a network call.
//!
//! A price file is a JSON object keyed by the exact model id a transcript records:
//! Claude Code's `message.model`, Codex's `turn_context` `model`. Each value is an object
//! whose `input_cost_per_token` is the price of one uncached input token in US dollars,
//! and whose `input_cost_per_token_above_<N>k_tokens` fields, if any, are the prices for
//! a prompt of more than N thousand tokens. Other fields are ignored, and a model without
//! `input_cost_per_token` is unpriced. These are the names LiteLLM's
//! `model_prices_and_context_window.json` uses, so that file can be given as it is.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use serde_json::value::RawValue;

/// When the bundled prices were read from Anthropic's and OpenAI's pricing pages.
pub const READ: &str = "24 November 2026";

/// The bundled prices, a price file. Read from
/// <https://platform.claude.com/docs/en/about-claude/pricing>, for each model
/// <https://platform.claude.com/docs/en/about-claude/model-deprecations> lists as active
/// under its API id, and <https://developers.openai.com/api/docs/pricing> (standard,
/// short context, and long context above 272,000 input tokens), with <https://developers.openai.com/api/docs/models/gpt-5.3-codex> for
/// `gpt-5.3-codex`, which that page no longer lists.
const BUNDLED: &str = include_str!("prices.json");

/// The note of each section whose estimates are priced at each model's price, from the
/// bundled prices. [`Prices::note`] gives the note for the prices in use.
pub const NOTE: &str = "Dollars (usd est) price the characters of each estimate, divided by 4, once as uncached input at the published price of the model whose reply read them, from prices bundled with this release and read on 24 November 2026: an illustration, not a bill. Cached re-sends cost less and text kept in context costs again in later turns. Unpriced tokens est are those read by a model without a bundled price, by a model priced by prompt size whose prompt size is not recorded, or by no reply, and add no dollars.";

/// One model's price for uncached input, in US dollars per million tokens.
#[derive(Debug, PartialEq)]
struct Price {
    usd_per_mtok: f64,
    /// For a prompt of more than so many tokens, the price instead, fewest tokens first.
    above: Vec<(u64, f64)>,
}

/// The prices in use: the bundled ones, or a price file's.
#[derive(Debug)]
pub struct Prices {
    models: HashMap<String, Price>,
    /// The price file as the report shows it, when one was given.
    file: Option<String>,
}

impl Prices {
    /// The prices bundled with this release.
    ///
    /// # Panics
    /// Never: a test parses the bundled file.
    #[must_use]
    pub fn bundled() -> Self {
        Self {
            models: parse(BUNDLED).expect("the bundled prices parse"),
            file: None,
        }
    }

    /// The prices of the price file whose text is `text`, shown in the report as `shown`.
    ///
    /// # Errors
    /// Returns a message naming the file and the first entry that is not a price, as
    /// [`parse`] finds it.
    pub fn from_file(text: &str, shown: &str) -> Result<Self, String> {
        let models = parse(text).map_err(|e| format!("price file {shown}: {e}"))?;
        Ok(Self {
            models,
            file: Some(shown.to_owned()),
        })
    }

    /// The price of `model`'s uncached input, if it has one. `prompt_tokens` gives the
    /// size of the reading request's prompt; it is asked for only by a model whose price
    /// depends on it, which stays unpriced when the size is not known.
    #[must_use]
    pub fn usd_per_mtok(
        &self,
        model: &str,
        prompt_tokens: impl FnOnce() -> Option<u64>,
    ) -> Option<f64> {
        let price = self.models.get(model)?;
        if price.above.is_empty() {
            return Some(price.usd_per_mtok);
        }
        let tokens = prompt_tokens()?;
        let tier = price.above.iter().rev().find(|(most, _)| tokens > *most);
        Some(tier.map_or(price.usd_per_mtok, |&(_, usd)| usd))
    }

    /// The price of `model`'s uncached input for the shortest prompt, if it has one: a
    /// price that does not move with prompt size, to compare models by.
    #[must_use]
    pub fn base_usd_per_mtok(&self, model: &str) -> Option<f64> {
        self.models.get(model).map(|p| p.usd_per_mtok)
    }

    /// The price file as the report shows it, when one was given.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    /// The note of each section whose estimates are priced at each model's price.
    #[must_use]
    pub fn note(&self) -> String {
        match &self.file {
            None => NOTE.to_owned(),
            Some(file) => format!(
                "Dollars (usd est) price the characters of each estimate, divided by 4, once as uncached input at the price of the model whose reply read them in the price file {file}, given with --prices: an illustration, not a bill. Cached re-sends cost less and text kept in context costs again in later turns. Unpriced tokens est are those read by a model the file gives no price, by a model priced by prompt size whose prompt size is not recorded, or by no reply, and add no dollars."
            ),
        }
    }
}

/// The field holding the price of one uncached input token.
const BASE: &str = "input_cost_per_token";

/// The models a price file prices.
///
/// # Errors
/// Fails on text that is not a JSON object or has an object with a key twice (which JSON
/// readers settle in different ways), an entry that is not an object, a price that
/// is not a number of at least 0, that is too large to multiply by a million or that is
/// above 0 but too small for an f64 to hold whole (below about 2.2e-308), a tier
/// field not written as `input_cost_per_token_above_<N>k_tokens` with N a whole number
/// without leading zeros
/// and a tier without `input_cost_per_token`, and when no model has a price at all.
fn parse(text: &str) -> Result<HashMap<String, Price>, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    serde_json::from_str::<UniqueKeys>(text).map_err(|e| e.to_string())?;
    let entries = doc
        .as_object()
        .ok_or("not a JSON object of model ids and their prices")?;
    // Each entry's text as the file writes it, which keeps a price's digits whole.
    let written: HashMap<String, Box<RawValue>> =
        serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut models = HashMap::new();
    for (model, entry) in entries {
        if let Some(price) = price(model, entry, &written[model])? {
            models.insert(model.clone(), price);
        }
    }
    if models.is_empty() {
        return Err(format!("no model has an {BASE}"));
    }
    Ok(models)
}

/// The price in `model`'s entry, written in the file as `written`, or `None` when it has
/// no `input_cost_per_token`.
fn price(model: &str, entry: &Value, written: &RawValue) -> Result<Option<Price>, String> {
    let entry = entry
        .as_object()
        .ok_or_else(|| format!("{model}: not a JSON object"))?;
    let written: HashMap<String, Box<RawValue>> =
        serde_json::from_str(written.get()).map_err(|e| e.to_string())?;
    let mut base = None;
    let mut above = Vec::new();
    for (field, value) in entry {
        let most = if field == BASE {
            None
        } else if let Some(n) = field
            .strip_prefix("input_cost_per_token_above_")
            .and_then(|rest| rest.strip_suffix("_tokens"))
        {
            Some(thousands(n).ok_or_else(|| {
                format!("{model}: {field}: a tier is written input_cost_per_token_above_<N>k_tokens, N a whole number")
            })?)
        } else {
            continue;
        };
        // The decimal the file gives, shifted 6 places: `1e-7 * 1e6` would be
        // 0.09999999999999999, a cent short on a 150,000-token estimate.
        let usd = value
            .as_f64()
            .and_then(|usd| format!("{usd}e6").parse::<f64>().ok())
            .filter(|usd| usd.is_finite() && *usd >= 0.0)
            .ok_or_else(|| format!("{model}: {field}: not a number of at least 0"))?;
        if too_small(written[field].get()) {
            return Err(format!(
                "{model}: {field}: a price above 0 but too small to tell from 0 or from other such prices"
            ));
        }
        match most {
            None => base = Some(usd),
            Some(most) => above.push((most, usd)),
        }
    }
    match base {
        Some(usd_per_mtok) => {
            above.sort_by_key(|&(most, _)| most);
            Ok(Some(Price {
                usd_per_mtok,
                above,
            }))
        }
        None if above.is_empty() => Ok(None),
        None => Err(format!("{model}: a tier but no {BASE}")),
    }
}

/// Whether JSON number `written` is above 0 but below the smallest normal f64, which
/// reads as 0 or loses digits, so that 2 such prices could read as one.
fn too_small(written: &str) -> bool {
    let digits = written.split(['e', 'E']).next().unwrap_or("");
    digits.bytes().any(|b| matches!(b, b'1'..=b'9'))
        && written
            .parse::<f64>()
            .is_ok_and(|n| n.abs() < f64::MIN_POSITIVE)
}

/// A JSON value none of whose objects repeats a key.
struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(UniqueKeys)
    }
}

impl<'de> Visitor<'de> for UniqueKeys {
    type Value = UniqueKeys;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!(
                    "{key} appears twice in one object"
                )));
            }
            map.next_value::<UniqueKeys>()?;
        }
        Ok(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
        while seq.next_element::<UniqueKeys>()?.is_some() {}
        Ok(self)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_unit<E>(self) -> Result<Self, E> {
        Ok(self)
    }
}

/// `N` of a tier's `<N>k`, in tokens.
fn thousands(n: &str) -> Option<u64> {
    let digits = n.strip_suffix('k')?;
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(1000)
}

/// The priced part of a row's characters.
#[derive(Default, Clone, Copy, Debug)]
pub struct Priced {
    /// Characters read by a model with a price.
    chars: u64,
    /// Those characters times their model's price per million tokens.
    chars_usd: f64,
}

impl Priced {
    /// Adds `chars` characters read by a model priced at `usd_per_mtok`, if any.
    pub fn add(&mut self, chars: u64, usd_per_mtok: Option<f64>) {
        if let Some(usd) = usd_per_mtok {
            self.chars += chars;
            self.chars_usd += chars as f64 * usd;
        }
    }

    /// The dollars, at 4 characters a token, rounded to cents.
    #[must_use]
    pub fn usd(&self) -> f64 {
        (self.chars_usd / 4.0 / 1e4).round() / 100.0
    }

    /// The part of `est_tokens(total)` not priced, where `total` holds these characters.
    #[must_use]
    pub fn unpriced_tokens(&self, total: u64) -> u64 {
        crate::detectors::est_tokens(total).saturating_sub(crate::detectors::est_tokens(self.chars))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A price in dollars per million tokens.
    fn flat(prices: &Prices, model: &str) -> Option<f64> {
        prices.usd_per_mtok(model, || panic!("{model} is not priced by prompt size"))
    }

    /// The key check takes every value JSON has; given another kind, such as bytes from a
    /// non-JSON reader, it says what it expects.
    #[test]
    fn the_key_check_names_what_it_expects() {
        let bytes = de::value::BytesDeserializer::<de::value::Error>::new(b"x");
        let err = UniqueKeys::deserialize(bytes)
            .err()
            .expect("bytes are refused");
        assert_eq!(
            err.to_string(),
            "invalid type: byte array, expected a JSON value"
        );
    }

    #[test]
    fn only_listed_model_ids_have_a_price() {
        let p = Prices::bundled();
        assert_eq!(flat(&p, "claude-opus-5-5"), Some(4.0));
        assert_eq!(flat(&p, "claude-opus-4-8"), Some(5.0));
        assert_eq!(flat(&p, "claude-opus-4-5-20251101"), Some(5.0));
        assert_eq!(flat(&p, "claude-haiku-4-5-20251001"), Some(1.0));
        // GPT-6 models are priced by prompt size; these prompts are in the short tier.
        assert_eq!(p.usd_per_mtok("gpt-6.1-sol", || Some(272_000)), Some(2.0));
        assert_eq!(p.usd_per_mtok("gpt-6-luna", || Some(1)), Some(0.1));
        // GPT-6.2's long-context price applies to the whole request above 272,000 tokens.
        assert_eq!(p.usd_per_mtok("gpt-6.2-astra", || Some(272_000)), Some(8.0));
        assert_eq!(
            p.usd_per_mtok("gpt-6.2-astra", || Some(272_001)),
            Some(16.0)
        );
        assert_eq!(p.usd_per_mtok("gpt-6.2-sol", || Some(272_000)), Some(2.0));
        assert_eq!(p.usd_per_mtok("gpt-6.2-sol", || Some(272_001)), Some(4.0));
        assert_eq!(flat(&p, "gpt-5.3-codex"), Some(1.75));
        for unpriced in [
            "claude-opus-4-5",
            "claude-opus-4-1-20250805",
            "gpt-6-pro",
            "<synthetic>",
            "claude-opus-5-5-x",
            "",
            "GPT-6.1-SOL",
        ] {
            assert_eq!(flat(&p, unpriced), None, "{unpriced}");
        }
    }

    #[test]
    fn the_bundled_file_prices_every_listed_model() {
        let p = Prices::bundled();
        assert_eq!(p.models.len(), 29);
        let mut tiered: Vec<&str> = p
            .models
            .iter()
            .filter(|(_, price)| !price.above.is_empty())
            .map(|(id, _)| id.as_str())
            .collect();
        tiered.sort_unstable();
        assert_eq!(
            tiered,
            [
                "claude-haiku-5-5",
                "gpt-5.6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-6-astra",
                "gpt-6-luna",
                "gpt-6-sol",
                "gpt-6.1-sol",
                "gpt-6.1-terra",
                "gpt-6.2-astra",
                "gpt-6.2-sol"
            ]
        );
        // OpenAI's long context doubles each GPT-6, GPT-6.2 and GPT-5.6 input price.
        for (id, price) in p.models.iter().filter(|(id, _)| id.starts_with("gpt-")) {
            if id != "gpt-5.3-codex" {
                assert_eq!(price.above, [(272_000, 2.0 * price.usd_per_mtok)], "{id}");
            }
        }
        assert!(p.models.values().all(|price| price.usd_per_mtok > 0.0));
        assert_eq!(p.note(), NOTE);
    }

    #[test]
    fn haiku_5_5_is_priced_by_the_prompt_that_read_it() {
        let p = Prices::bundled();
        let haiku = |tokens| p.usd_per_mtok("claude-haiku-5-5", || tokens);
        assert_eq!(haiku(Some(0)), Some(0.10));
        assert_eq!(haiku(Some(100_000)), Some(0.10));
        assert_eq!(haiku(Some(100_001)), Some(0.50));
        assert_eq!(haiku(None), None);
    }

    #[test]
    fn a_price_file_replaces_the_bundled_prices() {
        let p = Prices::from_file(
            r#"{"sample_spec": {"input_cost_per_token": 0.0, "mode": "chat"},
                "new-model": {"input_cost_per_token": 3e-6, "output_cost_per_token": 1e-5,
                              "input_cost_per_token_batches": 1e-6},
                "image-model": {"output_cost_per_image": 0.04},
                "tiered": {"input_cost_per_token": 1e-6,
                           "input_cost_per_token_above_200k_tokens": 2e-6,
                           "input_cost_per_token_above_32k_tokens": 1.5e-6,
                           "input_cost_per_token_above_200k_tokens_batches": 9.0}}"#,
            "~/prices.json",
        )
        .expect("prices");
        assert_eq!(flat(&p, "new-model"), Some(3.0));
        assert_eq!(flat(&p, "sample_spec"), Some(0.0));
        assert_eq!(flat(&p, "image-model"), None);
        assert_eq!(flat(&p, "claude-opus-5-5"), None);
        let tiered = |tokens| p.usd_per_mtok("tiered", || Some(tokens));
        assert_eq!(tiered(32_000), Some(1.0));
        assert_eq!(tiered(32_001), Some(1.5));
        assert_eq!(tiered(200_000), Some(1.5));
        assert_eq!(tiered(200_001), Some(2.0));
        let note = p.note();
        assert!(note.contains("price file ~/prices.json"), "{note}");
        assert!(!note.contains("bundled"), "{note}");
    }

    /// Malformed price files and the error each must give.
    const MALFORMED: [(&str, &str); 22] = [
        ("", "not valid JSON"),
        ("{\"m\": {\"input_cost_per_token\": 1e-6}", "not valid JSON"),
        ("[]", "not a JSON object of model ids"),
        ("{\"m\": 1e-6}", "m: not a JSON object"),
        (
            "{\"m\": {\"input_cost_per_token\": \"1e-6\"}}",
            "m: input_cost_per_token: not a number of at least 0",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": -1e-6}}",
            "m: input_cost_per_token: not a number of at least 0",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": null}}",
            "m: input_cost_per_token: not a number of at least 0",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e303}}",
            "m: input_cost_per_token: not a number of at least 0",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-324}}",
            "m: input_cost_per_token: a price above 0 but too small to tell from 0",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6, \"input_cost_per_token_above_200k_tokens\": 0.00001e-310}}",
            "m: input_cost_per_token_above_200k_tokens: a price above 0 but too small",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 2.5E-320}}",
            "m: input_cost_per_token: a price above 0 but too small",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6, \"input_cost_per_token_above_1m_tokens\": 2e-6}}",
            "m: input_cost_per_token_above_1m_tokens: a tier is written",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6, \"input_cost_per_token_above_k_tokens\": 2e-6}}",
            "a tier is written",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6, \"input_cost_per_token_above_+5k_tokens\": 2e-6}}",
            "a tier is written",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6, \"input_cost_per_token_above_99999999999999999k_tokens\": 2e-6}}",
            "a tier is written",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6, \"input_cost_per_token_above_1k_tokens\": 2e-6, \"input_cost_per_token_above_01k_tokens\": 3e-6}}",
            "m: input_cost_per_token_above_01k_tokens: a tier is written",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6, \"input_cost_per_token_above_200k_tokens\": true}}",
            "m: input_cost_per_token_above_200k_tokens: not a number",
        ),
        (
            "{\"m\": {\"input_cost_per_token_above_200k_tokens\": 2e-6}}",
            "m: a tier but no input_cost_per_token",
        ),
        ("{}", "no model has an input_cost_per_token"),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6}, \"m\": {\"input_cost_per_token\": 2e-6}}",
            "m appears twice in one object at line 1",
        ),
        (
            "{\"m\": {\"input_cost_per_token\": 1e-6, \"input_cost_per_token\": 2e-6}}",
            "input_cost_per_token appears twice in one object",
        ),
        (
            "{\"m\": {\"output_cost_per_token\": 1e-6}}",
            "no model has an input_cost_per_token",
        ),
    ];

    #[test]
    fn a_malformed_price_file_fails_naming_what_is_wrong() {
        for (text, wanted) in MALFORMED {
            let err = Prices::from_file(text, "~/p.json").expect_err(text);
            assert!(err.starts_with("price file ~/p.json: "), "{err}");
            assert!(err.contains(wanted), "{text}: {err}");
        }
    }

    /// A zero price is one, however it is written, and the smallest price an f64 holds
    /// whole is kept; below that, prices would read as 0 or as each other.
    #[test]
    fn zero_and_the_smallest_whole_price_are_kept() {
        for zero in ["0", "0.0", "-0.0", "0e-400", "0.000E+5"] {
            let text = format!(r#"{{"m": {{"input_cost_per_token": {zero}}}}}"#);
            let p = Prices::from_file(&text, "p.json").expect(&text);
            assert_eq!(flat(&p, "m"), Some(0.0), "{zero}");
        }
        let p = Prices::from_file(r#"{"m": {"input_cost_per_token": 2.3e-308}}"#, "p.json")
            .expect("prices");
        assert!(flat(&p, "m").is_some_and(|usd| usd > 0.0));
    }

    /// Fields the prices do not use may hold any JSON value, a negative whole number
    /// included, as price lists write -1 for an unknown limit.
    #[test]
    fn other_fields_of_any_json_type_are_read() {
        let p = Prices::from_file(
            r#"{"m": {"input_cost_per_token": 1e-6, "max_tokens": -1, "max_input_tokens": 8,
                      "supports_vision": false, "notes": null, "modes": ["chat"],
                      "ratio": 0.5}}"#,
            "p.json",
        )
        .expect("prices");
        assert_eq!(flat(&p, "m"), Some(1.0));
    }

    #[test]
    fn a_price_is_the_decimal_the_file_gives_so_cents_round_as_written() {
        let p = Prices::from_file(r#"{"m": {"input_cost_per_token": 1e-7}}"#, "p.json")
            .expect("prices");
        assert_eq!(flat(&p, "m"), Some(0.1));
        // 150,000 tokens est at US$0.10 per million: US$0.015, rounded to cents.
        let mut priced = Priced::default();
        priced.add(600_000, flat(&p, "m"));
        assert_eq!(priced.usd(), 0.02);
    }

    #[test]
    fn priced_characters_sum_across_models_and_round_once() {
        let mut p = Priced::default();
        // The unpriced share is what the priced characters leave of the row's estimate.
        p.add(3, Some(8.0));
        p.add(3, Some(2.0));
        p.add(5, None);
        assert_eq!(p.usd(), 0.0);
        assert_eq!(p.unpriced_tokens(11), 1);
        let mut big = Priced::default();
        big.add(10_000, Some(8.0));
        big.add(20_000, Some(2.0));
        assert_eq!(big.usd(), 0.03);
        assert_eq!(big.unpriced_tokens(30_000), 0);
    }
}
