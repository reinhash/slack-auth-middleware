use http_body_util::BodyExt;
use std::{
    convert::Infallible,
    task::{Context, Poll},
};

use axum::{body::Body, extract::Request, response::Response};
use futures_util::future::BoxFuture;
use hex;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct SlackAuthConfig {
    pub version_number: String,
    pub slack_signing_secret: String,
}

#[derive(Clone)]
pub struct SlackAuthLayer {
    config: SlackAuthConfig,
}

impl SlackAuthLayer {
    #[must_use]
    pub const fn new(config: SlackAuthConfig) -> Self {
        Self { config }
    }
}

impl<S> Layer<S> for SlackAuthLayer {
    type Service = SlackAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            inner,
            config: self.config.clone(),
        }
    }
}
// TODO: remove unwraps
// TODO: write tests
// TODO: write documentation

#[derive(Clone)]
pub struct SlackAuthService<S> {
    inner: S,
    config: SlackAuthConfig,
}

impl<S> Service<Request<Body>> for SlackAuthService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let clone = self.config.clone();
        let config = std::mem::replace(&mut self.config, clone);
        Box::pin(async move {
            let deny = || {
                let response = Response::builder().status(401).body(Body::empty()).unwrap();
                Ok(response)
            };

            let (parts, body) = req.into_parts();
            let bytes = match body.collect().await {
                Ok(bytes) => bytes.to_bytes(),
                Err(_) => return deny(),
            };
            let request_body = std::str::from_utf8(&bytes).unwrap();
            let slack_signature = match parts.headers.get("x-slack-signature") {
                Some(signature) => match signature.to_str() {
                    Ok(signature) => signature,
                    Err(_) => return deny(),
                },
                None => return deny(),
            };
            let Some(slack_request_timestamp) = parts.headers.get("x-slack-request-timestamp")
            else {
                return deny();
            };
            let slack_request_timestamp = slack_request_timestamp
                .to_str()
                .unwrap_or("")
                .parse::<i64>()
                .unwrap_or(0);
            let Some(parsed_slack_request_timestamp) =
                chrono::DateTime::from_timestamp(slack_request_timestamp, 0)
            else {
                return deny();
            };
            if chrono::offset::Utc::now()
                .signed_duration_since(parsed_slack_request_timestamp)
                .num_seconds()
                > 60 * 5
            {
                return deny();
            }
            let signer =
                SecretSigner::new(config, request_body.to_string(), slack_request_timestamp);
            let generated_hash = signer.sign();
            if generated_hash != slack_signature {
                return deny();
            }
            let req = Request::from_parts(parts, Body::from(bytes));
            inner.call(req).await
        })
    }
}

struct SecretSigner {
    config: SlackAuthConfig,
    request_body: String,
    timestamp: i64,
}

impl SecretSigner {
    #[must_use]
    pub const fn new(config: SlackAuthConfig, request_body: String, timestamp: i64) -> Self {
        Self {
            config,
            request_body,
            timestamp,
        }
    }

    #[must_use]
    fn sign(&self) -> String {
        let base_string = format!(
            "{version_number}:{timestamp}:{request_body}",
            version_number = self.config.version_number,
            timestamp = self.timestamp,
            request_body = self.request_body
        );
        let hash = self.hmac_signature(&base_string);
        format!(
            "{version_number}={hash}",
            version_number = self.config.version_number,
            hash = hash
        )
    }

    fn hmac_signature(&self, msg: &str) -> String {
        type HmacSha256 = Hmac<Sha256>;

        let mut mac =
            HmacSha256::new_from_slice(self.config.slack_signing_secret.as_bytes()).unwrap();
        mac.update(msg.as_bytes());
        let code_bytes = mac.finalize().into_bytes();
        hex::encode(code_bytes)
    }
}
