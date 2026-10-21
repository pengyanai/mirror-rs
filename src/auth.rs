use moka::future::Cache;
use moka::Expiry;
use std::time::{Duration, Instant};

#[derive(Clone, Hash, PartialEq, Eq)]
pub struct TokenKey {
    mirror: String,
    repository: String,
}

impl TokenKey {
    pub fn new(mirror: &str, path: &str) -> Self {
        let path = path.split('?').next().unwrap_or(path);
        let repository = path
            .strip_prefix("/v2/")
            .and_then(|p| {
                p.rsplit_once("/manifests/")
                    .or_else(|| p.rsplit_once("/blobs/"))
                    .map(|(repo, _)| repo)
            })
            .unwrap_or(path);
        Self {
            mirror: mirror.trim_end_matches('/').to_owned(),
            repository: repository.to_owned(),
        }
    }
}

#[derive(Clone)]
struct Token {
    value: String,
    expires_at: Instant,
}

struct TokenExpiry;
impl Expiry<TokenKey, Token> for TokenExpiry {
    fn expire_after_create(&self, _: &TokenKey, token: &Token, now: Instant) -> Option<Duration> {
        Some(token.expires_at.saturating_duration_since(now))
    }
}

#[derive(Clone)]
pub struct TokenCache(Cache<TokenKey, Token>);

impl Default for TokenCache {
    fn default() -> Self {
        Self(
            Cache::builder()
                .max_capacity(4096)
                .expire_after(TokenExpiry)
                .build(),
        )
    }
}

impl TokenCache {
    pub async fn get(&self, key: &TokenKey) -> Option<String> {
        self.0.get(key).await.map(|token| token.value)
    }

    pub async fn invalidate(&self, key: &TokenKey, rejected: &str) {
        if self.get(key).await.as_deref() == Some(rejected) {
            self.0.invalidate(key).await;
        }
    }

    pub async fn fetch(
        &self,
        client: &reqwest::Client,
        key: TokenKey,
        challenge: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        self.0
            .try_get_with(key, fetch_token(client, challenge, timeout))
            .await
            .map(|token| token.value)
            .map_err(|err| (*err).clone())
    }
}

async fn fetch_token(
    client: &reqwest::Client,
    www_authenticate: &str,
    timeout: Duration,
) -> Result<Token, String> {
    let challenge = http_auth::ChallengeParser::new(www_authenticate)
        .filter_map(Result::ok)
        .find(|challenge| challenge.scheme.eq_ignore_ascii_case("bearer"))
        .ok_or("missing Bearer challenge")?;
    let param = |name: &str| {
        challenge
            .params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.to_unescaped())
    };
    let realm = param("realm").ok_or("missing token realm")?;
    let query: Vec<_> = ["service", "scope"]
        .into_iter()
        .filter_map(|name| param(name).map(|value| (name, value)))
        .collect();
    let started = Instant::now();
    let json: serde_json::Value = client
        .get(realm)
        .query(&query)
        .timeout(timeout)
        .send()
        .await
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json()
        .await
        .map_err(|err| err.to_string())?;
    let value = json
        .get("token")
        .or_else(|| json.get("access_token"))
        .and_then(|token| token.as_str())
        .filter(|token| !token.is_empty())
        .ok_or("missing token")?
        .to_owned();
    let lifetime = Duration::from_secs(
        json.get("expires_in")
            .and_then(|value| value.as_u64())
            .unwrap_or(60),
    );
    Ok(Token {
        value,
        expires_at: started.checked_add(lifetime).unwrap_or(started),
    })
}
