//! Shared same-team tactical knowledge for the G0 state bus.
//!
//! The reducer is deliberately independent of networking and map loading. A
//! client turns its observations into [`TeamReport`] values, then feeds reports
//! from its own team into [`TeamSnapshot`].

use crate::world::Team;

/// The bomb-site belief shared by a team.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlantSite {
    Unknown,
    A,
    B,
}

/// A compact observation published by one bot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TeamReport {
    pub bot_id: u16,
    pub team: Team,
    pub alive: bool,
    pub origin: [f32; 3],
    /// Current tactical site, if assigned. `None` means mid/flex/unknown.
    pub assigned_site: Option<PlantSite>,
    /// Last site where this bot observed an enemy contact.
    pub contact_site: Option<PlantSite>,
    pub contact_at: Option<f32>,
    pub bomb_carrier: bool,
    pub bomb_planted: bool,
    pub bomb_origin: Option<[f32; 3]>,
    pub observed_at: f32,
    pub role: u8,
    pub rung: u8,
}

/// A bounded report cache and derived same-team tactical belief.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamSnapshot {
    team: Team,
    reports: Vec<TeamReport>,
    pub alive_a: u16,
    pub alive_b: u16,
    pub alive_mid: u16,
    pub pressure: PlantSite,
    pub plant_site: PlantSite,
    pub plant_origin: Option<[f32; 3]>,
    pub updated_at: f32,
}

impl TeamSnapshot {
    /// Create an empty snapshot for one playing team.
    pub fn new(team: Team) -> Self {
        Self {
            team,
            reports: Vec::new(),
            alive_a: 0,
            alive_b: 0,
            alive_mid: 0,
            pressure: PlantSite::Unknown,
            plant_site: PlantSite::Unknown,
            plant_origin: None,
            updated_at: 0.0,
        }
    }

    pub fn team(&self) -> Team {
        self.team
    }

    pub fn reports(&self) -> &[TeamReport] {
        &self.reports
    }

    /// Apply one report and recompute the derived tactical state.
    pub fn apply(&mut self, report: TeamReport, sites: &[[f32; 3]; 2]) {
        if report.team != self.team {
            return;
        }
        if let Some(existing) = self
            .reports
            .iter_mut()
            .find(|current| current.bot_id == report.bot_id)
        {
            *existing = report;
        } else {
            self.reports.push(report);
        }
        self.updated_at = self.updated_at.max(report.observed_at);
        self.recompute(sites);
    }

    fn recompute(&mut self, sites: &[[f32; 3]; 2]) {
        self.alive_a = 0;
        self.alive_b = 0;
        self.alive_mid = 0;
        self.pressure = PlantSite::Unknown;
        let mut contact_at = [f32::NEG_INFINITY; 2];
        let mut contact_count = [0u16; 2];
        let mut planted_origin = None;
        let mut planted = false;

        for report in &self.reports {
            if report.alive {
                match report.assigned_site {
                    Some(PlantSite::A) => self.alive_a += 1,
                    Some(PlantSite::B) => self.alive_b += 1,
                    _ => self.alive_mid += 1,
                }
            }
            if let Some(site) = report.contact_site {
                let i = site_index(site);
                contact_count[i] = contact_count[i].saturating_add(1);
                if let Some(at) = report.contact_at {
                    contact_at[i] = contact_at[i].max(at);
                }
            }
            planted |= report.bomb_planted;
            if planted_origin.is_none() {
                planted_origin = report.bomb_origin;
            }
        }

        if contact_count[0] > contact_count[1] && contact_count[0] >= 2 {
            self.pressure = PlantSite::A;
        } else if contact_count[1] > contact_count[0] && contact_count[1] >= 2 {
            self.pressure = PlantSite::B;
        }

        if planted {
            let origin = planted_origin.or(self.plant_origin);
            self.plant_origin = origin;
            self.plant_site = origin
                .map(|position| nearest_site(position, sites))
                .unwrap_or(self.plant_site);
        } else {
            self.plant_origin = None;
            self.plant_site = PlantSite::Unknown;
        }

        // Ignore stale contact timestamps when deriving pressure. The counts
        // above intentionally remain simple: a report is one bot's latest
        // belief, not a stream of repeated observations.
        let _ = contact_at;
    }
}

fn site_index(site: PlantSite) -> usize {
    match site {
        PlantSite::A => 0,
        PlantSite::B => 1,
        PlantSite::Unknown => 0,
    }
}

fn nearest_site(origin: [f32; 3], sites: &[[f32; 3]; 2]) -> PlantSite {
    let distance = |site: [f32; 3]| {
        let dx = origin[0] - site[0];
        let dy = origin[1] - site[1];
        let dz = origin[2] - site[2];
        dx * dx + dy * dy + dz * dz
    };
    if distance(sites[0]) <= distance(sites[1]) {
        PlantSite::A
    } else {
        PlantSite::B
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(
        bot_id: u16,
        alive: bool,
        assigned_site: Option<PlantSite>,
        contact_site: Option<PlantSite>,
    ) -> TeamReport {
        TeamReport {
            bot_id,
            team: Team::CounterTerrorist,
            alive,
            origin: [0.0; 3],
            assigned_site,
            contact_site,
            contact_at: contact_site.map(|_| 1.0),
            bomb_carrier: false,
            bomb_planted: false,
            bomb_origin: None,
            observed_at: 1.0,
            role: 0,
            rung: 0,
        }
    }

    #[test]
    fn five_ct_reports_and_b_site_wipe_set_pressure_to_b() {
        let sites = [[1000.0, 1000.0, 100.0], [-1500.0, 2600.0, 48.0]];
        let mut snapshot = TeamSnapshot::new(Team::CounterTerrorist);
        snapshot.apply(report(1, true, Some(PlantSite::A), Some(PlantSite::B)), &sites);
        snapshot.apply(report(2, true, Some(PlantSite::A), Some(PlantSite::B)), &sites);
        snapshot.apply(report(3, false, Some(PlantSite::B), None), &sites);
        snapshot.apply(report(4, false, Some(PlantSite::B), None), &sites);
        snapshot.apply(report(5, true, None, None), &sites);
        assert_eq!(snapshot.alive_a, 2);
        assert_eq!(snapshot.alive_b, 0);
        assert_eq!(snapshot.alive_mid, 1);
        assert_eq!(snapshot.pressure, PlantSite::B);
    }

    #[test]
    fn planted_a_origin_is_inherited_by_late_joiner_reports() {
        let sites = [[1000.0, 1000.0, 100.0], [-1500.0, 2600.0, 48.0]];
        let origin = [1040.0, 980.0, 100.0];
        let mut snapshot = TeamSnapshot::new(Team::CounterTerrorist);
        let mut planter = report(7, true, Some(PlantSite::A), None);
        planter.bomb_planted = true;
        planter.bomb_origin = Some(origin);
        snapshot.apply(planter, &sites);
        let mut late = report(8, true, Some(PlantSite::B), None);
        late.bomb_planted = true;
        snapshot.apply(late, &sites);
        assert_eq!(snapshot.plant_site, PlantSite::A);
        assert_eq!(snapshot.plant_origin, Some(origin));
    }

    #[test]
    fn reports_from_other_teams_are_ignored() {
        let sites = [[0.0; 3], [100.0, 0.0, 0.0]];
        let mut snapshot = TeamSnapshot::new(Team::CounterTerrorist);
        let mut enemy = report(1, true, Some(PlantSite::A), Some(PlantSite::A));
        enemy.team = Team::Terrorist;
        snapshot.apply(enemy, &sites);
        assert!(snapshot.reports().is_empty());
    }
}
