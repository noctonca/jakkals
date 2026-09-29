//! The profile: one TOML file that is the harness. Read, validated and
//! hashed before a run starts; a run records the hash. See the profile
//! table in docs/ARCHITECTURE.md for every field and its default.

use std::fmt;
use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use toml::Spanned;

use crate::money;
use crate::provider::http::{self, HttpConfig, OPENROUTER_BASE_URL};
use crate::run::Limits;

/// The largest profile read. A profile is a page of TOML and a system
/// prompt; a file past this is not a profile.
const PROFILE_CAP_BYTES: u64 = 1024 * 1024;

/// The default for `limits.tool_output_bytes`: 32 KiB, about 8,000
/// tokens of text. A choice, not a measurement: room for a long file
/// or a search's hits, small next to any model's window.
pub const TOOL_OUTPUT_BYTES_DEFAULT: u32 = 32 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// Sent verbatim as the system message; empty means none is sent.
    pub system_prompt: String,
    pub limits: Limits,
    pub provider: ProviderSettings,
    /// `sha256:` and the hex digest of the file's bytes, so a run can be
    /// matched to its profile with standard tools.
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderSettings {
    pub base_url: String,
    /// The environment variable holding the key; `None` sends no key.
    pub api_key_env: Option<String>,
    pub params: Map<String, Value>,
}

/// Why a profile can't be used. Each names the field at fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileError {
    /// The file couldn't be read, or is past [`PROFILE_CAP_BYTES`].
    Read { detail: String },
    /// Not TOML, or not a profile: a syntax error, a wrong type, a
    /// missing required field or an unknown one, with its position.
    Toml { detail: String },
    /// A field has a value the harness can't run with.
    Invalid {
        field: &'static str,
        problem: &'static str,
    },
    /// A field for a capability that isn't built yet. Refused rather
    /// than ignored, so a profile never claims what a run won't do.
    NotBuilt { field: &'static str },
    /// `provider.api_key_env` names a variable that is unset or empty.
    KeyUnset { variable: String },
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { detail } => write!(formatter, "can't read the profile: {detail}"),
            Self::Toml { detail } => write!(formatter, "the profile isn't valid: {detail}"),
            Self::Invalid { field, problem } => write!(formatter, "profile `{field}` {problem}"),
            Self::NotBuilt { field } => write!(
                formatter,
                "profile `{field}` is for a capability not built yet; remove it"
            ),
            Self::KeyUnset { variable } => write!(
                formatter,
                "provider.api_key_env names `{variable}`, which is unset or empty"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

impl Profile {
    pub fn read(path: &Path) -> Result<Self, ProfileError> {
        let read_error = |error: std::io::Error| ProfileError::Read {
            detail: format!("{}: {error}", path.display()),
        };
        let file = std::fs::File::open(path).map_err(read_error)?;
        let mut bytes = Vec::new();
        file.take(PROFILE_CAP_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
        if u64::try_from(bytes.len()).expect("length fits u64") > PROFILE_CAP_BYTES {
            return Err(ProfileError::Read {
                detail: format!("{}: larger than {PROFILE_CAP_BYTES} bytes", path.display()),
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| ProfileError::Read {
            detail: format!("{}: not UTF-8", path.display()),
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        let file: File = toml::from_str(text).map_err(|error| ProfileError::Toml {
            detail: error.to_string(),
        })?;
        if file.tools.is_some() {
            return Err(ProfileError::NotBuilt { field: "tools" });
        }
        if file.mcp.is_some() {
            return Err(ProfileError::NotBuilt { field: "mcp" });
        }
        let limits = limits(&file.limits, text)?;
        let provider = provider(file.provider)?;
        Ok(Self {
            system_prompt: file.system_prompt,
            limits,
            provider,
            hash: hash(text.as_bytes()),
        })
    }

    /// The HTTP provider's configuration, with the key read through
    /// `env` (the process environment, in a run).
    pub fn http_config(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<HttpConfig, ProfileError> {
        let api_key = match &self.provider.api_key_env {
            None => None,
            Some(variable) => {
                Some(env(variable).filter(|key| !key.is_empty()).ok_or_else(|| {
                    ProfileError::KeyUnset {
                        variable: variable.clone(),
                    }
                })?)
            }
        };
        Ok(HttpConfig {
            base_url: self.provider.base_url.clone(),
            api_key,
            params: self.provider.params.clone(),
        })
    }
}

fn limits(file: &FileLimits, text: &str) -> Result<Limits, ProfileError> {
    let positive = |field: &'static str, value: u64| {
        if value == 0 {
            Err(ProfileError::Invalid {
                field,
                problem: "must be positive",
            })
        } else {
            Ok(())
        }
    };
    positive("limits.steps", file.steps.into())?;
    positive("limits.wall_s", file.wall_s.into())?;
    if let Some(tokens) = file.tokens {
        positive("limits.tokens", tokens)?;
    }
    if let Some(context_tokens) = file.context_tokens {
        positive("limits.context_tokens", context_tokens.into())?;
    }
    if let Some(tool_output_bytes) = file.tool_output_bytes {
        positive("limits.tool_output_bytes", tool_output_bytes.into())?;
    }
    let cost_nano_usd = match &file.cost_usd {
        None => None,
        Some(cost) => {
            // Read from the digits written in the file, so the limit is
            // exactly what the profile says, not its nearest float.
            let literal: String = text[cost.span()]
                .trim_start_matches('+')
                .chars()
                .filter(|character| *character != '_')
                .collect();
            let nano = money::nano_usd(&literal).ok_or(ProfileError::Invalid {
                field: "limits.cost_usd",
                problem: "must be a number of US dollars",
            })?;
            positive("limits.cost_usd", nano)?;
            Some(nano)
        }
    };
    Ok(Limits {
        steps: file.steps,
        wall_s: file.wall_s,
        cost_nano_usd,
        tokens: file.tokens,
        context_tokens: file.context_tokens,
        tool_output_bytes: file.tool_output_bytes.unwrap_or(TOOL_OUTPUT_BYTES_DEFAULT),
    })
}

fn provider(file: FileProvider) -> Result<ProviderSettings, ProfileError> {
    let base_url = file
        .base_url
        .unwrap_or_else(|| OPENROUTER_BASE_URL.to_owned());
    if http::chat_url(&base_url).is_none() {
        return Err(ProfileError::Invalid {
            field: "provider.base_url",
            problem: "must be an http or https URL",
        });
    }
    if file
        .api_key_env
        .as_ref()
        .is_some_and(|variable| variable.is_empty())
    {
        return Err(ProfileError::Invalid {
            field: "provider.api_key_env",
            problem: "must name a variable; leave it out to send no key",
        });
    }
    if http::RESERVED_PARAMS
        .iter()
        .any(|name| file.params.contains_key(*name))
    {
        return Err(ProfileError::Invalid {
            field: "provider.params",
            problem: "may not set model, messages, tools or stream",
        });
    }
    Ok(ProviderSettings {
        base_url,
        api_key_env: file.api_key_env,
        params: file.params,
    })
}

fn hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hash = String::from("sha256:");
    for byte in digest.iter() {
        hash.push_str(&format!("{byte:02x}"));
    }
    hash
}

// The file as written. Unknown fields are refused everywhere but
// `provider.params`, so a misspelt limit is an error, not a run without it.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    system_prompt: String,
    limits: FileLimits,
    #[serde(default)]
    provider: FileProvider,
    tools: Option<toml::Value>,
    mcp: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLimits {
    steps: u32,
    wall_s: u32,
    /// Only its span is used: see [`limits`].
    cost_usd: Option<Spanned<f64>>,
    tokens: Option<u64>,
    context_tokens: Option<u32>,
    tool_output_bytes: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileProvider {
    base_url: Option<String>,
    api_key_env: Option<String>,
    #[serde(default)]
    params: Map<String, Value>,
}

#[cfg(test)]
mod tests;
