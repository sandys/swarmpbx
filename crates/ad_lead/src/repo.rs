//! Lead persistence boundary. `LeadRepo` is what the API depends on;
//! `InMemoryLeadRepo` backs tests and local runs. A Postgres implementation
//! (ad_db) implements the same trait, so the API does not change.

use std::collections::BTreeMap;
use std::sync::Mutex;

use ad_core::{CampaignId, LeadId};

use crate::{Lead, LeadStatus};

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("lead {0} not found")]
    NotFound(LeadId),
    #[error("storage error: {0}")]
    Storage(String),
}

/// Input to create a lead. The repo assigns id/timestamps and defaults status
/// to `New` and `called_count` to 0.
#[derive(Debug, Clone)]
pub struct NewLead {
    pub campaign_id: CampaignId,
    pub phone_number: String,
    pub timezone: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub context: BTreeMap<String, String>,
}

pub trait LeadRepo: Send + Sync {
    fn create(
        &self,
        new: NewLead,
    ) -> impl std::future::Future<Output = Result<Lead, RepoError>> + Send;

    /// Bulk-load leads (one campaign's contact list). Returns the created leads.
    fn bulk_create(
        &self,
        new: Vec<NewLead>,
    ) -> impl std::future::Future<Output = Result<Vec<Lead>, RepoError>> + Send;

    fn get(&self, id: LeadId) -> impl std::future::Future<Output = Result<Lead, RepoError>> + Send;

    fn list_by_campaign(
        &self,
        campaign_id: CampaignId,
    ) -> impl std::future::Future<Output = Result<Vec<Lead>, RepoError>> + Send;
}

#[derive(Default)]
pub struct InMemoryLeadRepo {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: i64,
    by_id: std::collections::HashMap<i64, Lead>,
}

impl InMemoryLeadRepo {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert_locked(inner: &mut Inner, new: NewLead) -> Lead {
        let now = chrono::Utc::now();
        inner.next_id += 1;
        let lead = Lead {
            id: LeadId(inner.next_id),
            campaign_id: new.campaign_id,
            phone_number: new.phone_number,
            timezone: new.timezone,
            state: new.state,
            postal_code: new.postal_code,
            status: LeadStatus::New,
            called_count: 0,
            last_local_call_time: None,
            context: new.context,
            created_at: now,
            updated_at: now,
        };
        inner.by_id.insert(lead.id.0, lead.clone());
        lead
    }
}

impl LeadRepo for InMemoryLeadRepo {
    async fn create(&self, new: NewLead) -> Result<Lead, RepoError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| RepoError::Storage(e.to_string()))?;
        Ok(Self::insert_locked(&mut inner, new))
    }

    async fn bulk_create(&self, new: Vec<NewLead>) -> Result<Vec<Lead>, RepoError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| RepoError::Storage(e.to_string()))?;
        Ok(new
            .into_iter()
            .map(|n| Self::insert_locked(&mut inner, n))
            .collect())
    }

    async fn get(&self, id: LeadId) -> Result<Lead, RepoError> {
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

    async fn list_by_campaign(&self, campaign_id: CampaignId) -> Result<Vec<Lead>, RepoError> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| RepoError::Storage(e.to_string()))?;
        let mut out: Vec<Lead> = inner
            .by_id
            .values()
            .filter(|l| l.campaign_id == campaign_id)
            .cloned()
            .collect();
        out.sort_by_key(|l| l.id.0);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nl(campaign: i64, phone: &str) -> NewLead {
        NewLead {
            campaign_id: CampaignId(campaign),
            phone_number: phone.into(),
            timezone: Some("America/New_York".into()),
            state: Some("NY".into()),
            postal_code: Some("10001".into()),
            context: BTreeMap::new(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_and_get() {
        let repo = InMemoryLeadRepo::new();
        let lead = repo.create(nl(1, "+13125550100")).await.unwrap();
        assert_eq!(lead.status, LeadStatus::New);
        assert_eq!(lead.called_count, 0);
        assert_eq!(repo.get(lead.id).await.unwrap(), lead);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bulk_create_and_list_scoped() {
        let repo = InMemoryLeadRepo::new();
        repo.bulk_create(vec![nl(1, "+1a"), nl(1, "+1b"), nl(2, "+2a")])
            .await
            .unwrap();
        assert_eq!(repo.list_by_campaign(CampaignId(1)).await.unwrap().len(), 2);
        assert_eq!(repo.list_by_campaign(CampaignId(2)).await.unwrap().len(), 1);
        assert!(repo
            .list_by_campaign(CampaignId(3))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_missing_is_not_found() {
        let repo = InMemoryLeadRepo::new();
        assert!(matches!(
            repo.get(LeadId(42)).await,
            Err(RepoError::NotFound(LeadId(42)))
        ));
    }
}
