use std::sync::Arc;

use anyhow::Context;
use http::Uri;
use http::{Response, StatusCode};

use hyper::{Body, Client};

use super::authentication_flow::AuthResponse;

#[derive(Debug, Clone)]
pub struct BearerConfig {
    pub realm: Uri,
    pub service: String,
    pub scope: Option<String>,
}
impl std::fmt::Display for BearerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ realm: {}, service: {}, scope: {} }}",
            self.realm,
            self.service,
            self.scope.as_ref().map(|e| e.as_str()).unwrap_or("")
        )
    }
}

impl BearerConfig {
    pub fn from_auth_header(auth_header: &str) -> anyhow::Result<Self> {
        let mut realm = None;
        let mut scope = None;
        let mut service = None;

        let mut auth_header = auth_header
            .strip_prefix("Bearer")
            .ok_or_else(|| anyhow::anyhow!("Invalid auth header"))?;
        auth_header = auth_header.trim_start_matches(' ');
        for part in auth_header.split(',') {
            let mut part = part.split('=');
            let key = part
                .next()
                .ok_or_else(|| anyhow::anyhow!("Invalid auth header"))?
                .trim();
            let value = part
                .next()
                .ok_or_else(|| anyhow::anyhow!("Invalid auth header"))?
                .trim()
                .trim_matches('"');
            match key {
                "realm" => {
                    realm = Some(
                        value
                            .parse()
                            .with_context(|| format!("Failed to parse realm from {:?}", value))?,
                    )
                }
                "service" => service = Some(value.to_string()),
                "scope" => scope = Some(value.to_string()),
                _ => (),
            }
        }

        match (realm, service) {
            (Some(realm), Some(service)) => Ok(Self {
                realm,
                service,
                scope,
            }),
            _ => Err(anyhow::anyhow!("Invalid auth header")),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum RequestFailType {
    #[error("Failed to connect: '{0}'")]
    ConnectError(hyper::Error),
    #[error("Generic hyper error: '{0}'")]
    HyperError(hyper::Error),
    #[error("Internal error: '{0}'")]
    AnyhowError(anyhow::Error),
    #[error("Auth failed: '{0}'")]
    AuthFailure(BearerConfig),
    #[error("Got a redirection code: '{0}'")]
    Redirection(String),
}
impl From<anyhow::Error> for RequestFailType {
    fn from(e: anyhow::Error) -> Self {
        RequestFailType::AnyhowError(e)
    }
}
pub async fn run_single_request<F, Fut, B>(
    auth_info: Arc<tokio::sync::Mutex<Option<AuthResponse>>>,
    uri: &Uri,
    context: B,
    complete_uri: F,
    inner_client: &Client<hyper_rustls::HttpsConnector<hyper::client::HttpConnector>>,
) -> Result<Response<Body>, RequestFailType>
where
    F: Fn(B, http::request::Builder) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<http::request::Request<Body>>>,
    B: Send + 'static + Sync,
{
    let req_builder = http::request::Builder::default().uri(uri);

    let li = auth_info.lock().await;
    let auth_token = li.as_ref().and_then(|e| e.token.clone());
    drop(li);
    let req_builder = if let Some(token) = auth_token {
        req_builder.header(http::header::AUTHORIZATION, format!("Bearer {}", token))
    } else {
        req_builder
    };
    let request = complete_uri(context, req_builder).await?;

    let r: Response<Body> = match inner_client.request(request).await {
        Err(e) => {
            if e.is_connect() {
                return Err(RequestFailType::ConnectError(e));
            } else {
                return Err(RequestFailType::HyperError(e));
            }
        }
        Ok(r) => {
            if r.status() == StatusCode::UNAUTHORIZED {
                if let Some(auth_header) = r
                    .headers()
                    .get("WWW-Authenticate")
                    .map(|e| e.to_str().ok())
                    .flatten()
                {
                    let b = BearerConfig::from_auth_header(auth_header).with_context(|| {
                        format!(
                            "unable to parse auth header when issuing request, got header {:?}",
                            auth_header
                        )
                    })?;
                    return Err(RequestFailType::AuthFailure(b));
                }
            }
            if r.status().is_redirection() {
                if let Some(location_header) = r.headers().get(http::header::LOCATION) {
                    let location_str = location_header.to_str().with_context(|| {
                        format!("Unable to parse redirection header {:?}", location_header)
                    })?;
                    return Err(RequestFailType::Redirection(location_str.to_string()));
                }
            }
            r
        }
    };
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_decode_auth_header() {
        let header = "Bearer realm=\"https://auth.docker.io/token\",service=\"registry.docker.io\"";

        let hdr = BearerConfig::from_auth_header(&header).expect("Should be able to decode header");
        assert_eq!(
            hdr.realm,
            "https://auth.docker.io/token".parse::<Uri>().unwrap()
        );
        assert_eq!(hdr.service, "registry.docker.io");
        assert_eq!(hdr.service, "registry.docker.io");
    }
}
