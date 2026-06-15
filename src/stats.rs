//! Per-column summary statistics.
//!
//! [`ColStats`] folds one column's cells into count/min/max/sum/mean/stddev. It
//! only *computes* — turning the result into rows (the `stats` command) or into
//! a colour (`fmt`'s value-based formatting, later) is the caller's job, so the
//! same accumulator serves both. Keeping computation and presentation apart is
//! what makes it reusable.

use crate::field::Field;

/// The profile columns emitted per input column, in order. After a `stats`
/// stage the row *is* this schema, so downstream `sort`/`cols`/`fmt` see it.
pub const STATS_SCHEMA: [&str; 8] = [
    "field", "count", "empty", "min", "max", "sum", "mean", "stddev",
];

/// Running statistics for one column. A column is treated as numeric until a
/// non-empty cell fails to parse as a number; then numeric stats are dropped and
/// only count/empty and lexical min/max are reported.
#[derive(Clone, Debug)]
pub struct ColStats {
    count: u64, // non-empty cells
    empty: u64, // empty cells
    numeric: bool,
    nnum: u64, // finite numeric values folded into the aggregates below
    nmin: f64,
    nmax: f64,
    sum: f64,
    // Welford running mean/variance over the finite numeric values (n == nnum).
    mean: f64,
    m2: f64,
    // Lexical min/max, used to report text columns.
    smin: Option<String>,
    smax: Option<String>,
}

impl Default for ColStats {
    fn default() -> Self {
        ColStats {
            count: 0,
            empty: 0,
            numeric: true,
            nnum: 0,
            nmin: f64::INFINITY,
            nmax: f64::NEG_INFINITY,
            sum: 0.0,
            mean: 0.0,
            m2: 0.0,
            smin: None,
            smax: None,
        }
    }
}

impl ColStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one cell into the accumulator.
    pub fn update(&mut self, f: &Field) {
        let s = f.as_str();
        if s.is_empty() {
            self.empty += 1;
            return;
        }
        self.count += 1;

        // Lexical min/max — cheap and valid for any column type.
        if self.smin.as_deref().is_none_or(|m| s.as_ref() < m) {
            self.smin = Some(s.as_ref().to_owned());
        }
        if self.smax.as_deref().is_none_or(|m| s.as_ref() > m) {
            self.smax = Some(s.as_ref().to_owned());
        }

        if self.numeric {
            let v = match f {
                Field::Num(n) => Some(*n),
                _ => s.as_ref().trim().parse::<f64>().ok(),
            };
            match v {
                // A finite value folds into the aggregates.
                Some(v) if v.is_finite() => {
                    self.nnum += 1;
                    self.sum += v;
                    self.nmin = self.nmin.min(v);
                    self.nmax = self.nmax.max(v);
                    let delta = v - self.mean;
                    self.mean += delta / self.nnum as f64;
                    self.m2 += delta * (v - self.mean);
                }
                // NaN/inf parses as a number but would poison sum/mean/stddev, so
                // it's skipped from the aggregates (still counted as non-empty).
                // The column stays numeric. (select/sort/to-num still accept it.)
                Some(_) => {}
                None => self.numeric = false,
            }
        }
    }

    /// Fold another partial accumulator into this one. Associative, so parallel
    /// shards can each build a partial over their slice of the input and the
    /// merged result is identical to a single pass — see [`Self::update`].
    pub fn merge(&mut self, other: &ColStats) {
        self.count += other.count;
        self.empty += other.empty;
        // Numeric only if every shard saw the column as numeric.
        self.numeric = self.numeric && other.numeric;
        // Lexical min/max.
        if let Some(o) = &other.smin
            && self.smin.as_deref().is_none_or(|m| o.as_str() < m)
        {
            self.smin = Some(o.clone());
        }
        if let Some(o) = &other.smax
            && self.smax.as_deref().is_none_or(|m| o.as_str() > m)
        {
            self.smax = Some(o.clone());
        }
        // Numeric min/max — the ±∞ identities make an empty partner a no-op.
        self.nmin = self.nmin.min(other.nmin);
        self.nmax = self.nmax.max(other.nmax);
        self.sum += other.sum;
        // Welford parallel combine (Chan et al.) over the finite-value counts.
        if other.nnum > 0 {
            let (na, nb) = (self.nnum as f64, other.nnum as f64);
            let n = na + nb;
            let delta = other.mean - self.mean;
            self.mean += delta * nb / n;
            self.m2 += other.m2 + delta * delta * na * nb / n;
            self.nnum += other.nnum;
        }
    }

    /// Render this column's profile row: `field_name` followed by the
    /// [`STATS_SCHEMA`] columns. Numeric stats are blank for a text or all-empty
    /// column.
    pub fn to_row(&self, field_name: &str) -> Vec<Field<'static>> {
        let mut row: Vec<Field<'static>> = Vec::with_capacity(STATS_SCHEMA.len());
        row.push(Field::Owned(field_name.to_owned()));
        row.push(Field::Num(self.count as f64));
        row.push(Field::Num(self.empty as f64));
        row.push(self.min_field());
        row.push(self.max_field());
        if self.has_numeric() {
            row.push(Field::Num(self.sum));
            row.push(Field::Num(self.mean));
            row.push(self.stddev_field());
        } else {
            row.push(Field::Str(""));
            row.push(Field::Str(""));
            row.push(Field::Str(""));
        }
        row
    }

    // Granular accessors so the group-by reducer (`agg`) can pull a single
    // aggregate out without re-deriving the numeric-vs-text policy. These mirror
    // the branches of [`Self::to_row`]; presentation (column layout) stays the
    // caller's job.

    /// Non-empty cell count.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Whether finite numeric aggregates (sum/mean/stddev) are available.
    pub fn has_numeric(&self) -> bool {
        self.numeric && self.nnum > 0
    }

    pub fn sum(&self) -> f64 {
        self.sum
    }

    pub fn mean(&self) -> f64 {
        self.mean
    }

    /// `min`: numeric for a numeric column, else lexical; blank when empty.
    pub fn min_field(&self) -> Field<'static> {
        if self.has_numeric() {
            Field::Num(self.nmin)
        } else {
            opt_str(&self.smin)
        }
    }

    /// `max`: numeric for a numeric column, else lexical; blank when empty.
    pub fn max_field(&self) -> Field<'static> {
        if self.has_numeric() {
            Field::Num(self.nmax)
        } else {
            opt_str(&self.smax)
        }
    }

    /// Sample stddev (n-1) as a cell; blank for <2 finite values or a text column.
    pub fn stddev_field(&self) -> Field<'static> {
        if self.has_numeric() && self.nnum >= 2 {
            Field::Num((self.m2 / (self.nnum - 1) as f64).sqrt())
        } else {
            Field::Str("")
        }
    }
}

fn opt_str(s: &Option<String>) -> Field<'static> {
    match s {
        Some(s) => Field::Owned(s.clone()),
        None => Field::Str(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(c: &ColStats) -> Vec<String> {
        c.to_row("x")
            .iter()
            .map(|f| f.as_str().into_owned())
            .collect()
    }

    fn profile(vals: &[&str]) -> Vec<String> {
        let mut c = ColStats::new();
        for v in vals {
            c.update(&Field::Str(v));
        }
        cells(&c)
    }

    #[test]
    fn numeric_column() {
        // field,count,empty,min,max,sum,mean,stddev
        let cells = profile(&["1", "2", "3", ""]);
        assert_eq!(cells, ["x", "3", "1", "1", "3", "6", "2", "1"]);
    }

    #[test]
    fn text_column_blanks_numeric_stats() {
        let cells = profile(&["banana", "apple", "cherry"]);
        assert_eq!(cells, ["x", "3", "0", "apple", "cherry", "", "", ""]);
    }

    #[test]
    fn single_value_has_no_stddev() {
        let cells = profile(&["5"]);
        assert_eq!(cells[6], "5"); // mean
        assert_eq!(cells[7], ""); // stddev undefined for n=1
    }

    #[test]
    fn all_empty_column() {
        let cells = profile(&["", "", ""]);
        assert_eq!(cells, ["x", "0", "3", "", "", "", "", ""]);
    }

    #[test]
    fn one_non_numeric_cell_makes_it_text() {
        let cells = profile(&["1", "2", "oops"]);
        assert_eq!(cells[3], "1"); // lexical min
        assert_eq!(cells[4], "oops"); // lexical max
        assert_eq!(cells[5], ""); // sum blank
    }

    #[test]
    fn merge_matches_single_pass() {
        // Building partials and merging them must equal a single pass, at every
        // split point (covers the Welford combine, count/empty, and min/max).
        let vals = ["3", "1", "", "5", "9", "2", "8", "4"];
        let whole = {
            let mut c = ColStats::new();
            vals.iter().for_each(|v| c.update(&Field::Str(v)));
            cells(&c)
        };
        for split in 0..=vals.len() {
            let mut a = ColStats::new();
            vals[..split].iter().for_each(|v| a.update(&Field::Str(v)));
            let mut b = ColStats::new();
            vals[split..].iter().for_each(|v| b.update(&Field::Str(v)));
            a.merge(&b);
            assert_eq!(cells(&a), whole, "split at {split}");
        }
    }

    #[test]
    fn merge_propagates_text_and_lexical_minmax() {
        // A non-numeric cell in either partial makes the merged column text.
        let mut a = ColStats::new();
        ["5", "2"].iter().for_each(|v| a.update(&Field::Str(v)));
        let mut b = ColStats::new();
        ["oops", "9"].iter().for_each(|v| b.update(&Field::Str(v)));
        a.merge(&b);
        let cells = cells(&a);
        assert_eq!(cells[1], "4"); // count
        assert_eq!(cells[3], "2"); // lexical min over {5,2,oops,9}
        assert_eq!(cells[4], "oops"); // lexical max
        assert_eq!(cells[5], ""); // sum blank (text column)
    }

    #[test]
    fn non_finite_values_are_skipped_not_poisoning() {
        // "inf" parses as a number but must not poison sum/mean/stddev; it's
        // counted as a non-empty cell and the column stays numeric over [1, 2].
        // field,count,empty,min,max,sum,mean,stddev
        let cells = profile(&["1", "2", "inf"]);
        assert_eq!(cells[1], "3"); // count: all three non-empty
        assert_eq!(&cells[3..7], ["1", "2", "3", "1.5"]); // min,max,sum,mean over finite
        assert_eq!(cells[7], "0.707107"); // stddev over the two finite values
        // A column of only non-finite values has no finite aggregates.
        let only = profile(&["nan", "inf"]);
        assert_eq!(only[1], "2"); // counted
        assert_eq!(&only[5..8], ["", "", ""]); // sum/mean/stddev blank
    }
}
