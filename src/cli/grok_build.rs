use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use codex_mixin::clients::grok_build::GrokBuildModel;
use codex_mixin::config::{GatewayConfig, stored_config_path};
use codex_mixin::gateway_access::GatewayClient;
use codex_mixin::provider::{ProviderModel, catalog_model_slug};

use super::official_models::selected_official_models;
use super::runtime::effective_gateway_bind;

const GROK_BUILD_PROVIDER_ID: &str = "codex-mixin-managed";
const GROK_BUILD_API_KEY_FILE: &str = "grok-build-api-key";

pub(in crate::cli) fn install_grok_build(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    codex_mixin::application::client::install_with_client_key(GatewayClient::GrokBuild, || {
        let gateway_config = GatewayConfig::from_stored_config()?;
        let official_models = selected_official_models(&gateway_config)?;
        let config_path = resolve_grok_build_config_path(config_path)?;
        let key_path = grok_build_key_path()?;
        let bind = effective_gateway_bind(&gateway_config)?;
        anyhow::ensure!(
            bind.ip().is_loopback(),
            "Grok Build integration requires a loopback gateway"
        );
        install_grok_build_with_models(
            &config_path,
            &key_path,
            bind,
            &gateway_config,
            &official_models,
            true,
        )
        .map(|_| ())
    })
}

pub(in crate::cli) fn uninstall_grok_build(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config_path = resolve_grok_build_config_path(config_path)?;
    let key_path = grok_build_key_path()?;
    codex_mixin::clients::grok_build::uninstall(&config_path, &key_path)?;
    println!("Grok Build config restored: {}", config_path.display());
    println!("Grok Build provider removed: {GROK_BUILD_PROVIDER_ID}");
    println!("reload required: restart Grok Build or start a new Grok Build session");
    Ok(())
}

fn default_grok_build_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("GROK_HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path).join("config.toml");
    }
    codex_mixin::platform::home_dir()
        .join(".grok")
        .join("config.toml")
}

fn resolve_grok_build_config_path(config_path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    std::path::absolute(config_path.unwrap_or_else(default_grok_build_config_path))
        .map_err(Into::into)
}

fn grok_build_key_path() -> anyhow::Result<PathBuf> {
    std::path::absolute(stored_config_path().with_file_name(GROK_BUILD_API_KEY_FILE))
        .map_err(Into::into)
}

fn install_grok_build_with_models(
    config_path: &Path,
    key_path: &Path,
    bind: SocketAddr,
    gateway_config: &GatewayConfig,
    official_models: &[ProviderModel],
    announce: bool,
) -> anyhow::Result<bool> {
    let models = collect_grok_build_models(gateway_config, official_models)?;
    anyhow::ensure!(
        !models.is_empty(),
        "no enabled upstream models are available; refresh or select models before installing to Grok Build"
    );
    let client_key = gateway_config.require_client_key(GatewayClient::GrokBuild)?;
    let changed = codex_mixin::clients::grok_build::install(
        config_path,
        key_path,
        bind,
        &models,
        &client_key,
    )?;
    if announce {
        println!("Grok Build config updated: {}", config_path.display());
        println!("Grok Build provider: {GROK_BUILD_PROVIDER_ID}");
        println!("Grok Build base URL: http://{bind}/v1");
        println!("models installed: {}", models.len());
        println!("reload required: restart Grok Build or start a new Grok Build session");
    }
    Ok(changed)
}

fn collect_grok_build_models(
    config: &GatewayConfig,
    official_models: &[ProviderModel],
) -> anyhow::Result<Vec<GrokBuildModel>> {
    let max_completion_tokens = u32::try_from(config.default_max_tokens)
        .map_err(|_| anyhow::anyhow!("default max tokens exceeds Grok Build's u32 range"))?;
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for provider in &config.providers {
        if !provider.enabled {
            continue;
        }
        for upstream_model_id in &provider.selected_models {
            let Some(cached) = provider
                .cached_models
                .iter()
                .find(|candidate| &candidate.id == upstream_model_id)
            else {
                continue;
            };
            let id = catalog_model_slug(upstream_model_id, &provider.id);
            if !seen.insert(id.clone()) {
                continue;
            }
            models.push(grok_build_model(
                id,
                format!("{upstream_model_id} · {}", provider.display_name),
                cached
                    .display_name
                    .as_deref()
                    .filter(|display| *display != upstream_model_id)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Codex Mixin model from {}", provider.display_name)),
                cached,
                config,
                max_completion_tokens,
            ));
        }
    }
    for model in official_models {
        if !seen.insert(model.id.clone()) {
            continue;
        }
        models.push(grok_build_model(
            model.id.clone(),
            format!("{} · OpenAI", model.id),
            model
                .display_name
                .as_deref()
                .filter(|display| *display != model.id)
                .map(str::to_owned)
                .unwrap_or_else(|| "Official OpenAI model through Codex Mixin".to_owned()),
            model,
            config,
            max_completion_tokens,
        ));
    }
    for profile in &config.fusion_profiles {
        let id = profile.model_slug();
        if !seen.insert(id.clone()) {
            continue;
        }
        models.push(GrokBuildModel {
            id,
            name: format!(
                "Fusion ({}): {} -> {}",
                profile.id,
                profile.panel_models.join("+"),
                profile.judge_model
            ),
            description: "Codex Mixin multi-model Fusion profile".to_owned(),
            context_window: config.default_context_window,
            max_completion_tokens,
            supports_reasoning: false,
        });
    }
    Ok(models)
}

fn grok_build_model(
    id: String,
    name: String,
    description: String,
    model: &ProviderModel,
    config: &GatewayConfig,
    max_completion_tokens: u32,
) -> GrokBuildModel {
    GrokBuildModel {
        id,
        name,
        description,
        context_window: model
            .context_window
            .unwrap_or(config.default_context_window),
        max_completion_tokens,
        supports_reasoning: model.supports_thinking != Some(false),
    }
}

pub(in crate::cli) fn sync_installed_grok_build_client_key() -> anyhow::Result<()> {
    let config_path = resolve_grok_build_config_path(None)?;
    let key_path = grok_build_key_path()?;
    codex_mixin::application::client::sync_managed_client_key(
        GatewayClient::GrokBuild,
        || codex_mixin::clients::grok_build::is_managed(&config_path, &key_path),
        |key| codex_mixin::clients::grok_build::sync_client_key(&key_path, key),
    )?;
    Ok(())
}

pub(in crate::cli) fn sync_installed_grok_build_models() -> anyhow::Result<bool> {
    let gateway_config = GatewayConfig::from_stored_config()?;
    let official_models = selected_official_models(&gateway_config)?;
    let config_path = resolve_grok_build_config_path(None)?;
    let key_path = grok_build_key_path()?;
    if !codex_mixin::clients::grok_build::is_managed(&config_path, &key_path)? {
        return Ok(false);
    }
    let bind = effective_gateway_bind(&gateway_config)?;
    anyhow::ensure!(
        bind.ip().is_loopback(),
        "Grok Build integration requires a loopback gateway"
    );
    install_grok_build_with_models(
        &config_path,
        &key_path,
        bind,
        &gateway_config,
        &official_models,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_mixin::config::ThinkingMode;
    use codex_mixin::provider::{ProviderModel, custom_provider};

    fn gateway_config() -> GatewayConfig {
        let mut provider = custom_provider("custom", "upstream-key");
        provider.display_name = "Custom Provider".to_owned();
        provider.selected_models = vec!["vision-model".to_owned()];
        provider.cached_models = vec![ProviderModel {
            id: "vision-model".to_owned(),
            display_name: Some("Vision Model".to_owned()),
            context_window: Some(128_000),
            supports_thinking: Some(true),
            ..ProviderModel::default()
        }];
        GatewayConfig {
            bind: "127.0.0.1:8787".parse().unwrap(),
            providers: vec![provider],
            official_responses_url: String::new(),
            codex_auth_path: PathBuf::from("/tmp/missing-auth"),
            gateway_api_key: None,
            gateway_client_keys: codex_mixin::gateway_access::GatewayClientKeys {
                grok_build: Some("grok-build-client-key".to_owned()),
                ..Default::default()
            },
            accept_codex_oauth: false,
            official_selected_models: None,
            default_max_tokens: 8192,
            default_context_window: 256_000,
            request_timeout: std::time::Duration::from_secs(30),
            thinking_mode: ThinkingMode::Auto,
            enable_web_search_tool: false,
            web_search_tool_type: "web_search".to_owned(),
            web_search_max_uses: None,
            fusion_profiles: Vec::new(),
        }
    }

    #[test]
    fn installs_selected_models_using_provider_qualified_gateway_ids() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let key_path = directory.path().join("key");
        let config = gateway_config();
        install_grok_build_with_models(&config_path, &key_path, config.bind, &config, &[], false)
            .unwrap();
        let document = std::fs::read_to_string(config_path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let model = &document["model"]["vision-model-custom"];
        assert_eq!(model["model"].as_str(), Some("vision-model-custom"));
        assert_eq!(
            model["name"].as_str(),
            Some("vision-model · Custom Provider")
        );
        assert_eq!(
            model["description"].as_str(),
            Some("[codex-mixin managed] Vision Model")
        );
        assert_eq!(model["context_window"].as_integer(), Some(128_000));
        assert_eq!(
            std::fs::read_to_string(key_path).unwrap(),
            "grok-build-client-key"
        );
    }
}
