//! Campaign persistence boundary.
//!
//! `CampaignRepo` is the trait the API depends on. `InMemoryCampaignRepo` is a
//! working store for development and tests. A Postgres implementation lands in
//! a later slice and implements the same trait, so the API does not change.

use std::collections::HashMap;
use std::sync::Mutex;

use ad_core::{CampaignId, OrgId};

use crate::Campaign;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("campaign {0} not found")]
    NotFound(CampaignId),
    #[error("storage error: {0}")]
    Storage(String),
}

/// What a caller supplies to create a campaign. The repo assigns the id and the
/// timestamps.
#[derive(Debug, Clone)]
pub struct NewCampaign {
    pub org_id: OrgId,
    pub name: String,
}

/// The persistence boundary. Native `async fn` in trait (Rust >= 1.75).
pub trait CampaignRepo: Send + Sync {
    fn create(
        &self,
        new: NewCampaign,
    ) -> impl std::future::Future<Output = Result<Campaign, RepoError>> + Send;

    fn get(
        &self,
        id: CampaignId,
    ) -> impl std::future::Future<Output = Result<Campaign, RepoError>> + Send;

    fn list(
        &self,
        org_id: OrgId,
    ) -> impl std::future::Future<Output = Result<Vec<Campaign>, RepoError>> + Send;

    /// Persist a mutated aggregate (for example after a lifecycle transition).
    fn save(
        &self,
        campaign: Campaign,
    ) -> impl std::future::Future<Output = Result<Campaign, RepoError>> + Send;
}

/// In-memory store. Thread-safe. For development and tests only.
#[derive(Default)]
pub struct InMemoryCampaignRepo {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: i64,
    by_id: HashMap<i64, Campaign>,
}

impl InMemoryCampaignRepo {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CampaignRepo for InMemoryCampaignRepo {
    async fn create(&self, new: NewCampaign) -> Result<Campaign, RepoError> {
        let now = chrono::Utc::now();
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| RepoError::Storage(e.to_string()))?;
        inner.next_id += 1;
        let id = CampaignId(inner.next_id);
        let campaign = Campaign::new(id, new.org_id, new.name, now);
        inner.by_id.insert(id.0, campaign.clone());
        Ok(campaign)
    }

    async fn get(&self, id: CampaignId) -> Result<Campaign, RepoError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| RepoError::Storage(e.to_string()))?;
        inner
            .by_id
            .get(&id.0)
            .cloned()
            .ok_or(RepoError::NotFound(id))
    }

    async fn list(&self, org_id: OrgId) -> Result<Vec<Campaign>, RepoError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| RepoError::Storage(e.to_string()))?;
        let mut out: Vec<Campaign> = inner
            .by_id
            .values()
            .filter(|c| c.org_id == org_id)
            .cloned()
            .collect();
        // Deterministic order for stable API output and tests.
        out.sort_by_key(|c| c.id.0);
        Ok(out)
    }

    async fn save(&self, campaign: Campaign) -> Result<Campaign, RepoError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| RepoError::Storage(e.to_string()))?;
        if !inner.by_id.contains_key(&campaign.id.0) {
            return Err(RepoError::NotFound(campaign.id));
        }
        inner.by_id.insert(campaign.id.0, campaign.clone());
        Ok(campaign)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn create_get_list_roundtrip() {
        let repo = InMemoryCampaignRepo::new();
        let c = repo
            .create(NewCampaign {
                org_id: OrgId(7),
                name: "camp".into(),
            })
            .await
            .unwrap();
        assert_eq!(c.org_id, OrgId(7));

        let got = repo.get(c.id).await.unwrap();
        assert_eq!(got, c);

        let listed = repo.list(OrgId(7)).await.unwrap();
        assert_eq!(listed.len(), 1);

        // A different org sees nothing.
        assert!(repo.list(OrgId(8)).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_missing_is_not_found() {
        let repo = InMemoryCampaignRepo::new();
        let err = repo.get(CampaignId(999)).await.unwrap_err();
        assert!(matches!(err, RepoError::NotFound(CampaignId(999))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn save_requires_existing() {
        let repo = InMemoryCampaignRepo::new();
        let ghost = Campaign::new(CampaignId(5), OrgId(1), "x".into(), chrono::Utc::now());
        assert!(matches!(
            repo.save(ghost).await,
            Err(RepoError::NotFound(_))
        ));
    }
}
