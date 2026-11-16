use crate::services::service_hopper::today_ist;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct V1V2Cutoff {
    pub gtfs_id: String,
    pub cutoff_date: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReaderConfig {
    cutoffs: HashMap<String, NaiveDate>,
}

impl ReaderConfig {
    pub fn from_cutoffs(cutoffs: &[V1V2Cutoff]) -> anyhow::Result<Self> {
        let mut config = ReaderConfig::default();
        for c in cutoffs {
            let gtfs_id = c.gtfs_id.trim();
            if gtfs_id.is_empty() {
                anyhow::bail!("v1_v2_cutoffs: gtfs_id is empty");
            }
            let date =
                NaiveDate::parse_from_str(c.cutoff_date.trim(), "%Y-%m-%d").map_err(|e| {
                    anyhow::anyhow!(
                        "v1_v2_cutoffs: cutoff_date '{}' for {} is not YYYY-MM-DD: {}",
                        c.cutoff_date,
                        gtfs_id,
                        e
                    )
                })?;
            if config.cutoffs.insert(gtfs_id.to_string(), date).is_some() {
                anyhow::bail!("v1_v2_cutoffs: {} is listed twice", gtfs_id);
            }
        }
        Ok(config)
    }

    pub fn cutoff(&self, gtfs_id: &str) -> Option<NaiveDate> {
        self.cutoffs.get(gtfs_id).copied()
    }

    pub fn reads_v1(&self, gtfs_id: &str) -> bool {
        self.reads_v1_on(gtfs_id, today_ist())
    }

    pub fn reads_v1_on(&self, gtfs_id: &str, today: NaiveDate) -> bool {
        self.cutoff(gtfs_id).is_none_or(|cutoff| today < cutoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn cut(gtfs_id: &str, date: &str) -> V1V2Cutoff {
        V1V2Cutoff {
            gtfs_id: gtfs_id.to_string(),
            cutoff_date: date.to_string(),
        }
    }

    #[test]
    fn reads_v1_before_the_cutoff_and_not_from_it() {
        let cfg = ReaderConfig::from_cutoffs(&[cut("chennai_bus", "2026-11-22")]).unwrap();
        assert!(cfg.reads_v1_on("chennai_bus", day("2026-11-21")));
        assert!(!cfg.reads_v1_on("chennai_bus", day("2026-11-22")));
        assert!(!cfg.reads_v1_on("chennai_bus", day("2026-12-01")));
    }

    #[test]
    fn a_feed_without_a_cutoff_keeps_reading_v1() {
        let cfg = ReaderConfig::from_cutoffs(&[cut("chennai_bus", "2026-11-22")]).unwrap();
        assert!(cfg.reads_v1_on("kolkata_bus", day("2026-12-01")));
        assert!(ReaderConfig::default().reads_v1("kolkata_bus"));
    }

    #[test]
    fn bad_cutoffs_are_rejected() {
        assert!(ReaderConfig::from_cutoffs(&[cut("a", "22-11-2026")]).is_err());
        assert!(
            ReaderConfig::from_cutoffs(&[cut("a", "2026-11-22"), cut("a", "2026-11-23")]).is_err()
        );
        assert!(ReaderConfig::from_cutoffs(&[cut(" ", "2026-11-22")]).is_err());
    }
}
