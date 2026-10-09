//! Storage selection for the API.
//!
//! The repository trait uses native `async fn`, which is not dyn-compatible, so
//! we dispatch over a small enum instead of a trait object. `InMemory` backs
//! tests and local runs with no database; `Pg` backs a configured Postgres.
//! Both arms call the same `CampaignRepo` implementation.

use ad_campaign::repo::{CampaignRepo, InMemoryCampaignRepo, NewCampaign, RepoError};
use ad_campaign::Campaign;
use ad_core::{CampaignId, LeadId, OrgId};
use ad_db::PgCampaignRepo;
use ad_db::PgLeadRepo;
use ad_lead::repo::{InMemoryLeadRepo, LeadRepo, NewLead, RepoError as LeadRepoError};
use ad_lead::Lead;

#[derive(Clone)]
pub enum CampaignStore {
    InMemory(std::sync::Arc<InMemoryCampaignRepo>),
    Pg(PgCampaignRepo),
}

impl CampaignStore {
    pub fn in_memory() -> Self {
        CampaignStore::InMemory(std::sync::Arc::new(InMemoryCampaignRepo::new()))
    }

    pub async fn create(&self, new: NewCampaign) -> Result<Campaign, RepoError> {
        match self {
            CampaignStore::InMemory(r) => r.create(new).await,
            CampaignStore::Pg(r) => r.create(new).await,
        }
    }

    pub async fn get(&self, id: CampaignId) -> Result<Campaign, RepoError> {
        match self {
            CampaignStore::InMemory(r) => r.get(id).await,
            CampaignStore::Pg(r) => r.get(id).await,
        }
    }

    pub async fn list(&self, org_id: OrgId) -> Result<Vec<Campaign>, RepoError> {
        match self {
            CampaignStore::InMemory(r) => r.list(org_id).await,
            CampaignStore::Pg(r) => r.list(org_id).await,
        }
    }

    pub async fn save(&self, campaign: Campaign) -> Result<Campaign, RepoError> {
        match self {
            CampaignStore::InMemory(r) => r.save(campaign).await,
            CampaignStore::Pg(r) => r.save(campaign).await,
        }
    }
}

/// Storage selection for leads, mirroring `CampaignStore`.
#[derive(Clone)]
pub enum LeadStore {
    InMemory(std::sync::Arc<InMemoryLeadRepo>),
    Pg(PgLeadRepo),
}

impl LeadStore {
    pub fn in_memory() -> Self {
        LeadStore::InMemory(std::sync::Arc::new(InMemoryLeadRepo::new()))
    }

    pub async fn bulk_create(&self, new: Vec<NewLead>) -> Result<Vec<Lead>, LeadRepoError> {
        match self {
            LeadStore::InMemory(r) => r.bulk_create(new).await,
            LeadStore::Pg(r) => r.bulk_create(new).await,
        }
    }

    pub async fn list_by_campaign(
        &self,
        campaign_id: CampaignId,
    ) -> Result<Vec<Lead>, LeadRepoError> {
        match self {
            LeadStore::InMemory(r) => r.list_by_campaign(campaign_id).await,
            LeadStore::Pg(r) => r.list_by_campaign(campaign_id).await,
        }
    }

    pub async fn get(&self, id: LeadId) -> Result<Lead, LeadRepoError> {
        match self {
            LeadStore::InMemory(r) => r.get(id).await,
            LeadStore::Pg(r) => r.get(id).await,
        }
    }
}
