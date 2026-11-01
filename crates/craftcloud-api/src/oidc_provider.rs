use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    time::Duration,
};

use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IdTokenClaims, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    core::{
        CoreAuthenticationFlow, CoreClient, CoreIdTokenVerifier, CoreJwsSigningAlgorithm,
        CoreProviderMetadata,
    },
    http::{Request as HttpRequest, Response as HttpResponse},
};
use tokio::sync::Semaphore;
use url::Url;
use zeroize::Zeroize;

const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DISCOVERY_SUFFIX: &str = "/.well-known/openid-configuration";
static REQUEST_LIMIT: Semaphore = Semaphore::const_new(8);

pub(crate) struct ProviderSettings {
    pub discovery_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

impl Drop for ProviderSettings {
    fn drop(&mut self) {
        self.client_secret.zeroize();
    }
}

pub(crate) struct Authorization {
    pub authorization_url: String,
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub issuer: String,
}

pub(crate) struct Identity {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub display_name: Option<String>,
    pub mfa: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderError {
    InvalidConfiguration,
    UnsafeDestination,
    NetworkFailure,
    InvalidIdentityToken,
}

pub(crate) async fn authorize(settings: &ProviderSettings) -> Result<Authorization, ProviderError> {
    validate_client(settings)?;
    let (issuer, discovery) = issuer_from_discovery(&settings.discovery_url)?;
    let redirect = checked_redirect(&settings.redirect_uri)?;
    let metadata = discover(&issuer, &discovery).await?;
    verify_browser_destination(metadata.authorization_endpoint().as_str()).await?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(settings.client_id.clone()),
        Some(ClientSecret::new(settings.client_secret.clone())),
    )
    .set_redirect_uri(redirect);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("openid".to_string()))
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .set_pkce_challenge(challenge)
        .url();
    Ok(Authorization {
        authorization_url: url.to_string(),
        state: state.secret().to_owned(),
        nonce: nonce.secret().to_owned(),
        pkce_verifier: verifier.secret().to_owned(),
        issuer: issuer.to_string(),
    })
}

pub(crate) async fn exchange(
    settings: &ProviderSettings,
    expected_issuer: &str,
    code: &str,
    nonce: &str,
    pkce_verifier: &str,
) -> Result<Identity, ProviderError> {
    validate_client(settings)?;
    if code.is_empty()
        || code.len() > 4096
        || nonce.is_empty()
        || nonce.len() > 1024
        || pkce_verifier.len() < 43
        || pkce_verifier.len() > 128
    {
        return Err(ProviderError::InvalidConfiguration);
    }
    let (issuer, discovery) = issuer_from_discovery(&settings.discovery_url)?;
    if issuer.as_str() != expected_issuer {
        return Err(ProviderError::InvalidIdentityToken);
    }
    let redirect = checked_redirect(&settings.redirect_uri)?;
    let metadata = discover(&issuer, &discovery).await?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(settings.client_id.clone()),
        Some(ClientSecret::new(settings.client_secret.clone())),
    )
    .set_redirect_uri(redirect);
    let response = client
        .exchange_code(AuthorizationCode::new(code.to_owned()))
        .map_err(|_| ProviderError::InvalidConfiguration)?
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_owned()))
        .request_async(&PinnedHttpClient)
        .await
        .map_err(|_| ProviderError::NetworkFailure)?;
    let token = response
        .extra_fields()
        .id_token()
        .ok_or(ProviderError::InvalidIdentityToken)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ProviderError::InvalidIdentityToken)?
        .as_secs() as i64;
    let verifier: CoreIdTokenVerifier = client
        .id_token_verifier()
        .set_allowed_algs([
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            CoreJwsSigningAlgorithm::EcdsaP256Sha256,
        ])
        .set_issue_time_verifier_fn(move |issued| {
            let timestamp = issued.timestamp();
            if timestamp <= now.saturating_add(60) && timestamp >= now.saturating_sub(600) {
                Ok(())
            } else {
                Err("invalid issue time".to_owned())
            }
        });
    let claims = token
        .claims(&verifier, &Nonce::new(nonce.to_owned()))
        .map_err(|_| ProviderError::InvalidIdentityToken)?;
    Ok(identity_from_claims(claims))
}

fn validate_client(settings: &ProviderSettings) -> Result<(), ProviderError> {
    if settings.client_id.is_empty()
        || settings.client_id.len() > 1024
        || settings.client_secret.is_empty()
        || settings.client_secret.len() > 4096
    {
        return Err(ProviderError::InvalidConfiguration);
    }
    Ok(())
}

fn identity_from_claims<AC, GC>(claims: &IdTokenClaims<AC, GC>) -> Identity
where
    AC: openidconnect::AdditionalClaims,
    GC: openidconnect::GenderClaim,
{
    let standard = claims;
    Identity {
        issuer: standard.issuer().to_string(),
        subject: standard.subject().to_string(),
        email: standard.email().map(|v| v.as_str().to_owned()),
        email_verified: standard.email_verified().unwrap_or(false),
        display_name: standard
            .name()
            .and_then(|name| name.get(None))
            .map(|v| v.as_str().to_owned()),
        mfa: standard
            .auth_method_refs()
            .is_some_and(|amr| amr.iter().any(|v| v.as_str() == "mfa")),
    }
}

async fn discover(
    issuer: &IssuerUrl,
    discovery: &Url,
) -> Result<CoreProviderMetadata, ProviderError> {
    if issuer
        .join(".well-known/openid-configuration")
        .map_err(|_| ProviderError::InvalidConfiguration)?
        != *discovery
    {
        return Err(ProviderError::InvalidConfiguration);
    }
    CoreProviderMetadata::discover_async(issuer.clone(), &PinnedHttpClient)
        .await
        .map_err(|_| ProviderError::NetworkFailure)
}

fn issuer_from_discovery(value: &str) -> Result<(IssuerUrl, Url), ProviderError> {
    let discovery = checked_url(value)?;
    if !discovery.path().ends_with(DISCOVERY_SUFFIX) || discovery.query().is_some() {
        return Err(ProviderError::InvalidConfiguration);
    }
    let issuer_text = value
        .strip_suffix(DISCOVERY_SUFFIX)
        .ok_or(ProviderError::InvalidConfiguration)?;
    let issuer =
        IssuerUrl::new(issuer_text.to_owned()).map_err(|_| ProviderError::InvalidConfiguration)?;
    Ok((issuer, discovery))
}

fn checked_redirect(value: &str) -> Result<RedirectUrl, ProviderError> {
    let parsed = Url::parse(value).map_err(|_| ProviderError::InvalidConfiguration)?;
    let local = parsed.scheme() == "http"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.fragment().is_none()
        && match parsed.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(name)) => name == "localhost",
            _ => false,
        };
    if !local {
        checked_url(value).map_err(|_| ProviderError::InvalidConfiguration)?;
    }
    RedirectUrl::new(value.to_owned()).map_err(|_| ProviderError::InvalidConfiguration)
}

fn checked_url(value: &str) -> Result<Url, ProviderError> {
    let url = Url::parse(value).map_err(|_| ProviderError::UnsafeDestination)?;
    if url.username() != ""
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
    {
        return Err(ProviderError::UnsafeDestination);
    }
    if url.scheme() == "https" {
        if let Some(ip) = match url.host() {
            Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
            _ => None,
        } && !public_ip(ip)
        {
            return Err(ProviderError::UnsafeDestination);
        }
        return Ok(url);
    }
    #[cfg(any(test, feature = "test-util"))]
    if url.scheme() == "http"
        && std::env::var("CRAFTCLOUD_TEST_OIDC_LOOPBACK")
            .ok()
            .as_deref()
            == Some("1")
        && url.host().is_some_and(|host| match host {
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => ip.is_loopback(),
            _ => false,
        })
    {
        return Ok(url);
    }
    Err(ProviderError::UnsafeDestination)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let blocked = [
                "0.0.0.0/8",
                "10.0.0.0/8",
                "100.64.0.0/10",
                "127.0.0.0/8",
                "169.254.0.0/16",
                "172.16.0.0/12",
                "192.0.0.0/24",
                "192.0.2.0/24",
                "192.88.99.0/24",
                "192.168.0.0/16",
                "198.18.0.0/15",
                "198.51.100.0/24",
                "203.0.113.0/24",
                "224.0.0.0/4",
                "240.0.0.0/4",
            ];
            !blocked.iter().any(|n| {
                n.parse::<ipnet::Ipv4Net>()
                    .is_ok_and(|net| net.contains(&ip))
            })
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            if !"2000::/3"
                .parse::<ipnet::Ipv6Net>()
                .is_ok_and(|net| net.contains(&ip))
            {
                return false;
            }
            let blocked = [
                "::/128",
                "::1/128",
                "::ffff:0:0/96",
                "64:ff9b::/96",
                "100::/64",
                "2001:db8::/32",
                "2001::/23",
                "2002::/16",
                "fc00::/7",
                "fe80::/10",
                "ff00::/8",
            ];
            !blocked.iter().any(|n| {
                n.parse::<ipnet::Ipv6Net>()
                    .is_ok_and(|net| net.contains(&ip))
            })
        }
    }
}

struct PinnedHttpClient;
impl<'a> openidconnect::AsyncHttpClient<'a> for PinnedHttpClient {
    type Error = io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse<Vec<u8>>, Self::Error>> + Send + 'a>>;
    fn call(&'a self, request: HttpRequest<Vec<u8>>) -> Self::Future {
        Box::pin(async move { send_pinned(request).await })
    }
}

async fn send_pinned(request: HttpRequest<Vec<u8>>) -> Result<HttpResponse<Vec<u8>>, io::Error> {
    tokio::time::timeout(REQUEST_TIMEOUT, send_pinned_inner(request))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "OIDC request timeout"))?
}

async fn send_pinned_inner(
    request: HttpRequest<Vec<u8>>,
) -> Result<HttpResponse<Vec<u8>>, io::Error> {
    let _permit = tokio::time::timeout(REQUEST_TIMEOUT, REQUEST_LIMIT.acquire())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "OIDC request limit timeout"))?
        .map_err(|_| io::Error::other("OIDC request limit closed"))?;
    let url = Url::parse(&request.uri().to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid URL"))?;
    let url = checked_url(url.as_str())
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "unsafe URL"))?;
    let host = url
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing port"))?;
    let addresses = tokio::time::timeout(REQUEST_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "DNS timeout"))??
        .collect::<Vec<_>>();
    let addresses = if test_loopback_url(&url) {
        if addresses.is_empty() || addresses.iter().any(|a| !a.ip().is_loopback()) {
            return Err(io::Error::other("invalid test resolution"));
        }
        addresses
    } else {
        validate_resolution(addresses)?
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| io::Error::other("HTTP client error"))?;
    let (parts, body) = request.into_parts();
    let mut outgoing = client.request(parts.method, url).body(body);
    for (name, value) in &parts.headers {
        outgoing = outgoing.header(name, value);
    }
    let mut response = tokio::time::timeout(REQUEST_TIMEOUT, outgoing.send())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "HTTP timeout"))?
        .map_err(|_| io::Error::other("HTTP request failed"))?;
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response too large",
        ));
    }
    let status = response.status();
    let headers = response.headers().clone();
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::time::timeout(REQUEST_TIMEOUT, response.chunk())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "response timeout"))?
            .map_err(|_| io::Error::other("response read failed"))?;
        let Some(chunk) = chunk else {
            break;
        };
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|size| size > MAX_RESPONSE_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response too large",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut result = HttpResponse::builder().status(status);
    for (name, value) in headers.iter() {
        result = result.header(name, value);
    }
    result
        .body(bytes)
        .map_err(|_| io::Error::other("invalid HTTP response"))
}

fn validate_resolution(addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, io::Error> {
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "non-public DNS result",
        ));
    }
    Ok(addresses)
}

/// Validates discovery URL syntax before it can be persisted.
pub(crate) fn valid_discovery_url(value: &str) -> bool {
    issuer_from_discovery(value).is_ok()
}

fn test_loopback_url(url: &Url) -> bool {
    #[cfg(any(test, feature = "test-util"))]
    {
        url.scheme() == "http"
            && std::env::var("CRAFTCLOUD_TEST_OIDC_LOOPBACK")
                .ok()
                .as_deref()
                == Some("1")
            && match url.host() {
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                _ => false,
            }
    }
    #[cfg(not(any(test, feature = "test-util")))]
    {
        let _ = url;
        false
    }
}

async fn verify_browser_destination(value: &str) -> Result<(), ProviderError> {
    let url = checked_url(value)?;
    if test_loopback_url(&url) {
        return Ok(());
    }
    let host = url.host_str().ok_or(ProviderError::UnsafeDestination)?;
    let port = url
        .port_or_known_default()
        .ok_or(ProviderError::UnsafeDestination)?;
    let addresses = tokio::time::timeout(REQUEST_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| ProviderError::NetworkFailure)?
        .map_err(|_| ProviderError::NetworkFailure)?
        .collect();
    validate_resolution(addresses).map_err(|_| ProviderError::UnsafeDestination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_callback_preserves_local_development_exception() {
        assert!(checked_redirect("http://127.0.0.1:31000/api/v1/auth/oidc/callback").is_ok());
        assert!(checked_redirect("http://localhost:31000/api/v1/auth/oidc/callback").is_ok());
        assert!(checked_redirect("http://192.168.1.1/api/v1/auth/oidc/callback").is_err());
    }

    #[test]
    fn discovery_requires_exact_well_known_https_url() {
        assert!(
            issuer_from_discovery("https://id.example/.well-known/openid-configuration").is_ok()
        );
        for url in [
            "http://id.example/.well-known/openid-configuration",
            "https://id.example/.well-known/openid-configuration?x=1",
            "https://u:p@id.example/.well-known/openid-configuration",
            "https://127.0.0.1/.well-known/openid-configuration",
        ] {
            assert!(issuer_from_discovery(url).is_err());
        }
    }

    #[test]
    fn blocks_special_and_private_addresses_including_mapped_ipv4() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.1.1",
            "192.168.1.1",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(ip.parse().expect("valid test IP")), "{ip}");
        }
        assert!(public_ip("8.8.8.8".parse().expect("valid test IP")));
        assert!(public_ip(
            "2606:4700:4700::1111".parse().expect("valid test IP")
        ));
    }

    #[test]
    fn rejects_mixed_dns_answers_to_prevent_rebinding_to_private_hosts() {
        let answers = vec![
            "8.8.8.8:443".parse().expect("valid socket address"),
            "127.0.0.1:443".parse().expect("valid socket address"),
        ];
        assert!(validate_resolution(answers).is_err());
    }

    #[test]
    fn rejects_credentials_fragment_and_plain_http_by_default() {
        assert!(checked_url("https://user@id.example/path").is_err());
        assert!(checked_url("https://id.example/path#fragment").is_err());
        assert!(checked_url("http://id.example/path").is_err());
    }
}
