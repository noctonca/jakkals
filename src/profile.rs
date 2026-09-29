//! The profile: one TOML file that is the harness. Read, validated and
//! hashed before a run starts; a run records the hash. See the profile
//! table in docs/ARCHITECTURE.md for every field and its default.

use std::collections::BTreeMap;
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
use crate::tools::local::LocalTool;
use crate::tools::mcp::{
    CALL_TIMEOUT_S_DEFAULT, KeySettings, McpSettings, RESERVED_HEADERS, SERVER_NAME_CAP_BYTES,
    TOOL_NAME_CAP_BYTES,
};
use crate::tools::shell::{self, SHELL_TIMEOUT_S_DEFAULT, Sandbox, ShellSettings, WordsError};

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
    /// The local tools offered, in the order they are offered, each once.
    pub local_tools: Vec<LocalTool>,
    /// The shell's settings, exactly when `shell` is offered.
    pub shell: Option<ShellSettings>,
    /// The MCP servers, ordered by name.
    pub mcp: Vec<McpSettings>,
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
    /// An MCP server's table has a value the harness can't run with:
    /// the server's name itself when `field` is `None`.
    InvalidServer {
        server: String,
        field: Option<&'static str>,
        problem: &'static str,
    },
    /// A key's variable (`provider.api_key_env`, `mcp.<name>.key_env`)
    /// is unset or empty.
    KeyUnset { field: String, variable: String },
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { detail } => write!(formatter, "can't read the profile: {detail}"),
            Self::Toml { detail } => write!(formatter, "the profile isn't valid: {detail}"),
            Self::Invalid { field, problem } => write!(formatter, "profile `{field}` {problem}"),
            Self::InvalidServer {
                server,
                field: None,
                problem,
            } => write!(formatter, "profile `mcp.{server}` {problem}"),
            Self::InvalidServer {
                server,
                field: Some(field),
                problem,
            } => write!(formatter, "profile `mcp.{server}.{field}` {problem}"),
            Self::KeyUnset { field, variable } => write!(
                formatter,
                "{field} names `{variable}`, which is unset or empty"
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
        let limits = limits(&file.limits, text)?;
        let provider = provider(file.provider)?;
        let local_tools = local_tools(&file.tools.local)?;
        let shell = shell_settings(file.tools, local_tools.contains(&LocalTool::Shell))?;
        let mcp = mcp_servers(file.mcp)?;
        Ok(Self {
            system_prompt: file.system_prompt,
            limits,
            provider,
            local_tools,
            shell,
            mcp,
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
            Some(variable) => Some(read_key("provider.api_key_env", variable, &env)?),
        };
        Ok(HttpConfig {
            base_url: self.provider.base_url.clone(),
            api_key,
            params: self.provider.params.clone(),
        })
    }
}

/// An MCP server's key, read through `env` (the process environment,
/// in a run); `None` when the profile names no key for it.
pub fn mcp_key(
    server: &McpSettings,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, ProfileError> {
    server
        .key
        .as_ref()
        .map(|key| read_key(&format!("mcp.{}.key_env", server.name), &key.env, &env))
        .transpose()
}

fn read_key(
    field: &str,
    variable: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<String, ProfileError> {
    env(variable)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| ProfileError::KeyUnset {
            field: field.to_owned(),
            variable: variable.to_owned(),
        })
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

fn local_tools(names: &[LocalTool]) -> Result<Vec<LocalTool>, ProfileError> {
    let mut tools = Vec::with_capacity(names.len());
    for tool in names {
        if tools.contains(tool) {
            return Err(ProfileError::Invalid {
                field: "tools.local",
                problem: "names a tool twice",
            });
        }
        tools.push(*tool);
    }
    // Offered in a fixed order, so the order written doesn't change the
    // prompt.
    tools.sort();
    Ok(tools)
}

fn shell_settings(file: FileTools, offered: bool) -> Result<Option<ShellSettings>, ProfileError> {
    if !offered {
        let set = [
            ("tools.shell_allow", file.shell_allow.is_some()),
            ("tools.sandbox", file.sandbox.is_some()),
            ("tools.sandbox_read", file.sandbox_read.is_some()),
            ("tools.shell_timeout_s", file.shell_timeout_s.is_some()),
            ("tools.shell_env", file.shell_env.is_some()),
        ];
        if let Some((field, _)) = set.iter().find(|(_, set)| *set) {
            return Err(ProfileError::Invalid {
                field,
                problem: "is set, but `shell` isn't in tools.local",
            });
        }
        return Ok(None);
    }

    let entries = file.shell_allow.unwrap_or_default();
    if entries.is_empty() {
        return Err(ProfileError::Invalid {
            field: "tools.shell_allow",
            problem: "must name at least one command when `shell` is offered",
        });
    }
    let mut allow = Vec::with_capacity(entries.len());
    for entry in &entries {
        let words = shell::words(entry).map_err(|error| ProfileError::Invalid {
            field: "tools.shell_allow",
            problem: match error {
                WordsError::ShellCharacter(_) => {
                    "has an entry holding a character only a shell reads (| & ; < > ` $ * ? [ or a line break)"
                }
                WordsError::Unsplittable => "has an entry whose quotes don't close",
                WordsError::Empty => "has an empty entry",
            },
        })?;
        allow.push(words);
    }

    let sandbox = file
        .sandbox
        .or_else(Sandbox::native)
        .ok_or(ProfileError::Invalid {
            field: "tools.sandbox",
            problem: "has no default on this system; write `none` to run with word checks only",
        })?;
    let sandbox_read: Vec<std::path::PathBuf> = file
        .sandbox_read
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect();
    if sandbox == Sandbox::None && !sandbox_read.is_empty() {
        return Err(ProfileError::Invalid {
            field: "tools.sandbox_read",
            problem: "is set, but tools.sandbox is `none`",
        });
    }
    if sandbox_read.iter().any(|path| {
        !path.is_absolute()
            || path
                .components()
                .any(|component| component == std::path::Component::ParentDir)
    }) {
        return Err(ProfileError::Invalid {
            field: "tools.sandbox_read",
            problem: "must hold absolute paths without `..`",
        });
    }

    let timeout_s = file.shell_timeout_s.unwrap_or(SHELL_TIMEOUT_S_DEFAULT);
    if timeout_s == 0 {
        return Err(ProfileError::Invalid {
            field: "tools.shell_timeout_s",
            problem: "must be positive",
        });
    }
    let env: Vec<(String, String)> = file.shell_env.unwrap_or_default().into_iter().collect();
    if env.iter().any(|(name, _)| !is_variable_name(name)) {
        return Err(ProfileError::Invalid {
            field: "tools.shell_env",
            problem: "names a variable other than letters, digits and _, not starting with a digit",
        });
    }
    if env.iter().any(|(_, value)| value.contains('\0')) {
        return Err(ProfileError::Invalid {
            field: "tools.shell_env",
            problem: "has a value holding a NUL, which no environment can carry",
        });
    }
    Ok(Some(ShellSettings {
        allow,
        sandbox,
        sandbox_read,
        timeout_s,
        env,
    }))
}

/// A name every shell and `env` take: no `=`, no NUL, nothing a program
/// might read differently.
fn is_variable_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn mcp_servers(file: BTreeMap<String, FileMcp>) -> Result<Vec<McpSettings>, ProfileError> {
    let mut servers = Vec::with_capacity(file.len());
    // A BTreeMap: the servers come ordered by name.
    for (name, server) in file {
        let invalid = |field, problem| ProfileError::InvalidServer {
            server: name.clone(),
            field,
            problem,
        };
        if name.is_empty()
            || name.len() > SERVER_NAME_CAP_BYTES
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(invalid(
                None,
                "needs a name of 1 to 16 characters from a-z, 0-9 and -",
            ));
        }
        let scheme_ok = reqwest::Url::parse(&server.url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
        if !scheme_ok {
            return Err(invalid(Some("url"), "must be an http or https URL"));
        }
        if server.tools.is_empty() {
            return Err(invalid(Some("tools"), "must name at least one tool"));
        }
        for (index, tool) in server.tools.iter().enumerate() {
            if server.tools[..index].contains(tool) {
                return Err(invalid(Some("tools"), "names a tool twice"));
            }
            let offered = McpSettings::offered_name(&name, tool);
            if offered.len() > TOOL_NAME_CAP_BYTES
                || !offered
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            {
                return Err(invalid(
                    Some("tools"),
                    "names a tool whose offered name `<server>_<tool>` a provider can't take: \
                     over 64 characters, or holding other than letters, digits, _ and -",
                ));
            }
        }
        let key = match (server.key_env, server.key_header) {
            (None, Some(_)) => {
                return Err(invalid(Some("key_header"), "is set, but key_env isn't"));
            }
            (None, None) => None,
            (Some(env), _) if env.is_empty() => {
                return Err(invalid(
                    Some("key_env"),
                    "must name a variable; leave it out to send no key",
                ));
            }
            (Some(env), header) => {
                let header = header
                    .unwrap_or_else(|| "authorization".to_owned())
                    .to_ascii_lowercase();
                if reqwest::header::HeaderName::from_bytes(header.as_bytes()).is_err() {
                    return Err(invalid(Some("key_header"), "must be an HTTP header name"));
                }
                if RESERVED_HEADERS.contains(&header.as_str()) {
                    return Err(invalid(
                        Some("key_header"),
                        "names a header the transport sets itself",
                    ));
                }
                Some(KeySettings { env, header })
            }
        };
        let call_timeout_s = server.call_timeout_s.unwrap_or(CALL_TIMEOUT_S_DEFAULT);
        if call_timeout_s == 0 {
            return Err(invalid(Some("call_timeout_s"), "must be positive"));
        }
        servers.push(McpSettings {
            name,
            url: server.url,
            tools: server.tools,
            key,
            call_timeout_s,
        });
    }
    Ok(servers)
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
    #[serde(default)]
    tools: FileTools,
    #[serde(default)]
    mcp: BTreeMap<String, FileMcp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMcp {
    url: String,
    tools: Vec<String>,
    key_env: Option<String>,
    key_header: Option<String>,
    call_timeout_s: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTools {
    #[serde(default)]
    local: Vec<LocalTool>,
    shell_allow: Option<Vec<String>>,
    sandbox: Option<Sandbox>,
    sandbox_read: Option<Vec<String>>,
    shell_timeout_s: Option<u32>,
    /// A BTreeMap: the variables come ordered by name.
    shell_env: Option<BTreeMap<String, String>>,
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
