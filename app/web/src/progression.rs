//! The operator's file, in the browser.
//!
//! The rules are not here — they are in [`pocketskynet_core::progression`],
//! shared with the server and mirrored by the iOS client. This module is only
//! the stored half: what this browser has accumulated, persisted to local
//! storage, plus the bookkeeping for today's standing orders.
//!
//! Local storage rather than the server on purpose. Progression is a personal
//! readout, and making it a server resource would mean a messenger that stops
//! being fun when the LAN is down. The server sees it only when the ladder
//! reports in, and only as a number it cannot check.

use std::collections::BTreeMap;

use pocketskynet_core::progression::{self, Award, Directive, Rank, Snapshot, Trophy};
use serde::{Deserialize, Serialize};

const KEY: &str = "ps-progression";

/// The whole persisted file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Progression {
    #[serde(default)]
    pub load: i64,
    #[serde(default)]
    pub streak: i64,
    /// Local day index of the last boot; `None` before the first one.
    #[serde(default)]
    pub last_boot_day: Option<i64>,
    #[serde(default)]
    pub counts: BTreeMap<String, i64>,
    #[serde(default)]
    pub orders_completed: i64,
    /// The day today's order progress belongs to. Scoped so yesterday's
    /// half-finished order cannot leak into today.
    #[serde(default)]
    pub directive_day: i64,
    #[serde(default)]
    pub directive_progress: BTreeMap<String, u32>,
    #[serde(default)]
    pub directive_claimed: Vec<String>,
    #[serde(default = "default_palette")]
    pub palette: String,
}

fn default_palette() -> String {
    "cyan".to_string()
}

/// What just happened, for the UI to announce.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub granted: i64,
    pub award: Award,
    /// Orders completed by this award, with their bounties already paid.
    pub completed: Vec<Directive>,
    /// The rank crossed into, if this award crossed one.
    pub promoted: Option<Rank>,
}

impl Progression {
    pub fn load_stored() -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            use gloo_storage::{LocalStorage, Storage};
            LocalStorage::get(KEY).unwrap_or_default()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self::default()
        }
    }

    pub fn persist(&self) {
        #[cfg(target_arch = "wasm32")]
        {
            use gloo_storage::{LocalStorage, Storage};
            let _ = LocalStorage::set(KEY, self);
        }
    }

    pub fn rank(&self) -> Rank {
        progression::rank_for(self.load)
    }

    pub fn fraction(&self) -> f64 {
        progression::fraction_for(self.load)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            counts: self.counts.clone(),
            level: self.rank().level,
            streak: self.streak,
            load: self.load,
            directives_completed: self.orders_completed,
        }
    }

    pub fn trophies(&self) -> Vec<Trophy> {
        progression::earned_trophies(&self.snapshot())
    }

    pub fn today(&self) -> Vec<Directive> {
        progression::slate_for_day(self.directive_day)
    }

    pub fn directive_progress(&self, directive: &Directive) -> u32 {
        self.directive_progress
            .get(directive.id)
            .copied()
            .unwrap_or(0)
            .min(directive.goal)
    }

    pub fn is_complete(&self, directive: &Directive) -> bool {
        self.directive_progress
            .get(directive.id)
            .copied()
            .unwrap_or(0)
            >= directive.goal
    }

    pub fn completed_today(&self) -> usize {
        self.today().iter().filter(|d| self.is_complete(d)).count()
    }

    /// Roll the slate over if `today` is a new day. Wipes progress, not the
    /// lifetime counters.
    pub fn refresh(&mut self, today: i64) {
        if self.directive_day == today {
            return;
        }
        self.directive_day = today;
        self.directive_progress.clear();
        self.directive_claimed.clear();
    }

    /// Report in for the day: advance the streak, and pay the boot award the
    /// first time it happens on a given day.
    pub fn boot(&mut self, today: i64) -> Option<Outcome> {
        let is_new_day = self.last_boot_day != Some(today);
        self.streak = progression::advance_streak(self.streak, self.last_boot_day, today);
        self.last_boot_day = Some(today);
        self.refresh(today);

        if is_new_day {
            let multiplier = progression::streak_multiplier(self.streak);
            let outcome = self.record_scaled(Award::DailyBoot, multiplier, today);
            Some(outcome)
        } else {
            self.persist();
            None
        }
    }

    /// Record an award.
    pub fn record(&mut self, award: Award, today: i64) -> Outcome {
        self.record_scaled(award, 1.0, today)
    }

    fn record_scaled(&mut self, award: Award, multiplier: f64, today: i64) -> Outcome {
        self.refresh(today);

        let before = self.rank().level;
        let granted = ((award.load() as f64) * multiplier).round().max(1.0) as i64;
        self.load += granted;
        *self.counts.entry(award.key().to_string()).or_insert(0) += 1;

        // Fold into today's orders and pay whatever that completed.  One
        // directional pass: a bounty is not an award, cannot advance an order,
        // and so cannot complete itself.
        let mut completed = Vec::new();
        for directive in self.today() {
            if directive.award != award
                || self.directive_claimed.iter().any(|id| id == directive.id)
            {
                continue;
            }
            let next = self
                .directive_progress
                .entry(directive.id.to_string())
                .or_insert(0);
            *next += 1;
            if *next >= directive.goal {
                self.directive_claimed.push(directive.id.to_string());
                completed.push(directive);
            }
        }
        for directive in &completed {
            self.load += directive.bounty;
            self.orders_completed += 1;
        }

        let after = self.rank();
        let promoted = if after.level > before {
            Some(after)
        } else {
            None
        };

        self.persist();
        Outcome {
            granted,
            award,
            completed,
            promoted,
        }
    }
}

/// Today, as the local day index the rules count in.
pub fn today() -> i64 {
    progression::day_index(
        crate::format::now_ms(),
        crate::format::tz_offset_minutes() as i64 * 60,
    )
}

/// Record an award against the stored file.
///
/// Deliberately load-modify-persist rather than a long-lived handle in the
/// store: awards are rare (a keystroke never fires one), the file is a few
/// hundred bytes, and a single owner means no screen can hold a stale copy
/// and write it back over a newer one.
pub fn award(award: Award) -> Outcome {
    let mut file = Progression::load_stored();
    file.record(award, today())
}

/// Report in for the day. Called once when a session reaches the shell.
pub fn boot() -> Option<Outcome> {
    let mut file = Progression::load_stored();
    file.boot(today())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 20_000;

    #[test]
    fn recording_pays_the_award_and_counts_it() {
        let mut file = Progression {
            directive_day: DAY,
            ..Default::default()
        };
        let outcome = file.record(Award::Transmission, DAY);
        assert_eq!(outcome.granted, Award::Transmission.load());
        assert_eq!(file.load, Award::Transmission.load());
        assert_eq!(file.counts.get("transmission"), Some(&1));
    }

    #[test]
    fn crossing_a_threshold_reports_a_promotion_exactly_once() {
        let mut file = Progression {
            directive_day: DAY,
            load: 118,
            ..Default::default()
        };
        let first = file.record(Award::Transmission, DAY); // 118 -> 126, crosses 120
        assert_eq!(first.promoted.map(|r| r.level), Some(2));
        let second = file.record(Award::Transmission, DAY);
        assert!(
            second.promoted.is_none(),
            "the same rank must not fire twice"
        );
    }

    #[test]
    fn an_order_completes_once_and_pays_its_bounty() {
        let mut file = Progression {
            directive_day: DAY,
            ..Default::default()
        };
        let directive = file.today()[0];

        // Assert on *this* order, not on the aggregate: a slate can legitimately
        // carry two orders against the same award (relay-10 beside relay-25),
        // and driving the longer one necessarily finishes the shorter one too.
        let mut completions = 0;
        for _ in 0..directive.goal {
            let outcome = file.record(directive.award, DAY);
            completions += outcome
                .completed
                .iter()
                .filter(|d| d.id == directive.id)
                .count();
        }
        assert_eq!(completions, 1, "the order should complete exactly once");
        assert!(file.is_complete(&directive));

        let orders_so_far = file.orders_completed;
        let bounty_paid = file.load;

        // Past the goal it must neither re-complete nor re-pay.
        let after = file.record(directive.award, DAY);
        assert!(!after.completed.iter().any(|d| d.id == directive.id));
        assert_eq!(file.orders_completed, orders_so_far);
        assert_eq!(
            file.load - bounty_paid,
            directive.award.load(),
            "only the award itself, no second bounty"
        );
    }

    #[test]
    fn a_new_day_wipes_order_progress_but_not_the_file() {
        let mut file = Progression {
            directive_day: DAY,
            ..Default::default()
        };
        let directive = file.today()[0];
        file.record(directive.award, DAY);
        assert!(file.directive_progress.values().any(|&n| n > 0));

        let carried = file.load;
        file.refresh(DAY + 1);
        assert!(file.directive_progress.is_empty());
        assert!(file.directive_claimed.is_empty());
        assert_eq!(file.load, carried, "load is lifetime, not daily");
    }

    #[test]
    fn booting_twice_in_a_day_pays_once() {
        let mut file = Progression::default();
        assert!(file.boot(DAY).is_some());
        assert_eq!(file.streak, 1);
        let load_after_first = file.load;

        assert!(file.boot(DAY).is_none(), "the same day is not a second day");
        assert_eq!(file.load, load_after_first);

        assert!(file.boot(DAY + 1).is_some());
        assert_eq!(file.streak, 2);
    }

    #[test]
    fn a_streak_multiplies_the_boot_award() {
        let mut file = Progression {
            streak: 10,
            last_boot_day: Some(DAY - 1),
            ..Default::default()
        };
        let outcome = file.boot(DAY).expect("a new day pays");
        assert!(
            outcome.granted > Award::DailyBoot.load(),
            "a ten day streak should be worth more than a cold start"
        );
    }
}
