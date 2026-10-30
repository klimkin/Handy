use serde::{Deserialize, Serialize};
use specta::Type;

/// What one extended box plot draws: the range, the middle half, the median
/// and the mean.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Type)]
pub struct BoxStats {
    pub min: f64,
    pub q1: f64,
    pub median: f64,
    pub q3: f64,
    pub max: f64,
    pub mean: f64,
}

impl BoxStats {
    /// `None` when there are no values. Quartiles interpolate linearly
    /// between the two nearest ranks.
    pub fn of(mut values: Vec<f64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(f64::total_cmp);
        let last = values.len() - 1;
        let quantile = |p: f64| {
            let rank = last as f64 * p;
            let (below, above) = (rank.floor() as usize, rank.ceil() as usize);
            values[below] + (rank - below as f64) * (values[above] - values[below])
        };
        Some(Self {
            min: values[0],
            q1: quantile(0.25),
            median: quantile(0.5),
            q3: quantile(0.75),
            max: values[last],
            mean: values.iter().sum::<f64>() / values.len() as f64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_values_have_no_box() {
        assert_eq!(BoxStats::of(vec![]), None);
    }

    #[test]
    fn one_value_collapses_the_box() {
        let stats = BoxStats::of(vec![3.0]).unwrap();
        assert_eq!(
            stats,
            BoxStats {
                min: 3.0,
                q1: 3.0,
                median: 3.0,
                q3: 3.0,
                max: 3.0,
                mean: 3.0
            }
        );
    }

    #[test]
    fn quartiles_interpolate_between_ranks() {
        let stats = BoxStats::of(vec![40.0, 10.0, 30.0, 20.0]).unwrap();
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.q1, 17.5);
        assert_eq!(stats.median, 25.0);
        assert_eq!(stats.q3, 32.5);
        assert_eq!(stats.max, 40.0);
        assert_eq!(stats.mean, 25.0);
    }

    #[test]
    fn mean_follows_an_outlier_but_the_median_does_not() {
        let stats = BoxStats::of(vec![1.0, 1.0, 1.0, 1.0, 21.0]).unwrap();
        assert_eq!(stats.median, 1.0);
        assert_eq!(stats.mean, 5.0);
    }
}
