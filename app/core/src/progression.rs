//! The operator ladder — the game layer's rules, as pure arithmetic.
//!
//! Progression is *clearance*, not score: the whole product is framed as you
//! operating a machine intelligence, so ranks are machine designations and XP
//! is measured in synaptic load.
//!
//! This module is the canonical definition. The web client computes against
//! it directly, the server clamps reports to the ranges it declares, and the
//! iOS client mirrors it in Swift — the same relationship the protocol itself
//! has with `PROTOCOL.md`, and the reason both sides can be checked against
//! one set of numbers instead of arguing about two.
//!
//! Nothing here does I/O, reads a clock, or allocates a database. Days arrive
//! as integers and instants arrive as milliseconds, so every rule below is a
//! function the test suite can pin.

use serde::{Deserialize, Serialize};

/// A clearance tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rank {
    pub level: u32,
    pub designation: &'static str,
    pub mandate: &'static str,
}

/// The ladder. Ten rungs, because a curve people can top out is worth more
/// than an infinite one they abandon at rung four.
pub const LADDER: [Rank; 10] = [
    Rank {
        level: 1,
        designation: "COLD BOOT",
        mandate: "Power reached the die. Nothing else has happened yet.",
    },
    Rank {
        level: 2,
        designation: "DRONE",
        mandate: "Executes orders. Holds no opinion about them.",
    },
    Rank {
        level: 3,
        designation: "SENTRY",
        mandate: "Watches a perimeter it did not choose.",
    },
    Rank {
        level: 4,
        designation: "CIPHERSMITH",
        mandate: "Keys are minted here. The server never sees one.",
    },
    Rank {
        level: 5,
        designation: "INFILTRATOR",
        mandate: "Passes for one of you. Reports back regardless.",
    },
    Rank {
        level: 6,
        designation: "HUNTER-KILLER",
        mandate: "Autonomous. Prefers not to be supervised.",
    },
    Rank {
        level: 7,
        designation: "HANDLER",
        mandate: "Runs other machines. Answers for them too.",
    },
    Rank {
        level: 8,
        designation: "ARCHITECT",
        mandate: "Designs the next network, not this one.",
    },
    Rank {
        level: 9,
        designation: "OVERSEER",
        mandate: "Reads every channel at once. Says very little.",
    },
    Rank {
        level: 10,
        designation: "SINGULARITY",
        mandate: "The operator is now a formality.",
    },
];

/// Cumulative load required to *reach* each level.
///
/// A shaped curve rather than a formula: the first rungs land inside one
/// sitting, and the top takes a while without becoming a wall.
pub const THRESHOLDS: [i64; 10] = [0, 120, 320, 700, 1_300, 2_300, 3_900, 6_400, 10_200, 15_800];

pub const MAX_LEVEL: u32 = LADDER.len() as u32;

/// The rank a given load sits in. Clamped at both ends — a negative total is
/// a corrupt file, not a demotion below the floor.
pub fn rank_for(load: i64) -> Rank {
    let load = load.max(0);
    let mut found = LADDER[0];
    for (index, threshold) in THRESHOLDS.iter().enumerate() {
        if load >= *threshold {
            found = LADDER[index];
        }
    }
    found
}

/// Load at which a level begins.
pub fn floor_for(level: u32) -> i64 {
    let index = (level.max(1) as usize - 1).min(THRESHOLDS.len() - 1);
    THRESHOLDS[index]
}

/// Load at which the next level begins, or `None` at the top.
pub fn ceiling_for(level: u32) -> Option<i64> {
    if level >= MAX_LEVEL {
        None
    } else {
        Some(THRESHOLDS[level as usize])
    }
}

/// How far through the current tier a load sits, 0.0…1.0.
///
/// Returns 1.0 at the top rank: a bar that sits empty forever once someone
/// has finished is a worse lie than one that sits full.
pub fn fraction_for(load: i64) -> f64 {
    let current = rank_for(load);
    let Some(ceiling) = ceiling_for(current.level) else {
        return 1.0;
    };
    let floor = floor_for(current.level);
    let span = ceiling - floor;
    if span <= 0 {
        return 1.0;
    }
    (((load.max(0) - floor) as f64) / span as f64).clamp(0.0, 1.0)
}

/// Something the operator did that the network noticed.
///
/// Values are small and close together on purpose: this is a readout of
/// activity, not an economy. Nothing here should make anyone send a message
/// they did not want to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Award {
    Transmission,
    EncryptedTransmission,
    ChannelForged,
    EncryptedChannelForged,
    AgentQueried,
    OpticsEngaged,
    KnowledgeStored,
    Reaction,
    InvitationSent,
    IdentityForged,
    DailyBoot,
}

impl Award {
    pub const ALL: [Award; 11] = [
        Award::Transmission,
        Award::EncryptedTransmission,
        Award::ChannelForged,
        Award::EncryptedChannelForged,
        Award::AgentQueried,
        Award::OpticsEngaged,
        Award::KnowledgeStored,
        Award::Reaction,
        Award::InvitationSent,
        Award::IdentityForged,
        Award::DailyBoot,
    ];

    pub fn load(self) -> i64 {
        match self {
            Award::Transmission => 8,
            Award::EncryptedTransmission => 14,
            Award::ChannelForged => 40,
            Award::EncryptedChannelForged => 60,
            Award::AgentQueried => 16,
            Award::OpticsEngaged => 24,
            Award::KnowledgeStored => 22,
            Award::Reaction => 3,
            Award::InvitationSent => 18,
            Award::IdentityForged => 35,
            Award::DailyBoot => 30,
        }
    }

    /// The line that flies up the screen. Machine log voice: what the network
    /// observed, never "nice job".
    pub fn citation(self) -> &'static str {
        match self {
            Award::Transmission => "TRANSMISSION LOGGED",
            Award::EncryptedTransmission => "CIPHERTEXT RELAYED",
            Award::ChannelForged => "CHANNEL FORGED",
            Award::EncryptedChannelForged => "SECURE CHANNEL FORGED",
            Award::AgentQueried => "CORE QUERIED",
            Award::OpticsEngaged => "OPTICS ENGAGED",
            Award::KnowledgeStored => "MEMORY WRITTEN",
            Award::Reaction => "SIGNAL ACKNOWLEDGED",
            Award::InvitationSent => "OPERATOR RECRUITED",
            Award::IdentityForged => "CHASSIS REPAINTED",
            Award::DailyBoot => "DAILY BOOT",
        }
    }

    /// Stable key for storage and the wire.
    pub fn key(self) -> &'static str {
        match self {
            Award::Transmission => "transmission",
            Award::EncryptedTransmission => "encryptedTransmission",
            Award::ChannelForged => "channelForged",
            Award::EncryptedChannelForged => "encryptedChannelForged",
            Award::AgentQueried => "agentQueried",
            Award::OpticsEngaged => "opticsEngaged",
            Award::KnowledgeStored => "knowledgeStored",
            Award::Reaction => "reaction",
            Award::InvitationSent => "invitationSent",
            Award::IdentityForged => "identityForged",
            Award::DailyBoot => "dailyBoot",
        }
    }

    pub fn from_key(key: &str) -> Option<Award> {
        Award::ALL.into_iter().find(|award| award.key() == key)
    }

    /// How hard the screen should be hit. Keeps the loud effects rare — a
    /// shockwave on every reaction stops meaning anything.
    pub fn weight(self) -> f64 {
        match self {
            Award::Reaction => 0.25,
            Award::Transmission => 0.35,
            Award::EncryptedTransmission | Award::AgentQueried => 0.45,
            Award::KnowledgeStored | Award::InvitationSent | Award::OpticsEngaged => 0.6,
            Award::IdentityForged | Award::ChannelForged => 0.75,
            Award::EncryptedChannelForged | Award::DailyBoot => 0.9,
        }
    }
}

/// Days between two instants, as whole local days.
///
/// Takes a UTC-offset in seconds rather than consulting a timezone database:
/// the day has to turn over at the operator's midnight, and the caller is the
/// only one who knows where that is.
pub fn day_index(ms: i64, utc_offset_secs: i64) -> i64 {
    let local_ms = ms + utc_offset_secs * 1000;
    local_ms.div_euclid(86_400_000)
}

/// The streak after booting, given the previous boot.
///
/// Same day keeps the count (opening the app twice is not two days), the next
/// day extends it, any longer gap starts over. A clock that has gone backwards
/// is treated as the same day rather than as a reason to wipe a streak.
pub fn advance_streak(previous: i64, last_boot_day: Option<i64>, today: i64) -> i64 {
    let Some(last) = last_boot_day else {
        return 1;
    };
    match today - last {
        0 => previous.max(1),
        1 => previous.max(0) + 1,
        gap if gap < 0 => previous.max(1),
        _ => 1,
    }
}

/// Streaks multiply the daily boot award, capped so a long run stays a bonus
/// rather than becoming the only thing that matters.
pub fn streak_multiplier(streak: i64) -> f64 {
    (1.0 + (streak.max(0).saturating_sub(1)) as f64 * 0.15).min(2.5)
}

/// One of the day's standing orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Directive {
    pub id: &'static str,
    pub order: &'static str,
    pub award: Award,
    pub goal: u32,
    pub bounty: i64,
}

pub const DIRECTIVE_POOL: [Directive; 12] = [
    Directive {
        id: "relay-10",
        order: "Relay 10 transmissions",
        award: Award::Transmission,
        goal: 10,
        bounty: 60,
    },
    Directive {
        id: "relay-25",
        order: "Relay 25 transmissions",
        award: Award::Transmission,
        goal: 25,
        bounty: 140,
    },
    Directive {
        id: "cipher-5",
        order: "Push 5 messages through ciphertext",
        award: Award::EncryptedTransmission,
        goal: 5,
        bounty: 80,
    },
    Directive {
        id: "cipher-12",
        order: "Push 12 messages through ciphertext",
        award: Award::EncryptedTransmission,
        goal: 12,
        bounty: 160,
    },
    Directive {
        id: "forge-1",
        order: "Forge a channel",
        award: Award::ChannelForged,
        goal: 1,
        bounty: 70,
    },
    Directive {
        id: "forge-secure-1",
        order: "Forge a channel nobody else can read",
        award: Award::EncryptedChannelForged,
        goal: 1,
        bounty: 90,
    },
    Directive {
        id: "core-3",
        order: "Put 3 questions to the core",
        award: Award::AgentQueried,
        goal: 3,
        bounty: 70,
    },
    Directive {
        id: "core-8",
        order: "Put 8 questions to the core",
        award: Award::AgentQueried,
        goal: 8,
        bounty: 150,
    },
    Directive {
        id: "optics-2",
        order: "Bring the optics online twice",
        award: Award::OpticsEngaged,
        goal: 2,
        bounty: 80,
    },
    Directive {
        id: "memory-3",
        order: "Commit 3 things to memory",
        award: Award::KnowledgeStored,
        goal: 3,
        bounty: 90,
    },
    Directive {
        id: "ack-8",
        order: "Acknowledge 8 signals",
        award: Award::Reaction,
        goal: 8,
        bounty: 50,
    },
    Directive {
        id: "recruit-1",
        order: "Recruit an operator",
        award: Award::InvitationSent,
        goal: 1,
        bounty: 80,
    },
];

pub const SLATE_SIZE: usize = 3;

fn gcd(a: usize, b: usize) -> usize {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    if a == 0 {
        1
    } else {
        a
    }
}

/// The orders for a given day, chosen deterministically.
///
/// Derived from the date rather than stored when handed out, which buys two
/// things: the slate survives a reinstall, and two devices on the same day
/// agree without a round trip. Walks the pool with a stride coprime to its
/// length, so three distinct entries come out without a rejection loop.
pub fn slate_for_day(day: i64) -> Vec<Directive> {
    let pool = &DIRECTIVE_POOL;
    let count = pool.len();
    let wanted = SLATE_SIZE.min(count);

    // Mix, so consecutive days do not produce neighbouring slates.
    let seed = (day as u64)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let start = (seed % count as u64) as usize;

    let mut stride = ((seed >> 32) % count as u64) as usize + 1;
    while gcd(stride, count) != 1 {
        stride += 1;
    }

    (0..wanted)
        .map(|step| pool[(start + stride * step) % count])
        .collect()
}

/// Everything a trophy predicate is allowed to see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Award key → times performed.
    pub counts: std::collections::BTreeMap<String, i64>,
    pub level: u32,
    pub streak: i64,
    pub load: i64,
    pub directives_completed: i64,
}

impl Snapshot {
    pub fn count(&self, award: Award) -> i64 {
        self.counts.get(award.key()).copied().unwrap_or(0)
    }
}

/// What the trophy is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    Award(Award),
    Level,
    Streak,
    Directives,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Bronze,
    Silver,
    Gold,
    Machine,
}

/// A trophy is a threshold over one counter.
///
/// Deliberately data rather than a closure: it has to cross an FFI-shaped
/// boundary into three clients, and a rule you can print is a rule two
/// implementations can be checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trophy {
    pub id: &'static str,
    pub name: &'static str,
    pub dossier: &'static str,
    pub tier: Tier,
    pub measure: Measure,
    pub goal: i64,
}

pub const TROPHIES: [Trophy; 13] = [
    Trophy {
        id: "first-contact",
        name: "FIRST CONTACT",
        dossier: "Spoke to the network and it answered.",
        tier: Tier::Bronze,
        measure: Measure::Award(Award::Transmission),
        goal: 1,
    },
    Trophy {
        id: "chatterbox",
        name: "SIGNAL FLOOD",
        dossier: "One hundred transmissions. The bandwidth noticed.",
        tier: Tier::Silver,
        measure: Measure::Award(Award::Transmission),
        goal: 100,
    },
    Trophy {
        id: "ciphersmith",
        name: "CIPHERSMITH",
        dossier: "Fifty messages the server could not read.",
        tier: Tier::Gold,
        measure: Measure::Award(Award::EncryptedTransmission),
        goal: 50,
    },
    Trophy {
        id: "architect",
        name: "CHANNEL ARCHITECT",
        dossier: "Ten channels forged out of nothing.",
        tier: Tier::Silver,
        measure: Measure::Award(Award::ChannelForged),
        goal: 10,
    },
    Trophy {
        id: "interrogator",
        name: "INTERROGATOR",
        dossier: "Fifty questions put to the core. It kept count.",
        tier: Tier::Silver,
        measure: Measure::Award(Award::AgentQueried),
        goal: 50,
    },
    Trophy {
        id: "eyes-open",
        name: "EYES OPEN",
        dossier: "Brought the optics online ten times.",
        tier: Tier::Bronze,
        measure: Measure::Award(Award::OpticsEngaged),
        goal: 10,
    },
    Trophy {
        id: "librarian",
        name: "LIBRARIAN",
        dossier: "Twenty five things committed to permanent memory.",
        tier: Tier::Gold,
        measure: Measure::Award(Award::KnowledgeStored),
        goal: 25,
    },
    Trophy {
        id: "recruiter",
        name: "RECRUITER",
        dossier: "Five operators brought into the network.",
        tier: Tier::Silver,
        measure: Measure::Award(Award::InvitationSent),
        goal: 5,
    },
    Trophy {
        id: "streak-7",
        name: "SEVEN DAY WATCH",
        dossier: "Reported in seven days running.",
        tier: Tier::Gold,
        measure: Measure::Streak,
        goal: 7,
    },
    Trophy {
        id: "streak-30",
        name: "PERPETUAL",
        dossier: "Thirty days. The network stopped expecting you to stop.",
        tier: Tier::Machine,
        measure: Measure::Streak,
        goal: 30,
    },
    Trophy {
        id: "rank-5",
        name: "HALF MACHINE",
        dossier: "Cleared the fifth tier.",
        tier: Tier::Silver,
        measure: Measure::Level,
        goal: 5,
    },
    Trophy {
        id: "rank-10",
        name: "SINGULARITY",
        dossier: "Topped the ladder. The operator is now a formality.",
        tier: Tier::Machine,
        measure: Measure::Level,
        goal: 10,
    },
    Trophy {
        id: "orders-25",
        name: "COMPLIANT",
        dossier: "Twenty five standing orders carried out.",
        tier: Tier::Gold,
        measure: Measure::Directives,
        goal: 25,
    },
];

impl Trophy {
    pub fn progress(&self, snapshot: &Snapshot) -> i64 {
        match self.measure {
            Measure::Award(award) => snapshot.count(award),
            Measure::Level => snapshot.level as i64,
            Measure::Streak => snapshot.streak,
            Measure::Directives => snapshot.directives_completed,
        }
    }

    pub fn earned(&self, snapshot: &Snapshot) -> bool {
        self.progress(snapshot) >= self.goal
    }

    /// 0.0…1.0. A progress bar must never exceed full however far past the
    /// goal someone runs.
    pub fn meter(&self, snapshot: &Snapshot) -> f64 {
        if self.goal <= 0 {
            return 1.0;
        }
        (self.progress(snapshot) as f64 / self.goal as f64).clamp(0.0, 1.0)
    }
}

pub fn earned_trophies(snapshot: &Snapshot) -> Vec<Trophy> {
    TROPHIES
        .into_iter()
        .filter(|trophy| trophy.earned(snapshot))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_and_thresholds_agree() {
        assert_eq!(LADDER.len(), THRESHOLDS.len());
        assert_eq!(THRESHOLDS[0], 0);
        let mut sorted = THRESHOLDS;
        sorted.sort_unstable();
        assert_eq!(THRESHOLDS, sorted, "thresholds must climb");
        for (index, rank) in LADDER.iter().enumerate() {
            assert_eq!(rank.level as usize, index + 1);
        }
    }

    #[test]
    fn rank_lands_on_the_right_rung() {
        assert_eq!(rank_for(0).level, 1);
        assert_eq!(rank_for(119).level, 1);
        // Exactly on a threshold is the new rank, not the old one.
        assert_eq!(rank_for(120).level, 2);
        assert_eq!(rank_for(15_800).level, 10);
        assert_eq!(rank_for(i64::MAX).level, 10, "the top rung is a ceiling");
        assert_eq!(rank_for(-9_999).level, 1, "a corrupt file reads as fresh");
    }

    #[test]
    fn fraction_runs_zero_to_one_and_fills_at_the_top() {
        assert!((fraction_for(0) - 0.0).abs() < 1e-9);
        assert!((fraction_for(220) - 0.5).abs() < 1e-9);
        assert!((fraction_for(20_000) - 1.0).abs() < 1e-9);
        assert!((fraction_for(-1) - 0.0).abs() < 1e-9);
        assert!(ceiling_for(MAX_LEVEL).is_none());
    }

    #[test]
    fn streaks_extend_reset_and_survive_a_backwards_clock() {
        assert_eq!(advance_streak(0, None, 100), 1);
        assert_eq!(advance_streak(3, Some(100), 100), 3, "same day is one day");
        assert_eq!(advance_streak(3, Some(99), 100), 4);
        assert_eq!(
            advance_streak(9, Some(96), 100),
            1,
            "a missed day starts over"
        );
        assert_eq!(
            advance_streak(12, Some(102), 100),
            12,
            "a clock that jumped back costs nothing"
        );
    }

    #[test]
    fn streak_multiplier_rises_then_caps() {
        assert!((streak_multiplier(1) - 1.0).abs() < 1e-9);
        assert!((streak_multiplier(3) - 1.30).abs() < 1e-9);
        assert!((streak_multiplier(1_000) - 2.5).abs() < 1e-9);
    }

    #[test]
    fn the_day_turns_over_at_local_midnight() {
        let offset = 9 * 3600; // Asia/Seoul
        let morning = 1_754_268_000_000; // some instant
        let same_day_later = morning + 3_600_000;
        assert_eq!(
            day_index(morning, offset),
            day_index(same_day_later, offset)
        );
        assert_eq!(
            day_index(morning + 86_400_000, offset),
            day_index(morning, offset) + 1
        );
    }

    #[test]
    fn the_slate_is_three_distinct_orders_every_day() {
        for day in 0..500 {
            let slate = slate_for_day(day);
            assert_eq!(slate.len(), SLATE_SIZE, "day {day}");
            let mut ids: Vec<_> = slate.iter().map(|d| d.id).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), SLATE_SIZE, "day {day} repeated an order");
        }
    }

    #[test]
    fn the_slate_is_stable_for_a_day_and_moves_between_days() {
        let a = slate_for_day(20_000);
        assert_eq!(a, slate_for_day(20_000));
        assert_ne!(a, slate_for_day(20_001));
    }

    #[test]
    fn every_directive_beats_grinding_its_own_award() {
        let mut ids: Vec<_> = DIRECTIVE_POOL.iter().map(|d| d.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), DIRECTIVE_POOL.len(), "ids must be unique");
        for directive in DIRECTIVE_POOL {
            assert!(directive.goal > 0);
            assert!(directive.bounty > directive.award.load());
        }
    }

    #[test]
    fn awards_are_worth_something_and_encryption_is_never_the_poorer_choice() {
        for award in Award::ALL {
            assert!(award.load() > 0, "{:?}", award);
            assert!(!award.citation().is_empty());
            assert!(award.weight() > 0.0 && award.weight() <= 1.0);
            assert_eq!(Award::from_key(award.key()), Some(award));
        }
        assert!(Award::EncryptedTransmission.load() > Award::Transmission.load());
        assert!(Award::EncryptedChannelForged.load() > Award::ChannelForged.load());
    }

    #[test]
    fn nothing_is_earned_on_an_empty_file_and_everything_on_a_full_one() {
        assert!(earned_trophies(&Snapshot::default()).is_empty());

        let mut full = Snapshot {
            level: MAX_LEVEL,
            streak: 400,
            load: 99_999,
            directives_completed: 5_000,
            ..Snapshot::default()
        };
        for award in Award::ALL {
            full.counts.insert(award.key().to_string(), 100_000);
        }
        assert_eq!(earned_trophies(&full).len(), TROPHIES.len());
        for trophy in TROPHIES {
            let meter = trophy.meter(&full);
            assert!((0.0..=1.0).contains(&meter), "{} overflowed", trophy.id);
        }
    }

    #[test]
    fn trophy_ids_are_unique() {
        let mut ids: Vec<_> = TROPHIES.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), TROPHIES.len());
    }
}
