//! Conservative public acog publisher adapter.
pub struct Acog;
#[async_trait::async_trait]
impl super::Adapter for Acog {
 async fn discover(&self,s:&crate::guidelines::registry::SourceEntry,f:&crate::guidelines::fetch::Fetcher,l:Option<usize>)->anyhow::Result<Vec<super::PlanRow>> { super::Adapter::discover(&super::publisher_article::PublisherAdapter(super::publisher_article::PublisherSource::Acog),s,f,l).await }
 async fn extract(&self,s:&crate::guidelines::registry::SourceEntry,f:&crate::guidelines::fetch::Fetcher,r:&super::PlanRow)->anyhow::Result<crate::guidelines::doc::GuidelineDoc> { super::Adapter::extract(&super::publisher_article::PublisherAdapter(super::publisher_article::PublisherSource::Acog),s,f,r).await }
}
