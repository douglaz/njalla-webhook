pub mod client;
pub mod types;

pub use client::Client;
pub use types::*;

use crate::error::Result;

#[async_trait::async_trait]
pub trait DomainLister: Send + Sync {
    async fn list_domains(&self) -> Result<Vec<Domain>>;
}

#[async_trait::async_trait]
impl DomainLister for Client {
    async fn list_domains(&self) -> Result<Vec<Domain>> {
        Client::list_domains(self).await
    }
}
