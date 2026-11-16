//! Published prices for uncached input, bundled so that pricing makes no network call.

/// When `PRICES` and `BY_PROMPT` were read from Anthropic's and OpenAI's pricing pages.
pub const READ: &str = "16 November 2026";

/// US dollars per million uncached input tokens, by the exact model id a transcript
/// records: Claude Code's `message.model`, Codex's `turn_context` `model`. Read from
/// <https://platform.claude.com/docs/en/about-claude/pricing>, for each model
/// <https://platform.claude.com/docs/en/about-claude/model-deprecations> lists as active
/// under its API id, and <https://developers.openai.com/api/docs/pricing> (standard,
/// short context).
///
/// shortcut: the table is as old as the release, and a model it lacks is unpriced; a
/// price list the user can update would keep it current, at the cost of a file to
/// manage.
const PRICES: [(&str, f64); 26] = [
    ("claude-fable-5-2", 8.0),
    ("claude-mythos-5-2", 8.0),
    ("claude-fable-5-1", 10.0),
    ("claude-mythos-5-1", 10.0),
    ("claude-fable-5", 10.0),
    ("claude-mythos-5", 10.0),
    ("claude-opus-5-5", 4.0),
    ("claude-opus-5", 5.0),
    ("claude-opus-4-8", 5.0),
    ("claude-opus-4-7", 5.0),
    ("claude-opus-4-6", 5.0),
    ("claude-opus-4-5-20251101", 5.0),
    ("claude-sonnet-5-5", 2.0),
    ("claude-sonnet-5", 2.0),
    ("claude-sonnet-4-6", 3.0),
    ("claude-sonnet-4-5-20250929", 3.0),
    ("claude-haiku-4-5-20251001", 1.0),
    ("gpt-6-astra", 10.0),
    ("gpt-6.1-sol", 2.0),
    ("gpt-6.1-terra", 1.0),
    ("gpt-6-luna", 0.10),
    ("gpt-6-sol", 2.0),
    ("gpt-5.6-sol", 4.0),
    ("gpt-5.6-terra", 2.0),
    ("gpt-5.6-luna", 0.20),
    ("gpt-5.3-codex", 1.75),
];

/// Models whose price depends on the size of the prompt that read the text: the model
/// id, the largest prompt in tokens at the lower price, and the prices up to it and above
/// it. A prompt counts every input token, cache reads and writes included.
const BY_PROMPT: [(&str, u64, f64, f64); 1] = [("claude-haiku-5-5", 100_000, 0.10, 0.50)];

/// The note of each section whose estimates are priced at each model's price.
pub const NOTE: &str = "Dollars (usd est) price the characters of each estimate, divided by 4, once as uncached input at the published price of the model whose reply read them, from prices bundled with this release and read on 16 November 2026: an illustration, not a bill. Cached re-sends cost less and text kept in context costs again in later turns. Unpriced tokens est are those read by a model without a bundled price, or by no reply, and add no dollars.";

/// The price of `model`'s uncached input, when bundled. `prompt_tokens` gives the size of
/// the reading request's prompt; it is asked for only by a model priced by prompt size,
/// which stays unpriced when the size is not known.
#[must_use]
pub fn usd_per_mtok(model: &str, prompt_tokens: impl FnOnce() -> Option<u64>) -> Option<f64> {
    if let Some(&(_, usd)) = PRICES.iter().find(|(id, _)| *id == model) {
        return Some(usd);
    }
    let &(_, most, up_to, over) = BY_PROMPT.iter().find(|(id, ..)| *id == model)?;
    prompt_tokens().map(|tokens| if tokens <= most { up_to } else { over })
}

/// The priced part of a row's characters.
#[derive(Default, Clone, Copy, Debug)]
pub struct Priced {
    /// Characters read by a model with a bundled price.
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

    fn flat(model: &str) -> Option<f64> {
        usd_per_mtok(model, || panic!("{model} is not priced by prompt size"))
    }

    #[test]
    fn only_listed_model_ids_have_a_price() {
        assert_eq!(flat("claude-opus-5-5"), Some(4.0));
        assert_eq!(flat("claude-opus-4-8"), Some(5.0));
        assert_eq!(flat("claude-opus-4-5-20251101"), Some(5.0));
        assert_eq!(flat("claude-haiku-4-5-20251001"), Some(1.0));
        assert_eq!(flat("gpt-6.1-sol"), Some(2.0));
        assert_eq!(flat("gpt-5.3-codex"), Some(1.75));
        for unpriced in [
            "claude-opus-4-5",
            "claude-opus-4-1-20250805",
            "gpt-6-pro",
            "<synthetic>",
            "claude-opus-5-5-x",
            "",
            "GPT-6.1-SOL",
        ] {
            assert_eq!(flat(unpriced), None, "{unpriced}");
        }
    }

    #[test]
    fn every_listed_model_is_priced_once() {
        for (at, (id, usd)) in PRICES.iter().enumerate() {
            assert!(*usd > 0.0, "{id}");
            assert!(!PRICES[..at].iter().any(|(other, _)| other == id), "{id}");
            assert!(!BY_PROMPT.iter().any(|(other, ..)| other == id), "{id}");
        }
    }

    #[test]
    fn haiku_5_5_is_priced_by_the_prompt_that_read_it() {
        let haiku = |tokens| usd_per_mtok("claude-haiku-5-5", || tokens);
        assert_eq!(haiku(Some(0)), Some(0.10));
        assert_eq!(haiku(Some(100_000)), Some(0.10));
        assert_eq!(haiku(Some(100_001)), Some(0.50));
        assert_eq!(haiku(None), None);
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
