use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use url::Url;

#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

#[derive(Clone)]
pub struct CredentialPool {
    values: Arc<Vec<SecretString>>,
    next_index: Arc<AtomicUsize>,
}

impl fmt::Debug for CredentialPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialPool")
            .field("len", &self.values.len())
            .finish()
    }
}

impl CredentialPool {
    pub fn new(values: Vec<SecretString>) -> Result<Self> {
        if values.is_empty() || values.iter().any(|value| value.expose().trim().is_empty()) {
            bail!("credential pool must contain non-empty keys");
        }
        Ok(Self {
            values: Arc::new(values),
            next_index: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn next(&self) -> SecretString {
        let index = self.next_index.fetch_add(1, Ordering::Relaxed) % self.values.len();
        self.values[index].clone()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub api_base: Url,
    pub model: String,
    pub credentials: CredentialPool,
}

impl ProviderConfig {
    #[cfg(test)]
    fn for_test(api_base: &str, model: &str, credentials: Vec<SecretString>) -> Self {
        Self {
            api_base: normalize_api_base(api_base).unwrap(),
            model: model.to_string(),
            credentials: CredentialPool::new(credentials).unwrap(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProviderOverrides {
    pub api_base: Option<String>,
    pub model: Option<String>,
    pub credentials: Option<CredentialPool>,
}

pub fn normalize_api_base(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw).with_context(|| format!("invalid API base URL '{raw}'"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("API base URL must use http or https and include a host");
    }
    let normalized_path = url.path().trim_end_matches('/').to_string();
    url.set_path(if normalized_path.is_empty() {
        "/"
    } else {
        &normalized_path
    });
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

pub fn redact_provider_text(text: &str, credential: &str) -> String {
    if credential.is_empty() {
        text.to_string()
    } else {
        text.replace(credential, "[REDACTED]")
    }
}

pub fn resolve_review_provider(
    generation: &ProviderConfig,
    review: ProviderOverrides,
) -> Result<ProviderConfig> {
    let api_base = match review.api_base.as_deref() {
        Some(value) => normalize_api_base(value)?,
        None => generation.api_base.clone(),
    };
    let same_endpoint = api_base == generation.api_base;
    let credentials = match review.credentials {
        Some(pool) => pool,
        None if same_endpoint => generation.credentials.clone(),
        None => bail!(
            "review API endpoint differs from generation endpoint; explicit review credentials are required"
        ),
    };
    Ok(ProviderConfig {
        api_base,
        model: review.model.unwrap_or_else(|| generation.model.clone()),
        credentials,
    })
}

pub fn load_credential_pool(
    keyfile: Option<&Path>,
    key: Option<String>,
    label: &str,
) -> Result<CredentialPool> {
    if let Some(path) = keyfile {
        let file = File::open(path)
            .with_context(|| format!("failed to open {label} keyfile: {}", path.display()))?;
        let values = BufReader::new(file)
            .lines()
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .map(SecretString::new)
            .collect();
        return CredentialPool::new(values);
    }
    CredentialPool::new(vec![SecretString::new(
        key.with_context(|| format!("{label} API key is required"))?,
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_endpoint_inherits_generation_credentials_and_model() {
        let generation = ProviderConfig::for_test(
            "https://api.example/v1/",
            "generator",
            vec![SecretString::new("gen-key")],
        );
        let review = resolve_review_provider(&generation, ProviderOverrides::default()).unwrap();
        assert_eq!(review.api_base.as_str(), "https://api.example/v1");
        assert_eq!(review.model, "generator");
        assert_eq!(review.credentials.len(), 1);
    }

    #[test]
    fn different_endpoint_requires_explicit_review_credentials() {
        let generation = ProviderConfig::for_test(
            "https://generator.example/v1",
            "generator",
            vec![SecretString::new("gen-key")],
        );
        let error = resolve_review_provider(
            &generation,
            ProviderOverrides {
                api_base: Some("https://reviewer.example/v1".into()),
                model: Some("reviewer".into()),
                credentials: None,
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("explicit review credentials"),
            "{error:#}"
        );
    }

    #[test]
    fn different_endpoint_uses_explicit_review_credentials() {
        let generation = ProviderConfig::for_test(
            "https://generator.example/v1",
            "generator",
            vec![SecretString::new("gen-key")],
        );
        let review = resolve_review_provider(
            &generation,
            ProviderOverrides {
                api_base: Some("https://reviewer.example/v1/".into()),
                model: Some("reviewer".into()),
                credentials: Some(
                    CredentialPool::new(vec![SecretString::new("review-key")]).unwrap(),
                ),
            },
        )
        .unwrap();
        assert_eq!(review.api_base.as_str(), "https://reviewer.example/v1");
        assert_eq!(review.model, "reviewer");
        assert_eq!(review.credentials.next().expose(), "review-key");
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        let secret = SecretString::new("never-print-me");
        assert!(!format!("{secret:?}").contains("never-print-me"));
    }
}
