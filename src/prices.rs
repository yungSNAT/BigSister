//! Published prices for uncached input, bundled so that pricing makes no network call.

/// When `PRICES` was read from Anthropic's and OpenAI's pricing pages.
pub const READ: &str = "15 November 2026";

/// US dollars per million uncached input tokens, by the exact model id a transcript
/// records: Claude Code's `message.model`, Codex's `turn_context` `model`. Read from
/// <https://platform.claude.com/docs/en/about-claude/pricing> and
/// <https://developers.openai.com/api/docs/pricing> (standard, short context).
///
/// shortcut: the table is as old as the release, and a model it lacks is unpriced; a
/// price list the user can update would keep it current, at the cost of a file to
/// manage. Claude Haiku 5.5 is left out: its price depends on the prompt's size, which
/// a result's characters do not show.
const PRICES: [(&str, f64); 8] = [
    ("claude-fable-5-2", 8.0),
    ("claude-opus-5-5", 4.0),
    ("claude-sonnet-5-5", 2.0),
    ("claude-sonnet-4-6", 3.0),
    ("gpt-6-astra", 10.0),
    ("gpt-6.1-sol", 2.0),
    ("gpt-6.1-terra", 1.0),
    ("gpt-6-luna", 0.10),
];

/// The note of each section whose estimates are priced at each model's price.
pub const NOTE: &str = "Dollars (usd est) price the characters of each estimate, divided by 4, once as uncached input at the published price of the model whose reply read them, from prices bundled with this release and read on 15 November 2026: an illustration, not a bill. Cached re-sends cost less and text kept in context costs again in later turns. Unpriced tokens est are those read by a model without a bundled price, or by no reply, and add no dollars.";

/// The price of `model`'s uncached input, when bundled.
#[must_use]
pub fn usd_per_mtok(model: &str) -> Option<f64> {
    PRICES
        .iter()
        .find(|(id, _)| *id == model)
        .map(|(_, usd)| *usd)
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

    #[test]
    fn only_listed_model_ids_have_a_price() {
        assert_eq!(usd_per_mtok("claude-opus-5-5"), Some(4.0));
        assert_eq!(usd_per_mtok("gpt-6.1-sol"), Some(2.0));
        for unpriced in [
            "claude-haiku-5-5",
            "<synthetic>",
            "claude-opus-5-5-x",
            "",
            "GPT-6.1-SOL",
        ] {
            assert_eq!(usd_per_mtok(unpriced), None, "{unpriced}");
        }
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
