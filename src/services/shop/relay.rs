//! Relay-point delivery (Mondial Relay, Colissimo pick-up points). Carrier APIs are out of
//! scope: [`RelayPointProvider`] is the seam, [`FakeRelayPoints`] answers with deterministic
//! points around the searched postal code (development, tests, and until a carrier contract
//! exists). The chosen point is copied on the cart, the order and the fulfilment.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayPoint {
    /// `<carrier>:<carrier id>`.
    pub id: String,
    pub carrier: String,
    pub name: String,
    pub line1: String,
    pub postal_code: String,
    pub city: String,
    pub country_code: String,
    #[serde(default)]
    pub opening_hours: Vec<String>,
}

#[async_trait]
pub trait RelayPointProvider: Send + Sync {
    /// Points near a postal code (at most 10).
    async fn search(
        &self,
        carrier: &str,
        country_code: &str,
        postal_code: &str,
    ) -> anyhow::Result<Vec<RelayPoint>>;
    /// One point by id, `None` when unknown (checked again at checkout).
    async fn get(&self, id: &str) -> anyhow::Result<Option<RelayPoint>>;
}

/// Deterministic points: `<carrier>:<postal code>-<n>` for n in 1..=5.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeRelayPoints;

const NAMES: [&str; 5] = [
    "Tabac de la Place",
    "Épicerie du Marché",
    "Librairie Centrale",
    "Pressing des Halles",
    "Fleuriste du Parc",
];

fn point(carrier: &str, country: &str, postal: &str, n: usize) -> RelayPoint {
    RelayPoint {
        id: format!("{carrier}:{postal}-{n}"),
        carrier: carrier.to_string(),
        name: NAMES[(n - 1) % NAMES.len()].to_string(),
        line1: format!("{} rue de la République", n * 3),
        postal_code: postal.to_string(),
        city: "Ville (test)".into(),
        country_code: country.to_string(),
        opening_hours: vec!["Lun–Sam 9:00–19:00".into()],
    }
}

pub fn valid_fr_postal(p: &str) -> bool {
    p.len() == 5 && p.chars().all(|c| c.is_ascii_digit())
}

#[async_trait]
impl RelayPointProvider for FakeRelayPoints {
    async fn search(
        &self,
        carrier: &str,
        country_code: &str,
        postal_code: &str,
    ) -> anyhow::Result<Vec<RelayPoint>> {
        let (country, postal) = (country_code.trim().to_ascii_uppercase(), postal_code.trim());
        if country != "FR" || !valid_fr_postal(postal) {
            return Ok(vec![]);
        }
        Ok((1..=5)
            .map(|n| point(carrier, &country, postal, n))
            .collect())
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<RelayPoint>> {
        let Some((carrier, rest)) = id.split_once(':') else {
            return Ok(None);
        };
        let Some((postal, n)) = rest.split_once('-') else {
            return Ok(None);
        };
        let n: usize = match n.parse() {
            Ok(n) if (1..=5).contains(&n) => n,
            _ => return Ok(None),
        };
        if !valid_fr_postal(postal) || carrier.is_empty() {
            return Ok(None);
        }
        Ok(Some(point(carrier, "FR", postal, n)))
    }
}

/// The provider in use (only the fake one exists for now).
pub fn provider() -> &'static dyn RelayPointProvider {
    &FakeRelayPoints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_points_round_trip() {
        let p = FakeRelayPoints
            .search("mondial_relay", "fr", "75011")
            .await
            .unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(
            FakeRelayPoints.get(&p[2].id).await.unwrap().as_ref(),
            Some(&p[2])
        );
        assert!(
            FakeRelayPoints
                .search("mondial_relay", "FR", "7501")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            FakeRelayPoints
                .get("mondial_relay:75011-9")
                .await
                .unwrap()
                .is_none()
        );
    }
}
