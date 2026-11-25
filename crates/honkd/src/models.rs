use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct ModelsFile {
    #[serde(default)]
    pub providers: HashMap<String, Vec<ModelEntry>>,
}

pub async fn load_for_list(path: Option<&Path>) -> (Option<ModelsFile>, Option<String>) {
    let Some(p) = path else {
        return (None, None);
    };
    match tokio::fs::read_to_string(p).await {
        Ok(content) => match serde_json::from_str::<ModelsFile>(&content) {
            Ok(mf) => (Some(mf), None),
            Err(e) => (
                None,
                Some(format!("failed to parse models file {}: {e}", p.display())),
            ),
        },
        Err(e) => (
            None,
            Some(format!("failed to read models file {}: {e}", p.display())),
        ),
    }
}

pub fn list_reply(
    configured: Option<&ModelsFile>,
    warning: Option<String>,
    init_result: &Value,
    all_providers: bool,
) -> Value {
    let config_options = init_result.get("configOptions").and_then(Value::as_array);

    let provider_opt = config_options.and_then(|opts| {
        opts.iter()
            .find(|o| o.get("id").and_then(Value::as_str) == Some("provider"))
    });
    let provider = provider_opt
        .and_then(|o| o.get("currentValue").and_then(Value::as_str))
        .map(str::to_string);

    let model_opt = config_options.and_then(|opts| {
        opts.iter()
            .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))
    });
    let current = model_opt
        .and_then(|o| o.get("currentValue").and_then(Value::as_str))
        .map(str::to_string);

    let mut models = Vec::new();
    let mut seen: HashSet<(Option<String>, String)> = HashSet::new();

    if let Some(ref prov) = provider
        && let Some(cfg) = configured
        && let Some(entries) = cfg.providers.get(prov)
    {
        for entry in entries {
            if entry.id.is_empty() {
                continue;
            }
            if seen.insert((Some(prov.clone()), entry.id.clone())) {
                let name = entry.name.clone().unwrap_or_else(|| entry.id.clone());
                models.push(json!({
                    "id": entry.id,
                    "name": name,
                    "source": "configured",
                    "provider": prov,
                }));
            }
        }
    }

    if let Some(m_opt) = model_opt
        && let Some(goose_opts) = m_opt.get("options").and_then(Value::as_array)
    {
        for g_opt in goose_opts {
            let val = g_opt.get("value").and_then(Value::as_str).unwrap_or("");
            if !val.is_empty() && seen.insert((provider.clone(), val.to_string())) {
                let name = g_opt.get("name").and_then(Value::as_str).unwrap_or(val);
                let mut entry_json = json!({
                    "id": val,
                    "name": name,
                    "source": "goose",
                });
                if let Some(ref prov) = provider {
                    entry_json["provider"] = json!(prov);
                } else {
                    entry_json["provider"] = Value::Null;
                }
                models.push(entry_json);
            }
        }
    }

    if all_providers && let Some(cfg) = configured {
        let mut other_providers: Vec<&String> = cfg
            .providers
            .keys()
            .filter(|&k| provider.as_deref() != Some(k.as_str()))
            .collect();
        other_providers.sort();

        for other_prov in other_providers {
            if let Some(entries) = cfg.providers.get(other_prov) {
                for entry in entries {
                    if entry.id.is_empty() {
                        continue;
                    }
                    if seen.insert((Some(other_prov.clone()), entry.id.clone())) {
                        let name = entry.name.clone().unwrap_or_else(|| entry.id.clone());
                        models.push(json!({
                            "id": entry.id,
                            "name": name,
                            "source": "configured",
                            "provider": other_prov,
                        }));
                    }
                }
            }
        }
    }

    let mut reply = json!({
        "provider": provider,
        "current": current,
        "models": models,
    });

    if let Some(w) = warning {
        reply["warning"] = json!(w);
    }

    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_init_result(provider: Option<&str>, model: Option<&str>) -> Value {
        let mut opts = Vec::new();
        if let Some(p) = provider {
            opts.push(json!({
                "id": "provider",
                "name": "Provider",
                "type": "select",
                "currentValue": p,
                "options": [
                    { "name": "Goose (Default)", "value": "goose" },
                    { "name": "GCP Vertex AI", "value": "gcp_vertex_ai" }
                ]
            }));
        }
        if let Some(m) = model {
            opts.push(json!({
                "id": "model",
                "name": "Model",
                "type": "select",
                "currentValue": m,
                "options": [
                    { "name": "Claude Opus 5.5", "value": "claude-opus-5-5" },
                    { "name": "Gemini 3.8 Flash", "value": "gemini-3.8-flash" },
                    { "name": "Goose Model 3", "value": "model-3" }
                ]
            }));
        }
        json!({
            "sessionId": "test_sid",
            "configOptions": opts
        })
    }

    #[test]
    fn test_order_and_dedupe() {
        let mut providers = HashMap::new();
        providers.insert(
            "gcp_vertex_ai".to_string(),
            vec![
                ModelEntry {
                    id: "custom-1".to_string(),
                    name: Some("Custom 1".to_string()),
                },
                ModelEntry {
                    id: "claude-opus-5-5".to_string(),
                    name: None, // defaults to id
                },
                ModelEntry {
                    id: "custom-1".to_string(),
                    name: Some("Duplicate".to_string()),
                },
            ],
        );
        let mf = ModelsFile { providers };
        let init_result = sample_init_result(Some("gcp_vertex_ai"), Some("claude-opus-5-5"));

        let reply = list_reply(Some(&mf), None, &init_result, false);

        assert_eq!(reply["provider"], "gcp_vertex_ai");
        assert_eq!(reply["current"], "claude-opus-5-5");
        assert!(reply.get("warning").is_none());

        let models = reply["models"].as_array().expect("models array");
        assert_eq!(models.len(), 4);

        // 1. Configured custom-1
        assert_eq!(models[0]["id"], "custom-1");
        assert_eq!(models[0]["name"], "Custom 1");
        assert_eq!(models[0]["source"], "configured");
        assert_eq!(models[0]["provider"], "gcp_vertex_ai");

        // 2. Configured claude-opus-5-5 (name defaults to id)
        assert_eq!(models[1]["id"], "claude-opus-5-5");
        assert_eq!(models[1]["name"], "claude-opus-5-5");
        assert_eq!(models[1]["source"], "configured");
        assert_eq!(models[1]["provider"], "gcp_vertex_ai");

        // 3. Goose gemini-3.8-flash (claude-opus-5-5 was already listed)
        assert_eq!(models[2]["id"], "gemini-3.8-flash");
        assert_eq!(models[2]["name"], "Gemini 3.8 Flash");
        assert_eq!(models[2]["source"], "goose");
        assert_eq!(models[2]["provider"], "gcp_vertex_ai");

        // 4. Goose model-3
        assert_eq!(models[3]["id"], "model-3");
        assert_eq!(models[3]["name"], "Goose Model 3");
        assert_eq!(models[3]["source"], "goose");
        assert_eq!(models[3]["provider"], "gcp_vertex_ai");
    }

    #[test]
    fn test_missing_provider_key() {
        let mut providers = HashMap::new();
        providers.insert(
            "other_provider".to_string(),
            vec![ModelEntry {
                id: "other-model".to_string(),
                name: Some("Other".to_string()),
            }],
        );
        let mf = ModelsFile { providers };
        let init_result = sample_init_result(Some("gcp_vertex_ai"), Some("gemini-3.8-flash"));

        let reply = list_reply(Some(&mf), None, &init_result, false);
        assert_eq!(reply["provider"], "gcp_vertex_ai");
        assert_eq!(reply["current"], "gemini-3.8-flash");

        let models = reply["models"].as_array().expect("models array");
        assert_eq!(models.len(), 3);
        assert!(models.iter().all(|m| m["source"] == "goose"));
        assert!(models.iter().all(|m| m["provider"] == "gcp_vertex_ai"));
    }

    #[test]
    fn test_absent_config_options() {
        let mut providers = HashMap::new();
        providers.insert(
            "gcp_vertex_ai".to_string(),
            vec![ModelEntry {
                id: "custom-1".to_string(),
                name: None,
            }],
        );
        let mf = ModelsFile { providers };
        let init_result = json!({ "sessionId": "test_sid" });

        let reply = list_reply(Some(&mf), None, &init_result, false);
        assert!(reply["provider"].is_null());
        assert!(reply["current"].is_null());
        let models = reply["models"].as_array().expect("models array");
        assert!(models.is_empty());
    }

    #[test]
    fn test_all_providers_false_only_current_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "gcp_vertex_ai".to_string(),
            vec![ModelEntry {
                id: "vertex-model".to_string(),
                name: None,
            }],
        );
        providers.insert(
            "codex_lb".to_string(),
            vec![ModelEntry {
                id: "gpt-6-astra".to_string(),
                name: Some("GPT-6 Astra".to_string()),
            }],
        );
        let mf = ModelsFile { providers };
        let init_result = sample_init_result(Some("gcp_vertex_ai"), Some("gemini-3.8-flash"));

        let reply = list_reply(Some(&mf), None, &init_result, false);
        let models = reply["models"].as_array().expect("models array");
        assert!(!models.iter().any(|m| m["id"] == "gpt-6-astra"));
        assert!(models.iter().all(|m| m["provider"] == "gcp_vertex_ai"));
    }

    #[test]
    fn test_all_providers_true_appends_other_providers_sorted() {
        let mut providers = HashMap::new();
        providers.insert(
            "gcp_vertex_ai".to_string(),
            vec![ModelEntry {
                id: "vertex-model".to_string(),
                name: None,
            }],
        );
        providers.insert(
            "zebra_provider".to_string(),
            vec![ModelEntry {
                id: "z-model".to_string(),
                name: None,
            }],
        );
        providers.insert(
            "codex_lb".to_string(),
            vec![
                ModelEntry {
                    id: "gpt-6-astra".to_string(),
                    name: Some("GPT-6 Astra".to_string()),
                },
                ModelEntry {
                    id: "gpt-6.1-sol".to_string(),
                    name: None,
                },
            ],
        );
        let mf = ModelsFile { providers };
        let init_result = sample_init_result(Some("gcp_vertex_ai"), Some("claude-opus-5-5"));

        let reply = list_reply(Some(&mf), None, &init_result, true);
        let models = reply["models"].as_array().expect("models array");

        // 1: current provider configured (vertex-model)
        assert_eq!(models[0]["id"], "vertex-model");
        assert_eq!(models[0]["provider"], "gcp_vertex_ai");

        // 2..4: goose options from current provider
        assert_eq!(models[1]["id"], "claude-opus-5-5");
        assert_eq!(models[1]["source"], "goose");
        assert_eq!(models[1]["provider"], "gcp_vertex_ai");
        assert_eq!(models[2]["id"], "gemini-3.8-flash");
        assert_eq!(models[2]["provider"], "gcp_vertex_ai");
        assert_eq!(models[3]["id"], "model-3");
        assert_eq!(models[3]["provider"], "gcp_vertex_ai");

        // 5..6: codex_lb (sorted before zebra_provider) in file order
        assert_eq!(models[4]["id"], "gpt-6-astra");
        assert_eq!(models[4]["name"], "GPT-6 Astra");
        assert_eq!(models[4]["source"], "configured");
        assert_eq!(models[4]["provider"], "codex_lb");

        assert_eq!(models[5]["id"], "gpt-6.1-sol");
        assert_eq!(models[5]["name"], "gpt-6.1-sol");
        assert_eq!(models[5]["source"], "configured");
        assert_eq!(models[5]["provider"], "codex_lb");

        // 7: zebra_provider
        assert_eq!(models[6]["id"], "z-model");
        assert_eq!(models[6]["source"], "configured");
        assert_eq!(models[6]["provider"], "zebra_provider");
    }

    #[test]
    fn test_per_provider_id_dedup() {
        let mut providers = HashMap::new();
        providers.insert(
            "gcp_vertex_ai".to_string(),
            vec![
                ModelEntry {
                    id: "shared-model".to_string(),
                    name: Some("Vertex Shared".to_string()),
                },
                ModelEntry {
                    id: "shared-model".to_string(), // duplicate within provider
                    name: Some("Vertex Duplicate".to_string()),
                },
            ],
        );
        providers.insert(
            "codex_lb".to_string(),
            vec![
                ModelEntry {
                    id: "shared-model".to_string(), // same ID across providers -> kept!
                    name: Some("Codex Shared".to_string()),
                },
                ModelEntry {
                    id: "shared-model".to_string(), // duplicate within codex_lb -> deduped
                    name: Some("Codex Duplicate".to_string()),
                },
            ],
        );
        let mf = ModelsFile { providers };
        let init_result = sample_init_result(Some("gcp_vertex_ai"), None);

        let reply = list_reply(Some(&mf), None, &init_result, true);
        let models = reply["models"].as_array().expect("models array");

        let shared_entries: Vec<_> = models
            .iter()
            .filter(|m| m["id"] == "shared-model")
            .collect();
        assert_eq!(shared_entries.len(), 2);
        assert_eq!(shared_entries[0]["provider"], "gcp_vertex_ai");
        assert_eq!(shared_entries[0]["name"], "Vertex Shared");
        assert_eq!(shared_entries[1]["provider"], "codex_lb");
        assert_eq!(shared_entries[1]["name"], "Codex Shared");
    }

    #[test]
    fn test_skip_empty_configured_id() {
        let mut providers = HashMap::new();
        providers.insert(
            "gcp_vertex_ai".to_string(),
            vec![
                ModelEntry {
                    id: "".to_string(),
                    name: Some("Empty ID".to_string()),
                },
                ModelEntry {
                    id: "valid-id".to_string(),
                    name: None,
                },
            ],
        );
        let mf = ModelsFile { providers };
        let init_result = sample_init_result(Some("gcp_vertex_ai"), None);
        let reply = list_reply(Some(&mf), None, &init_result, false);
        let models = reply["models"].as_array().expect("models array");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "valid-id");
        assert_eq!(models[0]["provider"], "gcp_vertex_ai");
    }

    #[tokio::test]
    async fn test_load_for_list_and_garbled_file_warning() {
        assert_eq!(load_for_list(None).await, (None, None));

        let temp_dir = tempfile::tempdir().unwrap();
        let valid_path = temp_dir.path().join("models.json");
        tokio::fs::write(
            &valid_path,
            r#"{"providers": {"p1": [{"id": "m1", "name": "Model 1"}]}}"#,
        )
        .await
        .unwrap();

        let (loaded, warning) = load_for_list(Some(&valid_path)).await;
        assert!(warning.is_none());
        let loaded = loaded.expect("load valid");
        assert_eq!(loaded.providers["p1"].len(), 1);
        assert_eq!(loaded.providers["p1"][0].id, "m1");

        let garbled_path = temp_dir.path().join("garbled.json");
        tokio::fs::write(&garbled_path, "{ invalid json")
            .await
            .unwrap();
        let (loaded_garbled, warning_garbled) = load_for_list(Some(&garbled_path)).await;
        assert!(loaded_garbled.is_none());
        assert!(warning_garbled.is_some());

        let nonexistent_path = temp_dir.path().join("nonexistent.json");
        let (loaded_missing, warning_missing) = load_for_list(Some(&nonexistent_path)).await;
        assert!(loaded_missing.is_none());
        assert!(warning_missing.is_some());

        let init_result = sample_init_result(Some("p1"), Some("m1"));
        let reply = list_reply(
            None,
            Some("warning: file garbled".to_string()),
            &init_result,
            false,
        );
        assert_eq!(reply["warning"], "warning: file garbled");
    }
}
