use aws_config::SdkConfig;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{SignableRequest, sign};
use aws_sigv4::sign::v4;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::url::events_host;

#[derive(Clone, PartialEq, Eq)]
pub struct LambdaToken(String);

/// Authentication type for AppSync
#[derive(Clone)]
pub enum AuthType<'a> {
    /// IAM authentication
    Iam {
        app_id: String,
        region: String,
        config: &'a SdkConfig,
    },
    /// Lambda authentication
    Lambda(LambdaToken),
    /// API Key authentication
    ApiKey {
        app_id: String,
        region: String,
        key: String,
    },
}

impl<'a> AuthType<'a> {
    pub async fn new_iam(app_id: String, region: String, config: &'a SdkConfig) -> Self {
        Self::Iam {
            app_id,
            region,
            config,
        }
    }

    pub fn new_lambda(token: String) -> Self {
        Self::Lambda(LambdaToken(token))
    }

    pub async fn get_auth_headers(&self, payload: &str) -> Result<HashMap<String, String>> {
        match self {
            Self::Iam {
                app_id,
                region,
                config,
            } => {
                let credentials = config
                    .credentials_provider()
                    .ok_or(Error::Authentication(
                        "credentials are required to create a signed URL for DSQL".to_string(),
                    ))?
                    .provide_credentials()
                    .await
                    .map_err(|e| Error::Authentication(e.to_string()))?;

                let identity = Arc::new(credentials.into());
                // Reference:
                // https://github.com/aws-amplify/amplify-js/blob/3c4d29e37797e6bec73db534278ed5ffd3c5d7e5/packages/api-graphql/src/Providers/AWSWebSocketProvider/authHeaders.ts#L43-L78
                let signable_request = SignableRequest::new(
                    "POST",
                    crate::url::events(app_id, region),
                    // https://github.com/aws-amplify/amplify-js/blob/3c4d29e37797e6bec73db534278ed5ffd3c5d7e5/packages/api-graphql/src/Providers/AWSWebSocketProvider/authHeaders.ts#L43-L78
                    [
                        ("accept", "application/json, text/javascript"),
                        ("content-encoding", "amz-1.0"),
                        ("content-type", "application/json; charset=UTF-8"),
                    ]
                    .into_iter(),
                    aws_sigv4::http_request::SignableBody::Bytes(payload.as_bytes()),
                )
                .map_err(|e| {
                    Error::AwsSigning(format!("Failed to create signable request: {}", e))
                })?;

                let signing_params = v4::SigningParams::builder()
                    .identity(&identity)
                    .region(region)
                    .name("appsync") // service
                    .time(SystemTime::now())
                    .settings(Default::default())
                    .build()
                    .map_err(|e| {
                        Error::AwsSigning(format!("Failed to build signing params: {}", e))
                    })?;

                let signing_params = aws_sigv4::http_request::SigningParams::from(signing_params);
                let signing_result = sign(signable_request, &signing_params)
                    .map_err(|e| Error::AwsSigning(format!("Failed to sign request: {}", e)))?;

                // Extract the signed headers
                let mut result_headers = HashMap::new();
                for (name, value) in signing_result.output().headers() {
                    result_headers.insert(name.to_string(), value.to_string());
                }

                Ok(result_headers)
            }
            Self::Lambda(lambda_token) => {
                let mut headers = HashMap::new();
                headers.insert("Authorization".to_string(), lambda_token.0.clone());

                Ok(headers)
            }
            Self::ApiKey { app_id, region, key } => {
                let mut headers = HashMap::new();
                headers.insert("host".to_string(), events_host(app_id, region));
                headers.insert("x-api-key".to_string(), key.clone());
                Ok(headers)
            }
        }
    }
}

// We only care about variants, not value equality
impl PartialEq for AuthType<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AuthType::Iam { .. }, AuthType::Iam { .. }) => true,
            (AuthType::Lambda(l), AuthType::Lambda(r)) => l == r,
            _ => false,
        }
    }
}
