//! The largest estimated costs across sections, for `all`: figures on one basis, the
//! characters a model read divided by 4 at its input price, ranked by dollars.

use serde_json::{Value, json};

use crate::report::{Opts, Section, Table};

/// The figures ranked: the section, the index of its table, the finding, and that
/// table's token estimate and dollar columns.
const FIGURES: [(&str, usize, &str, &str, &str); 4] = [
    (
        "polls",
        0,
        "output of commands run again with no edit since",
        "poll_tokens_est",
        "poll_usd_est",
    ),
    (
        "rereads",
        0,
        "files read again after a compaction",
        "reread_tokens_est",
        "reread_usd_est",
    ),
    (
        "schemas",
        0,
        "tool schemas loaded in sessions that never called them",
        "idle_tokens_est",
        "idle_usd_est",
    ),
    (
        "schemas",
        1,
        "skills listed in sessions that never used them",
        "idle_tokens_est",
        "idle_usd_est",
    ),
];

/// The ranked figures of `whole`, sections built with no row limit and priced.
#[must_use]
pub fn section(whole: &[Section], opts: &Opts) -> Section {
    let mut rows: Vec<(f64, u64, Vec<Value>)> = FIGURES
        .iter()
        .filter_map(|&(name, index, finding, tokens, usd)| {
            let table = whole.iter().find(|s| s.name == name)?.tables.get(index)?;
            let sum = |column: &str| -> Option<(u64, f64)> {
                let at = table.columns.iter().position(|c| *c == column)?;
                Some(table.rows.iter().fold((0, 0.0), |(n, x), row| {
                    let v = &row[at];
                    (n + v.as_u64().unwrap_or(0), x + v.as_f64().unwrap_or(0.0))
                }))
            };
            let (tokens, _) = sum(tokens)?;
            let (_, dollars) = sum(usd)?;
            let (unpriced, _) = sum("unpriced_tokens_est")?;
            let dollars = (dollars * 100.0).round() / 100.0;
            (tokens > 0).then(|| {
                let row = vec![
                    json!(name),
                    json!(finding),
                    json!(tokens),
                    json!(dollars),
                    json!(unpriced),
                ];
                (dollars, tokens, row)
            })
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut table = Table::new(
        "Most dollars first",
        vec![
            "section",
            "finding",
            "waste_tokens_est",
            "waste_usd_est",
            "unpriced_tokens_est",
        ],
    );
    table.rows = rows.into_iter().map(|(_, _, row)| row).collect();
    table.truncate(opts.limit);
    Section {
        name: "ranked",
        title: "the largest estimated costs across sections",
        summary: Vec::new(),
        tables: vec![table],
        notes: vec![
            "Each figure sums one section's table before --limit cuts it, row by row as the table shows it: its token estimates (the characters a model read, divided by 4 and rounded up in each row) and its dollars (each at that model's input price, or at the --usd-per-mtok rate, rounded to cents in each row), so a figure can differ from an unrounded total by up to a token and half a cent a row. The figures share that basis, so they rank against each other, but they can overlap (a command's output can be both a poll and a re-read), so do not add them up. Unpriced tokens est are unpriced as each section's note defines them. A figure with no tokens, as when --agent leaves out its section's agents, is left out, so fewer than 4 rows can show. Cache rebuilds (cache) and model choice (models) are priced on other bases and are left out. Figures cover the report window, not a month. An illustration, not a bill.".to_owned(),
        ],
    }
}
