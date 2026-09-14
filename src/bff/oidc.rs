//! OIDC protocol verification is delegated to openidconnect, not a custom JWT parser.
use super::*;
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, HttpRequest, HttpResponse, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata},
};
impl<'c> openidconnect::AsyncHttpClient<'c> for Bff {
    type Error = Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<HttpResponse>> + Send + 'c>>;
    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(self.oidc_http(request))
    }
}

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

#[derive(Serialize, Deserialize)]
pub(super) struct Pending {
    nonce: String,
    pkce: String,
    endpoints: Vec<String>,
}

impl Bff {
    // The only permitted destinations are deployment-configured IdP origins. No browser URL,
    // token jku/x5u or redirect can add a destination or forward the client secret elsewhere.
    async fn oidc_http(&self, request: HttpRequest) -> Result<HttpResponse> {
        let url = Url::parse(&request.uri().to_string()).map_err(|_| Error::BffUpstream)?;
        self.oidc_destination(&url)?;
        let response = self
            .http
            .execute(request.try_into().map_err(|_| Error::BffUpstream)?)
            .await
            .map_err(|_| Error::BffUpstream)?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = bounded_response(response, 262144).await?;
        let mut output = HttpResponse::new(bytes);
        *output.status_mut() = status;
        *output.headers_mut() = headers;
        Ok(output)
    }
    fn oidc_destination(&self, url: &Url) -> Result<()> {
        if !self
            .config
            .oidc_origins
            .contains(&url.origin().ascii_serialization())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::BffUpstream);
        }
        Ok(())
    }
    async fn oidc_client(&self) -> Result<(Client, Vec<String>)> {
        // Fresh discovery/JWKS on each login step: rotation does not depend on process restart.
        // Login concurrency, network timeout and byte limits bound this work.
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(self.config.oidc_issuer.clone()).map_err(|_| Error::Configuration)?,
            self,
        )
        .await
        .map_err(|_| Error::BffUpstream)?;
        let endpoints = vec![
            metadata.authorization_endpoint().url().to_string(),
            metadata
                .token_endpoint()
                .ok_or(Error::BffUpstream)?
                .url()
                .to_string(),
            metadata.jwks_uri().url().to_string(),
        ];
        for endpoint in &endpoints {
            self.oidc_destination(&Url::parse(endpoint).map_err(|_| Error::BffUpstream)?)?;
        }
        if !metadata
            .id_token_signing_alg_values_supported()
            .contains(&CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256)
        {
            return Err(Error::BffUpstream);
        }
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.config.client_id.clone()),
            Some(ClientSecret::new(self.config.client_secret.clone())),
        )
        .set_redirect_uri(
            RedirectUrl::new(format!("{}/bff/v1/callback", self.config.origin))
                .map_err(|_| Error::Configuration)?,
        );
        Ok((client, endpoints))
    }
    pub(super) async fn authorization(&self) -> Result<(String, String, Pending)> {
        let (client, endpoints) = self.oidc_client().await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .set_pkce_challenge(challenge)
            .set_max_age(Duration::ZERO)
            .url();
        Ok((
            url.to_string(),
            state.secret().clone(),
            Pending {
                nonce: nonce.secret().clone(),
                pkce: verifier.secret().clone(),
                endpoints,
            },
        ))
    }
    pub(super) async fn exchange(
        &self,
        code: String,
        pending: Pending,
    ) -> Result<(String, DateTime<Utc>, DateTime<Utc>)> {
        let (client, endpoints) = self.oidc_client().await?;
        if endpoints != pending.endpoints {
            return Err(Error::SessionUnauthorized);
        }
        let tokens = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|_| Error::BffUpstream)?
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce))
            .request_async(self)
            .await
            .map_err(|error| match error {
                openidconnect::RequestTokenError::ServerResponse(_) => Error::SessionUnauthorized,
                _ => Error::BffUpstream,
            })?;
        let token = tokens.id_token().ok_or(Error::SessionUnauthorized)?;
        let verifier = client
            .id_token_verifier()
            .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256]);
        let claims = token
            .claims(&verifier, &Nonce::new(pending.nonce))
            .map_err(|_| Error::SessionUnauthorized)?;
        let now = Utc::now();
        let auth_time = claims.auth_time().ok_or(Error::SessionUnauthorized)?;
        if auth_time > now + chrono::Duration::seconds(5)
            || auth_time < now - chrono::Duration::seconds(300)
            || claims.issue_time() > now + chrono::Duration::seconds(5)
            || claims.issue_time() < now - chrono::Duration::seconds(300)
            || (claims.audiences().len() > 1 && claims.authorized_party().is_none())
            || claims
                .authorized_party()
                .is_some_and(|v| v.as_str() != self.config.client_id)
            || !governance::valid_text(claims.subject().as_str(), 500)
        {
            return Err(Error::SessionUnauthorized);
        }
        if let Some(expected) = claims.access_token_hash() {
            let actual = AccessTokenHash::from_token(
                tokens.access_token(),
                token
                    .signing_alg()
                    .map_err(|_| Error::SessionUnauthorized)?,
                token
                    .signing_key(&verifier)
                    .map_err(|_| Error::SessionUnauthorized)?,
            )
            .map_err(|_| Error::SessionUnauthorized)?;
            if actual != *expected {
                return Err(Error::SessionUnauthorized);
            }
        }
        let expires_at = claims
            .expiration()
            .min(auth_time + chrono::Duration::hours(1));
        if expires_at <= now + chrono::Duration::seconds(30) {
            return Err(Error::SessionUnauthorized);
        }
        // Access/refresh/ID tokens are intentionally dropped; never persisted or sent to the browser.
        Ok((claims.subject().to_string(), auth_time, expires_at))
    }
}
