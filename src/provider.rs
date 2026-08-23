use std::{env, time::Duration};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use thiserror::Error;

use crate::{
    config::{ModelOptions, ModelRef, Protocol, ResolvedConfig},
    model::{Model, OpenAiChatModel, OpenAiModel, OpenAiModelOptions},
};

pub struct ProviderSystem {
    config: ResolvedConfig,
}

impl ProviderSystem {
    pub fn new(config: ResolvedConfig) -> Self {
        Self { config }
    }

    pub fn selected_model(&self) -> &ModelRef {
        self.config.selected_model()
    }

    pub fn select(&self, reference: &ModelRef) -> Result<SelectedModel, ProviderError> {
        self.select_with_env(reference, |name| env::var(name).ok())
    }

    fn select_with_env(
        &self,
        reference: &ModelRef,
        environment: impl Fn(&str) -> Option<String>,
    ) -> Result<SelectedModel, ProviderError> {
        let provider = self
            .config
            .providers()
            .get(reference.provider())
            .ok_or_else(|| {
                let suggestions = closest(reference.provider(), self.config.providers().keys());
                ProviderError::UnknownProvider {
                    provider: reference.provider().to_owned(),
                    suggestions,
                }
            })?;
        let api_key = match &provider.api_key_env {
            Some(name) => {
                Some(
                    environment(name).ok_or_else(|| ProviderError::MissingCredential {
                        provider: reference.provider().to_owned(),
                        environment: name.clone(),
                    })?,
                )
            }
            None => None,
        };
        let headers = provider
            .headers
            .iter()
            .map(|(name, value)| {
                Ok((
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                        ProviderError::InvalidProvider {
                            provider: reference.provider().to_owned(),
                            message: format!("invalid header name {name}"),
                        }
                    })?,
                    HeaderValue::from_str(value).map_err(|_| ProviderError::InvalidProvider {
                        provider: reference.provider().to_owned(),
                        message: format!("invalid header value for {name}"),
                    })?,
                ))
            })
            .collect::<Result<HeaderMap, ProviderError>>()?;
        let model_config = provider
            .models
            .get(reference.model())
            .cloned()
            .unwrap_or_default();
        validate_model_options(reference, provider.protocol, &model_config.options)?;
        let options = OpenAiModelOptions {
            reasoning_effort: model_config
                .options
                .reasoning_effort
                .map(|value| value.as_str()),
            text_verbosity: model_config
                .options
                .text_verbosity
                .map(|value| value.as_str()),
            temperature: model_config.options.temperature,
            max_output_tokens: model_config.options.max_output_tokens,
        };
        let timeout = provider.timeout_ms.map(Duration::from_millis);
        let stream_idle_timeout = provider.stream_idle_timeout_ms.map(Duration::from_millis);
        let endpoint_name = match provider.protocol {
            Protocol::OpenAiResponses => "responses",
            Protocol::OpenAiChat => "chat/completions",
        };
        let endpoint = format!(
            "{}/{endpoint_name}",
            provider.base_url.as_str().trim_end_matches('/')
        );
        let model: Box<dyn Model> = match provider.protocol {
            Protocol::OpenAiResponses => Box::new(
                OpenAiModel::configured(
                    endpoint,
                    api_key,
                    headers,
                    options,
                    timeout,
                    stream_idle_timeout,
                )
                .map_err(|error| ProviderError::Initialization {
                    provider: reference.provider().to_owned(),
                    message: error.to_string(),
                })?,
            ),
            Protocol::OpenAiChat => Box::new(
                OpenAiChatModel::configured(
                    endpoint,
                    api_key,
                    headers,
                    options,
                    timeout,
                    stream_idle_timeout,
                )
                .map_err(|error| ProviderError::Initialization {
                    provider: reference.provider().to_owned(),
                    message: error.to_string(),
                })?,
            ),
        };
        let provider_name = provider.name.as_deref().unwrap_or(reference.provider());
        let model_name = model_config.name.as_deref().unwrap_or(reference.model());

        Ok(SelectedModel {
            reference: reference.clone(),
            display_name: format!("{provider_name} / {model_name}"),
            model,
        })
    }
}

pub struct SelectedModel {
    reference: ModelRef,
    display_name: String,
    model: Box<dyn Model>,
}

impl SelectedModel {
    pub fn reference(&self) -> &ModelRef {
        &self.reference
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn into_model(self) -> Box<dyn Model> {
        self.model
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unknown provider profile {provider}{suggestions}")]
    UnknownProvider {
        provider: String,
        suggestions: Suggestions,
    },
    #[error("provider {provider} requires environment variable {environment}")]
    MissingCredential {
        provider: String,
        environment: String,
    },
    #[error("invalid provider {provider}: {message}")]
    InvalidProvider { provider: String, message: String },
    #[error("invalid model options for {reference}: {message}")]
    InvalidModelOptions {
        reference: ModelRef,
        message: String,
    },
    #[error("failed to initialize provider {provider}: {message}")]
    Initialization { provider: String, message: String },
}

#[derive(Debug)]
pub struct Suggestions(Vec<String>);

impl std::fmt::Display for Suggestions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            Ok(())
        } else {
            write!(formatter, "; did you mean {}?", self.0.join(", "))
        }
    }
}

fn validate_model_options(
    reference: &ModelRef,
    protocol: Protocol,
    options: &ModelOptions,
) -> Result<(), ProviderError> {
    if protocol == Protocol::OpenAiChat && options.reasoning_effort.is_some() {
        return Err(ProviderError::InvalidModelOptions {
            reference: reference.clone(),
            message: "reasoningEffort is supported only by openai-responses".into(),
        });
    }
    if protocol == Protocol::OpenAiChat && options.text_verbosity.is_some() {
        return Err(ProviderError::InvalidModelOptions {
            reference: reference.clone(),
            message: "textVerbosity is supported only by openai-responses".into(),
        });
    }
    Ok(())
}

fn closest<'a>(query: &str, candidates: impl Iterator<Item = &'a String>) -> Suggestions {
    let mut ranked = candidates
        .map(|candidate| (edit_distance(query, candidate), candidate.clone()))
        .filter(|(distance, _)| *distance <= 3)
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(distance, candidate)| (*distance, candidate.clone()));
    Suggestions(
        ranked
            .into_iter()
            .take(3)
            .map(|(_, candidate)| candidate)
            .collect(),
    )
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_char) in right.chars().enumerate() {
            current.push(
                (current[right_index] + 1)
                    .min(previous[right_index + 1] + 1)
                    .min(previous[right_index] + usize::from(left_char != right_char)),
            );
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crate::config::resolve_test_config;

    use super::*;

    #[test]
    fn selected_provider_resolves_its_credential_late() {
        let config = resolve_test_config(
            r#"{
                "model": "openai/gpt-test",
                "provider": {
                    "openai": {
                        "models": { "gpt-test": { "name": "GPT Test" } }
                    }
                }
            }"#,
        )
        .unwrap();
        let system = ProviderSystem::new(config);

        let selected = system
            .select_with_env(system.selected_model(), |name| {
                (name == "OPENAI_API_KEY").then(|| "secret".into())
            })
            .unwrap();

        assert_eq!(selected.reference().to_string(), "openai/gpt-test");
        assert_eq!(selected.display_name(), "OpenAI / GPT Test");
    }

    #[test]
    fn local_chat_provider_can_be_selected_without_a_credential() {
        let config = resolve_test_config(
            r#"{
                "model": "ollama/qwen3-coder",
                "provider": {
                    "ollama": {
                        "protocol": "openai-chat",
                        "options": { "baseURL": "http://127.0.0.1:11434/v1" }
                    }
                }
            }"#,
        )
        .unwrap();
        let system = ProviderSystem::new(config);

        let selected = system
            .select_with_env(system.selected_model(), |_| None)
            .unwrap();

        assert_eq!(selected.reference().to_string(), "ollama/qwen3-coder");
    }

    #[test]
    fn missing_provider_has_a_suggestion() {
        let config = resolve_test_config("{}").unwrap();
        let system = ProviderSystem::new(config);
        let reference: ModelRef = "opena/gpt-test".parse().unwrap();

        let error = system
            .select_with_env(&reference, |_| Some("secret".into()))
            .err()
            .expect("unknown provider should fail");

        assert!(error.to_string().contains("did you mean openai"));
    }
}
