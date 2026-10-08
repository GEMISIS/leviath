//! The Bedrock account's data retention, for `lev providers retention`.
//!
//! The type is compiled into every build, so a caller can name it whatever
//! providers the build has; only a build with the `bedrock` feature can open
//! one.

use super::{AccountRetention, ModelRetention};
#[cfg(feature = "bedrock")]
use crate::provider::HttpClient;
use crate::provider::Result;

/// Where a [`DataRetention`] sends its requests.
pub struct DataRetentionSpec {
    /// The Bedrock API key.
    pub api_key: String,
    /// The AWS region; `None` is the provider's default.
    pub region: Option<String>,
    /// A gateway in front of the runtime host.
    pub base_url: Option<String>,
    /// The control plane, when it is not the region's.
    pub control_url: Option<String>,
    /// The `bedrock-mantle` host, when it is not the region's.
    pub mantle_url: Option<String>,
}

/// A Bedrock account, opened to read and set its data retention.
pub struct DataRetention {
    #[cfg(feature = "bedrock")]
    provider: super::BedrockProvider,
    /// A build without Bedrock never makes one.
    #[cfg(not(feature = "bedrock"))]
    never: std::convert::Infallible,
}

impl DataRetention {
    /// The account `spec` names, reached with `client`. Only a build with
    /// Bedrock can open one.
    #[cfg(feature = "bedrock")]
    pub fn open(client: HttpClient, spec: DataRetentionSpec) -> Self {
        Self {
            provider: super::BedrockProvider::new(client, spec.api_key)
                .with_region(spec.region)
                .with_base_url(spec.base_url)
                .with_control_url(spec.control_url)
                .with_mantle_url(spec.mantle_url),
        }
    }

    /// The region requests go to.
    pub fn region(&self) -> &str {
        #[cfg(feature = "bedrock")]
        return self.provider.region();
        #[cfg(not(feature = "bedrock"))]
        match self.never {}
    }

    /// The account's data retention mode, read from the control plane;
    /// `None` when a gateway fronts it.
    pub async fn account_retention(&self) -> Result<Option<AccountRetention>> {
        #[cfg(feature = "bedrock")]
        return self.provider.account_retention().await;
        #[cfg(not(feature = "bedrock"))]
        match self.never {}
    }

    /// Set the account's data retention mode: `none` is zero retention.
    pub async fn set_account_retention(&self, mode: &str) -> Result<AccountRetention> {
        #[cfg(feature = "bedrock")]
        return self.provider.set_account_retention(mode).await;
        #[cfg(not(feature = "bedrock"))]
        match (self.never, mode).0 {}
    }

    /// Read which retention modes each model allows; answers how many models
    /// were read.
    pub async fn read_model_retention(&self) -> Result<usize> {
        #[cfg(feature = "bedrock")]
        return self.provider.read_model_retention().await;
        #[cfg(not(feature = "bedrock"))]
        match self.never {}
    }

    /// What the last read said per model, by bare id, sorted.
    pub fn model_retentions(&self) -> Vec<(String, ModelRetention)> {
        #[cfg(feature = "bedrock")]
        return self.provider.model_retentions();
        #[cfg(not(feature = "bedrock"))]
        match self.never {}
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod tests {
    use super::*;

    /// Behind a gateway neither the control plane nor the listing host is
    /// reached, so every call answers without a request.
    #[tokio::test]
    async fn an_account_behind_a_gateway_answers_without_a_request() {
        let retention = DataRetention::open(
            crate::provider::build_http_client(None).expect("a test client builds"),
            DataRetentionSpec {
                api_key: "ABSK-test".to_string(),
                region: Some("eu-west-1".to_string()),
                base_url: Some("https://gw.example/bedrock".to_string()),
                control_url: None,
                mantle_url: None,
            },
        );
        assert_eq!(retention.region(), "eu-west-1");
        assert!(retention.account_retention().await.unwrap().is_none());
        assert!(retention.set_account_retention("none").await.is_err());
        assert_eq!(retention.read_model_retention().await.unwrap(), 0);
        assert!(retention.model_retentions().is_empty());
    }
}
