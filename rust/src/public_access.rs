use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub gzip_enabled: bool,
    pub gzip_min_bytes: u64,
    pub gzip_max_bytes: u64,
    pub qps: u32,
    pub hotlink_enabled: bool,
    pub allow_empty_referer: bool,
    pub allowed_referers: Vec<String>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            gzip_enabled: true,
            gzip_min_bytes: 256,
            gzip_max_bytes: 2_097_152,
            qps: 100,
            hotlink_enabled: false,
            allow_empty_referer: true,
            allowed_referers: vec![],
        }
    }
}
impl Policy {
    pub fn from_config(c: &Value) -> Result<Self> {
        let p: Self = match c.get("public_access") {
            None => Self::default(),
            Some(v) => serde_json::from_value(v.clone()).context("invalid public access policy")?,
        };
        p.validate()?;
        Ok(p)
    }
    pub fn load(path: &Path) -> Result<Self> {
        match crate::common::read_json(path, 65536) {
            Ok(v) => {
                let p: Self = serde_json::from_value(v)?;
                p.validate()?;
                Ok(p)
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(Self::default())
            }
            Err(e) => Err(e),
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!((1..=100_000).contains(&self.qps), "public QPS out of range");
        ensure!(
            self.gzip_min_bytes >= 256
                && self.gzip_min_bytes <= self.gzip_max_bytes
                && self.gzip_max_bytes <= 2_097_152,
            "invalid gzip size bounds"
        );
        ensure!(
            self.allowed_referers.len() <= 256,
            "too many allowed referers"
        );
        for entry in &self.allowed_referers {
            let domain = entry.strip_prefix("*.").unwrap_or(entry);
            ensure!(
                domain.len() <= 253 && domain.is_ascii() && domain == domain.to_ascii_lowercase(),
                "invalid referer domain"
            );
            ensure!(
                domain.split('.').all(|label| !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')),
                "invalid referer domain"
            );
            ensure!(
                !entry.starts_with("*.") || domain.contains('.'),
                "invalid referer wildcard"
            );
        }
        Ok(())
    }
    pub fn referer_allowed(&self, referer: Option<&str>) -> bool {
        if !self.hotlink_enabled {
            return true;
        }
        let Some(raw) = referer.filter(|s| !s.is_empty()) else {
            return self.allow_empty_referer;
        };
        if raw.bytes().any(|b| b <= 32 || b == 127 || b == b'\\') {
            return false;
        }
        let Some((scheme, rest)) = raw.split_once("://") else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return false;
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.is_empty() || authority.contains(['@', '%']) {
            return false;
        }
        let Ok(url) = url::Url::parse(raw) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        self.allowed_referers
            .iter()
            .any(|allowed| match allowed.strip_prefix("*.") {
                Some(domain) => {
                    host.len() > domain.len() + 1 && host.ends_with(&format!(".{domain}"))
                }
                None => host == allowed,
            })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn strict_configuration_and_optional_file() {
        assert_eq!(
            Policy::load(&tempfile::tempdir().unwrap().path().join("absent")).unwrap(),
            Policy::default()
        );
        for p in [
            json!({"qps":0}),
            json!({"gzip_min_bytes":255}),
            json!({"gzip_max_bytes":2097153}),
            json!({"gzip_min_bytes":999,"gzip_max_bytes":1}),
            json!({"unknown":true}),
            json!({"allowed_referers":["example.com;return 200"]}),
            json!({"allowed_referers":["https://example.com"]}),
            json!({"allowed_referers":["x.*.com"]}),
        ] {
            assert!(Policy::from_config(&json!({"public_access":p})).is_err());
        }
    }
    #[test]
    fn referer_matches_exact_or_subdomain_without_spoofing() {
        let p=Policy::from_config(&json!({"public_access":{"hotlink_enabled":true,"allowed_referers":["example.com","*.images.example.com","localhost"]}})).unwrap();
        for s in [
            None,
            Some(""),
            Some("https://example.com/a"),
            Some("https://example.com?query=1"),
            Some("https://example.com#fragment"),
            Some("http://a.images.example.com:8080/a"),
            Some("http://localhost/"),
        ] {
            assert!(p.referer_allowed(s), "{s:?}");
        }
        for s in [
            "https://images.example.com/",
            "https://example.com.evil/",
            "https://example.com@evil/",
            "https://user@example.com/",
            "https://example.com\\@evil/",
            "https://example.com /",
            "https://example.com/a b",
            "https://example.com:65536/",
            "https://example.com?query=1\\evil",
            "javascript:example.com",
            "http:example.com",
            "https://@example.com/",
            "https://%65xample.com/",
            "https:///example.com/",
        ] {
            assert!(!p.referer_allowed(Some(s)), "{s}");
        }
    }
}
