//! Conservative public asco publisher adapter.
pub struct Asco;
#[async_trait::async_trait]
impl super::Adapter for Asco {
 async fn discover(&self,s:&crate::guidelines::registry::SourceEntry,f:&crate::guidelines::fetch::Fetcher,l:Option<usize>)->anyhow::Result<Vec<super::PlanRow>> { super::Adapter::discover(&super::publisher_article::PublisherAdapter(super::publisher_article::PublisherSource::Asco),s,f,l).await }
 async fn extract(&self,s:&crate::guidelines::registry::SourceEntry,f:&crate::guidelines::fetch::Fetcher,r:&super::PlanRow)->anyhow::Result<crate::guidelines::doc::GuidelineDoc> { super::Adapter::extract(&super::publisher_article::PublisherAdapter(super::publisher_article::PublisherSource::Asco),s,f,r).await }
}
