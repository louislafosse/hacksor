//! Local settings persistence, stored in `codex_home/hacksor.json`. API keys are
//! also mirrored to `codex_home/.env` so the Codex provider resolves them across
//! restarts before any command sets the process environment.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::models::Provider;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub openrouter_api_key: Option<String>,
    #[serde(default)]
    pub vercel_api_key: Option<String>,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub personality: Option<String>,
    /// Free-text user "custom instructions" injected into the developer prompt.
    #[serde(default)]
    pub custom_instructions: Option<String>,
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
    /// Run security tools inside a local Kali container via `docker exec`.
    #[serde(default)]
    pub kali_mode: bool,
    /// Where the agent runs: "host" (needs codex installed) or "docker" (bundled
    /// runtime image with codex + ocx + tools; user needs only Docker).
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// When to bring up the heavy background services (intercepting proxy +
    /// Camoufox stealth browser): "on_demand" (default — start them only when the
    /// agent actually uses them, saving RAM) or "always_on" (pre-start with the
    /// container so first use has no warm-up).
    #[serde(default = "default_services_mode")]
    pub services_mode: String,
    /// Optional Cloudflare global API key + email + account id, used ONLY by the
    /// bundled `cloudfish` recon tool (passive subdomain discovery via Cloudflare's
    /// DNS scanner). Mirrored to `~/.cloudflare` so the container's cloudfish reads
    /// them. A global key is high-privilege; leave empty unless you use cloudfish.
    #[serde(default)]
    pub cloudflare_api_key: Option<String>,
    #[serde(default)]
    pub cloudflare_email: Option<String>,
    #[serde(default)]
    pub cloudflare_account_id: Option<String>,
}

fn default_services_mode() -> String {
    "on_demand".to_string()
}

fn default_runtime() -> String {
    // Docker is the default: it bundles the whole environment (codex + ocx +
    // the full toolset), so a fresh machine needs only Docker.
    "docker".to_string()
}

fn default_provider() -> String {
    "openrouter".to_string()
}

fn default_working_dir() -> String {
    dirs::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string())
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            openrouter_api_key: None,
            vercel_api_key: None,
            provider: default_provider(),
            personality: None,
            custom_instructions: None,
            working_dir: default_working_dir(),
            kali_mode: false,
            runtime: default_runtime(),
            services_mode: default_services_mode(),
            cloudflare_api_key: None,
            cloudflare_email: None,
            cloudflare_account_id: None,
        }
    }
}

impl Settings {
    pub fn load(codex_home: &Path) -> Self {
        let path = codex_home.join("hacksor.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    pub fn key_for(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::OpenRouter => self.openrouter_api_key.as_deref(),
            Provider::Vercel => self.vercel_api_key.as_deref(),
            // OpenCodex is a local proxy; keys live in its own config.
            Provider::OpenCodex => None,
        }
        .filter(|k| !k.is_empty())
    }

    /// Push all known keys into the process environment under their provider
    /// env-var names so the Codex client can read them.
    pub fn export_env(&self) {
        for provider in Provider::ALL {
            if let Some(key) = self.key_for(provider) {
                std::env::set_var(provider.env_key(), key);
            }
        }
    }

    pub fn save(&self, codex_home: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(codex_home)?;
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(codex_home.join("hacksor.json"), text)?;

        let mut env = String::new();
        for provider in Provider::ALL {
            if let Some(key) = self.key_for(provider) {
                env.push_str(&format!("{}={}\n", provider.env_key(), key));
            }
        }
        let _ = std::fs::write(codex_home.join(".env"), env);
        self.save_cloudflare();
        Ok(())
    }

    /// Mirror the Cloudflare creds to `~/.cloudflare` (KEY=VALUE, 0600) so the
    /// container's bundled `cloudfish` picks them up ($HOME is bind-mounted into
    /// the runtime). Removes the file when all three are empty.
    fn save_cloudflare(&self) {
        let path = match dirs::home_dir() {
            Some(h) => h.join(".cloudflare"),
            None => return,
        };
        let clean = |o: &Option<String>| o.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let (k, e, a) = (clean(&self.cloudflare_api_key), clean(&self.cloudflare_email), clean(&self.cloudflare_account_id));
        if k.is_none() && e.is_none() && a.is_none() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let mut body = String::new();
        if let Some(v) = k { body.push_str(&format!("CLOUDFLARE_API_KEY={v}\n")); }
        if let Some(v) = e { body.push_str(&format!("CLOUDFLARE_EMAIL={v}\n")); }
        if let Some(v) = a { body.push_str(&format!("CLOUDFLARE_ACCOUNT_ID={v}\n")); }
        if std::fs::write(&path, body).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_docker_openrouter() {
        let s = Settings::default();
        assert_eq!(s.provider, "openrouter");
        assert_eq!(s.runtime, "docker");
        assert!(!s.kali_mode);
        assert!(s.openrouter_api_key.is_none());
    }

    #[test]
    fn missing_fields_deserialize_to_defaults() {
        // An old/minimal config with no runtime field must not fail to load.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.runtime, "docker");
        assert_eq!(s.provider, "openrouter");
    }

    #[test]
    fn key_for_filters_empty_and_local_proxy() {
        let mut s = Settings::default();
        s.openrouter_api_key = Some("".to_string());
        assert!(s.key_for(Provider::OpenRouter).is_none(), "empty key is treated as unset");
        s.openrouter_api_key = Some("sk-or-abc".to_string());
        assert_eq!(s.key_for(Provider::OpenRouter), Some("sk-or-abc"));
        // OpenCodex keys live in its own config, never in Hacksor settings.
        assert!(s.key_for(Provider::OpenCodex).is_none());
    }

    #[test]
    fn save_and_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("hacksor-test-{}", std::process::id()));
        let mut s = Settings::default();
        s.vercel_api_key = Some("vck_xyz".to_string());
        s.runtime = "docker".to_string();
        s.save(&dir).unwrap();
        let loaded = Settings::load(&dir);
        assert_eq!(loaded.runtime, "docker");
        assert_eq!(loaded.vercel_api_key.as_deref(), Some("vck_xyz"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
