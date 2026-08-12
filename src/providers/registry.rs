//! Provider registry: validated, provider-agnostic, no discovery required.
//!
//! The registry is an in-memory map from provider id → adapter. It works with
//! manually configured model IDs; `/v1/models` is never called.

use std::collections::HashMap;
use std::sync::Arc;

use crate::providers::adapter::ProviderAdapter;
use crate::providers::anthropic::AnthropicAdapter;
use crate::providers::config::{ConfigError, ProviderConfig};
use crate::providers::error::ProviderError;

/// Concrete registry that owns adapters behind the typed port.
#[derive(Default)]
pub struct ProviderRegistry {
    adapters: HashMap<String, Arc<dyn ProviderAdapter>>,
    models: HashMap<String, Vec<String>>,
    default_models: HashMap<String, String>,
}

impl ProviderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry from a list of provider configs.
    ///
    /// Each config is validated; duplicate provider ids are rejected.
    /// The generic [`AnthropicAdapter`] is instantiated for every provider
    /// (provider quirks remain inside the adapter, not the registry).
    pub fn from_configs(configs: Vec<ProviderConfig>) -> Result<Self, ProviderError> {
        let mut reg = Self::new();
        for cfg in configs {
            reg.register_anthropic(cfg)?;
        }
        Ok(reg)
    }

    /// Register a single provider via the generic Anthropic adapter.
    pub fn register_anthropic(&mut self, config: ProviderConfig) -> Result<(), ProviderError> {
        config
            .validate()
            .map_err(|e| ProviderError::Config(e.to_string()))?;

        if self.adapters.contains_key(&config.id) {
            return Err(ProviderError::Config(
                ConfigError::DuplicateProviderId(config.id).to_string(),
            ));
        }

        let provider_id = config.id.clone();
        let model_ids = config
            .models
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        let default_model = config.default_model.clone();

        let adapter = AnthropicAdapter::from_config(&config)?;
        self.adapters.insert(provider_id.clone(), Arc::new(adapter));
        self.models.insert(provider_id.clone(), model_ids);
        if let Some(dm) = default_model {
            self.default_models.insert(provider_id, dm);
        }
        Ok(())
    }

    /// Register an arbitrary adapter (useful for tests / custom transports).
    pub fn register_adapter(
        &mut self,
        adapter: Arc<dyn ProviderAdapter>,
        model_ids: Vec<String>,
        default_model: Option<String>,
    ) -> Result<(), ProviderError> {
        let pid = adapter.provider_id().to_string();
        if self.adapters.contains_key(&pid) {
            return Err(ProviderError::Config(format!(
                "duplicate provider id: {pid}"
            )));
        }
        self.adapters.insert(pid.clone(), adapter);
        self.models.insert(pid.clone(), model_ids);
        if let Some(dm) = default_model {
            self.default_models.insert(pid, dm);
        }
        Ok(())
    }

    /// List registered provider ids.
    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.adapters.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Check if a provider exists.
    pub fn has_provider(&self, provider_id: &str) -> bool {
        self.adapters.contains_key(provider_id)
    }

    /// List model ids for a provider (empty if none configured; never requires discovery).
    pub fn model_ids(&self, provider_id: &str) -> Option<&[String]> {
        self.models.get(provider_id).map(|v| v.as_slice())
    }

    /// Resolve a provider + model pair. If `model_id` is `None`, the provider's
    /// default model is used if configured.
    pub fn resolve<'a>(
        &'a self,
        provider_id: &'a str,
        model_id: Option<&str>,
    ) -> Result<ResolvedHandle<'a>, ProviderError> {
        let adapter = self
            .adapters
            .get(provider_id)
            .ok_or_else(|| ProviderError::ProviderNotFound(provider_id.to_string()))?;

        let requested_model = match model_id {
            Some(m) => m,
            None => self
                .default_models
                .get(provider_id)
                .map(String::as_str)
                .ok_or_else(|| ProviderError::ModelNotFound {
                    provider: provider_id.to_string(),
                    model: "(no model specified and no default)".to_string(),
                })?,
        };

        let effective_model = model_without_context_suffix(requested_model).to_string();

        // Validate that the model is known if the provider has a non-empty model list.
        // If the list is empty, any model id is accepted (manual configuration without discovery).
        if let Some(known) = self.models.get(provider_id)
            && !known.is_empty()
            && !known.iter().any(|configured| {
                configured == requested_model
                    || model_without_context_suffix(configured) == effective_model
            })
        {
            return Err(ProviderError::ModelNotFound {
                provider: provider_id.to_string(),
                model: requested_model.to_string(),
            });
        }

        Ok(ResolvedHandle {
            provider_id,
            model_id: effective_model,
            adapter: adapter.as_ref(),
        })
    }

    /// Get the adapter for a provider (typed boundary).
    pub fn adapter(&self, provider_id: &str) -> Option<&dyn ProviderAdapter> {
        self.adapters
            .get(provider_id)
            .map(|a| a.as_ref() as &dyn ProviderAdapter)
    }

    /// Number of registered providers.
    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }
}

/// Claude Code uses a trailing `[1m]` suffix to select a larger context
/// window, then removes it before sending the provider request. Treat it as
/// routing metadata so either form resolves to the same upstream model.
fn model_without_context_suffix(model: &str) -> &str {
    if model.len() >= 4 && model[model.len() - 4..].eq_ignore_ascii_case("[1m]") {
        &model[..model.len() - 4]
    } else {
        model
    }
}

/// Borrowed handle for a resolved provider+model.
pub struct ResolvedHandle<'a> {
    pub provider_id: &'a str,
    pub model_id: String,
    pub adapter: &'a dyn ProviderAdapter,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::config::{AuthConfig, ModelCapabilities, ModelConfig};
    use url::Url;

    fn cfg(id: &str, models: Vec<&str>) -> ProviderConfig {
        ProviderConfig {
            id: id.to_string(),
            base_url: Url::parse("https://example.test").expect("url"),
            auth: AuthConfig::None,
            models: models
                .into_iter()
                .map(|m| ModelConfig {
                    id: m.to_string(),
                    display_name: None,
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: ModelCapabilities::default(),
                })
                .collect(),
            connect_timeout_ms: None,
            read_timeout_ms: Some(2000),
            default_model: None,
            extra_headers: vec![],
            retry: None,
        }
    }

    #[test]
    fn registry_works_without_discovery() {
        let mut reg = ProviderRegistry::new();
        reg.register_anthropic(cfg("prov-a", vec!["model-1", "model-2"]))
            .expect("register");
        assert!(reg.has_provider("prov-a"));
        let h = reg.resolve("prov-a", Some("model-1")).expect("resolve");
        assert_eq!(h.model_id, "model-1");
        // Unknown model when list non-empty should fail.
        assert!(reg.resolve("prov-a", Some("unknown")).is_err());
    }

    #[test]
    fn empty_model_list_accepts_any_id() {
        let mut reg = ProviderRegistry::new();
        reg.register_anthropic(cfg("prov-b", vec![]))
            .expect("register");
        // No discovery needed; any model id is accepted.
        let h = reg
            .resolve("prov-b", Some("hand-configured-model"))
            .expect("resolve any");
        assert_eq!(h.model_id, "hand-configured-model");
    }

    #[test]
    fn duplicate_provider_rejected() {
        let mut reg = ProviderRegistry::new();
        reg.register_anthropic(cfg("dup", vec![])).expect("first");
        let err = reg
            .register_anthropic(cfg("dup", vec![]))
            .expect_err("duplicate");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn default_model_fallback() {
        let mut c = cfg("prov-c", vec!["m1", "m2"]);
        c.default_model = Some("m1".to_string());
        let mut reg = ProviderRegistry::new();
        reg.register_anthropic(c).expect("register");
        let h = reg.resolve("prov-c", None).expect("default");
        assert_eq!(h.model_id, "m1");
    }

    #[test]
    fn one_million_context_suffix_resolves_as_an_alias() {
        let mut reg = ProviderRegistry::new();
        reg.register_anthropic(cfg("prov-d", vec!["model[1m]"]))
            .expect("register");

        let base = reg.resolve("prov-d", Some("model")).expect("base alias");
        assert_eq!(base.model_id, "model");

        let suffixed = reg
            .resolve("prov-d", Some("model[1m]"))
            .expect("suffixed model");
        assert_eq!(suffixed.model_id, "model");
    }
}
