use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::Context;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use super::files::{set_owner_only, write_atomic_if_changed, write_owner_only};

const PROVIDER_ID: &str = "codex-mixin-managed";
const API_BACKEND: &str = "responses";
const REASONING_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const MANAGED_DESCRIPTION_PREFIX: &str = "[codex-mixin managed] ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrokBuildModel {
    pub id: String,
    pub name: String,
    pub description: String,
    pub context_window: u64,
    pub max_completion_tokens: u32,
    pub supports_reasoning: bool,
}

pub fn install(
    config_path: &Path,
    key_path: &Path,
    bind: SocketAddr,
    models: &[GrokBuildModel],
    client_key: &str,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        !models.is_empty(),
        "Grok Build model list must not be empty"
    );
    anyhow::ensure!(
        !client_key.trim().is_empty() && client_key == client_key.trim(),
        "Grok Build client key must be non-empty and contain no surrounding whitespace"
    );
    anyhow::ensure!(
        bind.ip().is_loopback(),
        "Grok Build integration requires a loopback gateway"
    );
    let mut document = read_config(config_path)?;
    validate_provider_slot(&document, key_path)?;
    validate_model_slots(&document, models)?;
    let config_existed = config_path.exists();
    let existing_managed_models = managed_model_ids(&document)?;
    let previous_key = key_path
        .exists()
        .then(|| std::fs::read(key_path))
        .transpose()?;
    let key_changed = previous_key.as_deref() != Some(client_key.as_bytes());

    remove_managed_models(&mut document, &existing_managed_models)?;
    upsert_provider(&mut document, key_path, bind)?;
    upsert_models(&mut document, models)?;

    write_owner_only(key_path, client_key.as_bytes())?;
    let changed = match write_config(config_path, &document) {
        Ok(changed) => changed,
        Err(config_error) => {
            let key_rollback = match previous_key {
                Some(previous) => write_owner_only(key_path, &previous),
                None if key_path.exists() => std::fs::remove_file(key_path)
                    .with_context(|| format!("remove new Grok Build key {}", key_path.display())),
                None => Ok(()),
            };
            return match key_rollback {
                Ok(()) => Err(config_error),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "{config_error:#}; Grok Build key rollback also failed: {rollback_error:#}"
                )),
            };
        }
    };
    if !config_existed {
        set_owner_only(config_path)?;
    }
    Ok(changed || key_changed)
}

pub fn is_managed(config_path: &Path, key_path: &Path) -> anyhow::Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    let document = read_config(config_path)?;
    Ok(managed_provider(&document, key_path).is_some())
}

pub fn sync_client_key(key_path: &Path, client_key: &str) -> anyhow::Result<()> {
    write_owner_only(key_path, client_key.as_bytes())
}

pub fn uninstall(config_path: &Path, key_path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        config_path.exists(),
        "Grok Build config is not managed by Codex Mixin: {}",
        config_path.display()
    );
    let mut document = read_config(config_path)?;
    validate_managed_config(&document, key_path)?;

    let managed_models = managed_model_ids(&document)?;
    remove_managed_models(&mut document, &managed_models)?;
    let providers = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .context("Grok Build config has no model_providers table")?;
    providers.remove(PROVIDER_ID);
    if providers.is_empty() {
        document.remove("model_providers");
    }
    write_config(config_path, &document)?;
    if key_path.exists() {
        std::fs::remove_file(key_path)
            .with_context(|| format!("remove Grok Build gateway key {}", key_path.display()))?;
    }
    Ok(())
}

fn validate_provider_slot(document: &DocumentMut, key_path: &Path) -> anyhow::Result<()> {
    let Some(providers) = document.get("model_providers") else {
        return Ok(());
    };
    let providers = providers
        .as_table()
        .context("Grok Build model_providers must be a TOML table")?;
    if let Some(provider) = providers.get(PROVIDER_ID) {
        anyhow::ensure!(
            provider_is_managed(provider, key_path),
            "Grok Build provider {PROVIDER_ID} already exists and is not managed by Codex Mixin"
        );
        let models = document
            .get("model")
            .and_then(Item::as_table)
            .context("managed Grok Build provider has no model table")?;
        anyhow::ensure!(
            models.iter().any(|(_, item)| model_is_managed(item)),
            "Grok Build provider {PROVIDER_ID} already exists without Codex Mixin managed models"
        );
    }
    Ok(())
}

fn validate_model_slots(document: &DocumentMut, models: &[GrokBuildModel]) -> anyhow::Result<()> {
    let managed = managed_model_ids(document)?;
    let provider_models = provider_model_ids(document)?;
    anyhow::ensure!(
        provider_models.is_empty() || managed_provider_shape(document),
        "Grok Build config contains models assigned to {PROVIDER_ID} without a managed provider"
    );
    let Some(item) = document.get("model") else {
        return validate_unique_models(models);
    };
    let table = item
        .as_table()
        .context("Grok Build model must be a TOML table")?;
    validate_unique_models(models)?;
    for model in models {
        anyhow::ensure!(
            !table.contains_key(&model.id) || managed.contains(&model.id),
            "Grok Build model {} already exists and is not managed by Codex Mixin",
            model.id
        );
    }
    Ok(())
}

fn managed_provider_shape(document: &DocumentMut) -> bool {
    document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(PROVIDER_ID))
        .and_then(Item::as_table)
        .is_some_and(|provider| {
            provider.get("api_backend").and_then(Item::as_str) == Some(API_BACKEND)
                && provider
                    .get("base_url")
                    .and_then(Item::as_str)
                    .is_some_and(managed_base_url)
        })
}

fn validate_unique_models(models: &[GrokBuildModel]) -> anyhow::Result<()> {
    let mut ids = HashSet::new();
    for model in models {
        anyhow::ensure!(!model.id.trim().is_empty(), "Grok Build model id is empty");
        anyhow::ensure!(
            ids.insert(model.id.as_str()),
            "duplicate Grok Build model id: {}",
            model.id
        );
    }
    Ok(())
}

fn upsert_provider(
    document: &mut DocumentMut,
    key_path: &Path,
    bind: SocketAddr,
) -> anyhow::Result<()> {
    match document.get("model_providers") {
        None => document["model_providers"] = Item::Table(Table::new()),
        Some(item) if !item.is_table() => {
            anyhow::bail!("Grok Build model_providers must be a TOML table");
        }
        Some(_) => {}
    }
    let providers = document["model_providers"]
        .as_table_mut()
        .context("Grok Build model_providers must be a TOML table")?;
    let (command, arguments) = auth_helper(key_path);
    let mut args = Array::new();
    for argument in arguments {
        args.push(argument);
    }
    let mut auth = Table::new();
    auth["command"] = value(command);
    auth["args"] = Item::Value(Value::Array(args));
    auth["timeout_secs"] = value(5);

    let mut provider = Table::new();
    provider["base_url"] = value(format!("http://{bind}/v1"));
    provider["api_backend"] = value(API_BACKEND);
    provider["auth"] = Item::Table(auth);
    providers.insert(PROVIDER_ID, Item::Table(provider));
    Ok(())
}

fn upsert_models(document: &mut DocumentMut, models: &[GrokBuildModel]) -> anyhow::Result<()> {
    match document.get("model") {
        None => document["model"] = Item::Table(Table::new()),
        Some(item) if !item.is_table() => {
            anyhow::bail!("Grok Build model must be a TOML table");
        }
        Some(_) => {}
    }
    let model_table = document["model"]
        .as_table_mut()
        .context("Grok Build model must be a TOML table")?;
    for model in models {
        let context_window = i64::try_from(model.context_window)
            .context("Grok Build model context window exceeds TOML integer range")?;
        let mut entry = Table::new();
        entry["model"] = value(model.id.clone());
        entry["model_provider"] = value(PROVIDER_ID);
        entry["name"] = value(model.name.clone());
        entry["description"] = value(managed_description(&model.description));
        entry["context_window"] = value(context_window);
        entry["max_completion_tokens"] = value(i64::from(model.max_completion_tokens));
        if model.supports_reasoning {
            let mut efforts = Array::new();
            for effort in REASONING_EFFORTS {
                efforts.push(*effort);
            }
            entry["reasoning_efforts"] = Item::Value(Value::Array(efforts));
        } else {
            entry["supports_reasoning_effort"] = value(false);
        }
        model_table.insert(&model.id, Item::Table(entry));
    }
    Ok(())
}

fn managed_provider<'a>(document: &'a DocumentMut, key_path: &Path) -> Option<&'a Item> {
    let provider = document
        .get("model_providers")?
        .as_table()?
        .get(PROVIDER_ID)?;
    let has_managed_model = document
        .get("model")
        .and_then(Item::as_table)
        .is_some_and(|models| models.iter().any(|(_, item)| model_is_managed(item)));
    (provider_is_managed(provider, key_path) && has_managed_model).then_some(provider)
}

fn validate_managed_config(document: &DocumentMut, key_path: &Path) -> anyhow::Result<()> {
    let provider = document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(PROVIDER_ID));
    anyhow::ensure!(
        provider.is_some_and(|provider| provider_is_managed(provider, key_path)),
        "Grok Build provider {PROVIDER_ID} is not managed by Codex Mixin"
    );
    let has_managed_model = document
        .get("model")
        .and_then(Item::as_table)
        .is_some_and(|models| models.iter().any(|(_, item)| model_is_managed(item)));
    anyhow::ensure!(
        has_managed_model,
        "Grok Build provider {PROVIDER_ID} has no Codex Mixin managed models"
    );
    Ok(())
}

fn provider_is_managed(provider: &Item, key_path: &Path) -> bool {
    let Some(provider) = provider.as_table() else {
        return false;
    };
    if provider.get("api_backend").and_then(Item::as_str) != Some(API_BACKEND)
        || !provider
            .get("base_url")
            .and_then(Item::as_str)
            .is_some_and(managed_base_url)
    {
        return false;
    }
    let Some(auth) = provider.get("auth").and_then(Item::as_table) else {
        return false;
    };
    let (expected_command, expected_args) = auth_helper(key_path);
    auth.get("command").and_then(Item::as_str) == Some(expected_command)
        && auth
            .get("args")
            .and_then(Item::as_array)
            .map(|args| {
                args.iter().filter_map(Value::as_str).collect::<Vec<_>>()
                    == expected_args.iter().map(String::as_str).collect::<Vec<_>>()
            })
            .unwrap_or(false)
}

fn managed_base_url(base_url: &str) -> bool {
    let Some(authority) = base_url.strip_prefix("http://") else {
        return false;
    };
    let Some(authority) = authority.strip_suffix("/v1") else {
        return false;
    };
    authority
        .parse::<SocketAddr>()
        .is_ok_and(|address| address.ip().is_loopback())
}

fn managed_model_ids(document: &DocumentMut) -> anyhow::Result<HashSet<String>> {
    Ok(model_table(document)?
        .map(|table| {
            table
                .iter()
                .filter(|(_, item)| model_is_managed(item))
                .map(|(id, _)| id.to_owned())
                .collect()
        })
        .unwrap_or_default())
}

fn provider_model_ids(document: &DocumentMut) -> anyhow::Result<HashSet<String>> {
    Ok(model_table(document)?
        .map(|table| {
            table
                .iter()
                .filter(|(_, item)| model_uses_provider(item))
                .map(|(id, _)| id.to_owned())
                .collect()
        })
        .unwrap_or_default())
}

fn model_table(document: &DocumentMut) -> anyhow::Result<Option<&Table>> {
    let Some(item) = document.get("model") else {
        return Ok(None);
    };
    item.as_table()
        .map(Some)
        .context("Grok Build model must be a TOML table")
}

fn model_is_managed(item: &Item) -> bool {
    model_uses_provider(item)
        && item
            .as_table()
            .and_then(|entry| entry.get("description"))
            .and_then(Item::as_str)
            .is_some_and(|description| description.starts_with(MANAGED_DESCRIPTION_PREFIX))
}

fn model_uses_provider(item: &Item) -> bool {
    item.as_table()
        .and_then(|entry| entry.get("model_provider"))
        .and_then(Item::as_str)
        == Some(PROVIDER_ID)
}

fn remove_managed_models(
    document: &mut DocumentMut,
    managed: &HashSet<String>,
) -> anyhow::Result<()> {
    let Some(models_item) = document.get_mut("model") else {
        return Ok(());
    };
    let models = models_item
        .as_table_mut()
        .context("Grok Build model must be a TOML table")?;
    for id in managed {
        models.remove(id);
    }
    if models.is_empty() {
        document.remove("model");
    }
    Ok(())
}

fn managed_description(description: &str) -> String {
    format!("{MANAGED_DESCRIPTION_PREFIX}{description}")
}

fn auth_helper(key_path: &Path) -> (&'static str, Vec<String>) {
    let path = key_path.to_string_lossy().into_owned();
    if cfg!(windows) {
        (
            "powershell.exe",
            vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                "[Console]::Out.Write([IO.File]::ReadAllText($args[0]))".to_owned(),
                path,
            ],
        )
    } else {
        ("/bin/cat", vec![path])
    }
}

fn read_config(path: &Path) -> anyhow::Result<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read Grok Build config {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    raw.parse::<DocumentMut>()
        .with_context(|| format!("parse Grok Build config {}", path.display()))
}

fn write_config(path: &Path, document: &DocumentMut) -> anyhow::Result<bool> {
    let mut encoded = document.to_string();
    if !encoded.is_empty() && !encoded.ends_with('\n') {
        encoded.push('\n');
    }
    let changed = write_atomic_if_changed(path, encoded.as_bytes())?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> GrokBuildModel {
        GrokBuildModel {
            id: id.to_owned(),
            name: format!("{id} · Test"),
            description: "Codex Mixin model".to_owned(),
            context_window: 128_000,
            max_completion_tokens: 8192,
            supports_reasoning: true,
        }
    }

    #[test]
    fn install_and_uninstall_preserve_unrelated_grok_config() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let key_path = directory.path().join("grok-build-api-key");
        std::fs::write(
            &config_path,
            "[models]\ndefault = \"keep-me\"\n\n[model.user]\nmodel = \"user\"\nbase_url = \"https://example.test/v1\"\n",
        )
        .unwrap();

        assert!(
            install(
                &config_path,
                &key_path,
                "127.0.0.1:8787".parse().unwrap(),
                &[model("vision-custom")],
                "client-secret",
            )
            .unwrap()
        );
        let installed = read_config(&config_path).unwrap();
        assert_eq!(installed["models"]["default"].as_str(), Some("keep-me"));
        assert_eq!(installed["model"]["user"]["model"].as_str(), Some("user"));
        assert_eq!(
            installed["model_providers"][PROVIDER_ID]["base_url"].as_str(),
            Some("http://127.0.0.1:8787/v1")
        );
        assert_eq!(
            installed["model"]["vision-custom"]["model_provider"].as_str(),
            Some(PROVIDER_ID)
        );
        assert_eq!(
            installed["model"]["vision-custom"]["description"].as_str(),
            Some("[codex-mixin managed] vision-custom · Test")
        );
        assert_eq!(
            installed["model"]["vision-custom"]["reasoning_efforts"]
                .as_array()
                .unwrap()
                .len(),
            REASONING_EFFORTS.len()
        );
        assert_eq!(std::fs::read_to_string(&key_path).unwrap(), "client-secret");
        assert!(is_managed(&config_path, &key_path).unwrap());

        uninstall(&config_path, &key_path).unwrap();
        let restored = read_config(&config_path).unwrap();
        assert_eq!(restored["models"]["default"].as_str(), Some("keep-me"));
        assert_eq!(restored["model"]["user"]["model"].as_str(), Some("user"));
        assert!(restored.get("model_providers").is_none());
        assert!(!key_path.exists());
    }

    #[test]
    fn install_refuses_unmanaged_provider_or_model_collisions() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let key_path = directory.path().join("key");
        std::fs::write(
            &config_path,
            "[model_providers.codex-mixin-managed]\nbase_url = \"https://user.test/v1\"\n",
        )
        .unwrap();
        let error = install(
            &config_path,
            &key_path,
            "127.0.0.1:8787".parse().unwrap(),
            &[model("custom")],
            "key",
        )
        .unwrap_err();
        assert!(error.to_string().contains("not managed"));
        assert!(!key_path.exists());

        std::fs::write(
            &config_path,
            "[model.custom]\nmodel = \"user-model\"\nbase_url = \"https://user.test/v1\"\n",
        )
        .unwrap();
        let error = install(
            &config_path,
            &key_path,
            "127.0.0.1:8787".parse().unwrap(),
            &[model("custom")],
            "key",
        )
        .unwrap_err();
        assert!(error.to_string().contains("model custom already exists"));
        assert!(!key_path.exists());

        std::fs::write(
            &config_path,
            "[model.stale]\nmodel = \"stale\"\nmodel_provider = \"codex-mixin-managed\"\n",
        )
        .unwrap();
        let error = install(
            &config_path,
            &key_path,
            "127.0.0.1:8787".parse().unwrap(),
            &[model("fresh")],
            "key",
        )
        .unwrap_err();
        assert!(error.to_string().contains("without a managed provider"));
        assert!(!key_path.exists());
    }

    #[test]
    fn refresh_removes_stale_managed_models_and_keeps_user_models() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let key_path = directory.path().join("key");
        install(
            &config_path,
            &key_path,
            "127.0.0.1:8787".parse().unwrap(),
            &[model("old")],
            "first-key",
        )
        .unwrap();
        let mut document = read_config(&config_path).unwrap();
        let mut user = Table::new();
        user["model"] = value("user-model");
        user["base_url"] = value("https://user.test/v1");
        document["model"]
            .as_table_mut()
            .unwrap()
            .insert("user", Item::Table(user));
        write_config(&config_path, &document).unwrap();

        install(
            &config_path,
            &key_path,
            "127.0.0.1:9898".parse().unwrap(),
            &[model("new")],
            "second-key",
        )
        .unwrap();
        let refreshed = read_config(&config_path).unwrap();
        assert!(refreshed["model"].get("old").is_none());
        assert!(refreshed["model"].get("new").is_some());
        assert!(refreshed["model"].get("user").is_some());
        assert_eq!(
            refreshed["model_providers"][PROVIDER_ID]["base_url"].as_str(),
            Some("http://127.0.0.1:9898/v1")
        );
        assert_eq!(std::fs::read_to_string(&key_path).unwrap(), "second-key");
        assert!(
            !install(
                &config_path,
                &key_path,
                "127.0.0.1:9898".parse().unwrap(),
                &[model("new")],
                "second-key",
            )
            .unwrap()
        );
    }

    #[test]
    fn managed_provider_must_still_target_the_loopback_gateway() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let key_path = directory.path().join("key");
        install(
            &config_path,
            &key_path,
            "127.0.0.1:8787".parse().unwrap(),
            &[model("managed")],
            "key",
        )
        .unwrap();
        let mut document = read_config(&config_path).unwrap();
        document["model_providers"][PROVIDER_ID]["base_url"] =
            value("https://unrelated.example/v1");
        write_config(&config_path, &document).unwrap();

        assert!(!is_managed(&config_path, &key_path).unwrap());
        let error = install(
            &config_path,
            &key_path,
            "127.0.0.1:8787".parse().unwrap(),
            &[model("managed")],
            "key",
        )
        .unwrap_err();
        assert!(error.to_string().contains("not managed"));
    }

    #[test]
    fn install_rejects_a_non_loopback_gateway() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let key_path = directory.path().join("key");
        let error = install(
            &config_path,
            &key_path,
            "0.0.0.0:8787".parse().unwrap(),
            &[model("managed")],
            "key",
        )
        .unwrap_err();
        assert!(error.to_string().contains("loopback"));
        assert!(!config_path.exists());
        assert!(!key_path.exists());
    }
}
