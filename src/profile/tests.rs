//! Profiles in, a validated profile or the named fault out.

use serde_json::json;

use super::*;

const MINIMAL: &str = "[limits]\nsteps = 30\nwall_s = 600\n";

fn invalid(text: &str) -> ProfileError {
    Profile::parse(text).expect_err("the profile is refused")
}

fn with_limits(extra: &str) -> String {
    format!("{MINIMAL}{extra}\n")
}

#[test]
fn minimal_profile_takes_the_documented_defaults() {
    let profile = Profile::parse(MINIMAL).expect("the profile is valid");

    assert_eq!(
        profile,
        Profile {
            system_prompt: String::new(),
            limits: Limits {
                steps: 30,
                wall_s: 600,
                cost_nano_usd: None,
                tokens: None,
                context_tokens: None,
                tool_output_bytes: 32_768,
            },
            provider: ProviderSettings {
                base_url: "https://openrouter.ai/api/v1".to_owned(),
                api_key_env: None,
                params: Map::new(),
            },
            local_tools: Vec::new(),
            shell: None,
            // `shasum -a 256` of the same bytes.
            hash: "sha256:0ecfa70ca44c48c52c8b8636f358d73c955fb22d383c103433c2bdc5488a91cd"
                .to_owned(),
        }
    );
}

#[test]
fn every_field_is_read() {
    let text = r#"
system_prompt = """
You are a test.
Answer briefly."""

[limits]
steps = 12
wall_s = 90
cost_usd = 0.25
tokens = 200_000
context_tokens = 64_000
tool_output_bytes = 4096

[provider]
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "TEST_PROVIDER_KEY"

[provider.params]
temperature = 0.2
max_tokens = 1000
reasoning = { effort = "low" }

[tools]
local = ["search", "shell", "read", "list"]
shell_allow = ["git log", "rg", "'cargo' tree"]
sandbox = "seatbelt"
sandbox_read = ["/opt/homebrew"]
shell_timeout_s = 10
"#;
    let profile = Profile::parse(text).expect("the profile is valid");

    assert_eq!(profile.system_prompt, "You are a test.\nAnswer briefly.");
    assert_eq!(
        profile.limits,
        Limits {
            steps: 12,
            wall_s: 90,
            cost_nano_usd: Some(250_000_000),
            tokens: Some(200_000),
            context_tokens: Some(64_000),
            tool_output_bytes: 4096,
        }
    );
    assert_eq!(
        profile.provider,
        ProviderSettings {
            base_url: "http://127.0.0.1:8080/v1".to_owned(),
            api_key_env: Some("TEST_PROVIDER_KEY".to_owned()),
            params: json!({"temperature": 0.2, "max_tokens": 1000, "reasoning": {"effort": "low"}})
                .as_object()
                .expect("an object")
                .clone(),
        }
    );
    assert_eq!(
        profile.local_tools,
        [
            LocalTool::Read,
            LocalTool::List,
            LocalTool::Search,
            LocalTool::Shell
        ],
        "offered in a fixed order, whatever the order written"
    );
    assert_eq!(
        profile.shell,
        Some(ShellSettings {
            allow: vec![
                vec!["git".to_owned(), "log".to_owned()],
                vec!["rg".to_owned()],
                vec!["cargo".to_owned(), "tree".to_owned()],
            ],
            sandbox: Sandbox::Seatbelt,
            sandbox_read: vec!["/opt/homebrew".into()],
            timeout_s: 10,
        })
    );
}

#[test]
fn cost_limit_is_read_from_its_digits() {
    let cases = [
        ("0.000000001", 1),
        ("0.1", 100_000_000),
        ("2", 2_000_000_000),
        ("+1.5", 1_500_000_000),
        ("1_000.5", 1_000_500_000_000),
        ("5e-3", 5_000_000),
        ("0.30000000000000004", 300_000_000),
    ];
    for (literal, nano) in cases {
        let profile = Profile::parse(&with_limits(&format!("cost_usd = {literal}")))
            .expect("the profile is valid");
        assert_eq!(profile.limits.cost_nano_usd, Some(nano), "{literal}");
    }
}

#[test]
fn toml_faults_are_refused_with_their_position() {
    let cases = [
        ("[limits]\nwall_s = 600\n", "missing field `steps`"),
        ("[limits]\nsteps = 30\n", "missing field `wall_s`"),
        ("system_prompt = \"\"\n", "missing field `limits`"),
        (&with_limits("step = 3"), "unknown field `step`"),
        (
            &format!("{MINIMAL}[provider]\nkey = \"x\"\n"),
            "unknown field `key`",
        ),
        (&format!("{MINIMAL}models = []\n"), "unknown field `models`"),
        (
            &format!("{MINIMAL}[tools]\nlocal = [\"write\"]\n"),
            "unknown variant `write`",
        ),
        (
            &format!("{MINIMAL}[tools]\nremote = []\n"),
            "unknown field `remote`",
        ),
        ("[limits]\nsteps = -1\nwall_s = 600\n", "steps"),
        ("[limits]\nsteps = 30\nwall_s = \"600\"\n", "wall_s"),
        (&with_limits("cost_usd = \"0.5\""), "cost_usd"),
        ("[limits\n", "line 1"),
    ];
    for (text, expected) in cases {
        let error = invalid(text);
        let ProfileError::Toml { detail } = &error else {
            panic!("{text:?}: a TOML fault, not {error:?}");
        };
        assert!(
            detail.contains(expected),
            "{text:?}: {detail:?} names {expected:?}"
        );
    }
}

#[test]
fn values_the_harness_cant_run_with_are_refused() {
    let cases = [
        ("[limits]\nsteps = 0\nwall_s = 600\n", "limits.steps"),
        ("[limits]\nsteps = 30\nwall_s = 0\n", "limits.wall_s"),
        (&with_limits("tokens = 0"), "limits.tokens"),
        (&with_limits("context_tokens = 0"), "limits.context_tokens"),
        (
            &with_limits("tool_output_bytes = 0"),
            "limits.tool_output_bytes",
        ),
        (&with_limits("cost_usd = 0"), "limits.cost_usd"),
        (&with_limits("cost_usd = 0.0000000001"), "limits.cost_usd"),
        (&with_limits("cost_usd = -0.5"), "limits.cost_usd"),
        (&with_limits("cost_usd = inf"), "limits.cost_usd"),
        (&with_limits("cost_usd = nan"), "limits.cost_usd"),
        (&with_limits("cost_usd = 1e30"), "limits.cost_usd"),
        (
            &format!("{MINIMAL}[provider]\nbase_url = \"openrouter.ai\"\n"),
            "provider.base_url",
        ),
        (
            &format!("{MINIMAL}[provider]\napi_key_env = \"\"\n"),
            "provider.api_key_env",
        ),
        (
            &format!("{MINIMAL}[provider.params]\nstream = true\n"),
            "provider.params",
        ),
        (
            &format!("{MINIMAL}[provider.params]\nmodel = \"other/model\"\n"),
            "provider.params",
        ),
        (
            &format!("{MINIMAL}[tools]\nlocal = [\"read\", \"list\", \"read\"]\n"),
            "tools.local",
        ),
    ];
    for (text, field) in cases {
        let error = invalid(text);
        assert!(
            matches!(error, ProfileError::Invalid { field: named, .. } if named == field),
            "{text:?}: invalid {field}, not {error:?}"
        );
    }
}

#[test]
fn capabilities_not_built_are_refused_not_ignored() {
    let text = format!("{MINIMAL}[mcp.memory]\nurl = \"https://example.com/mcp\"\n");
    assert_eq!(invalid(&text), ProfileError::NotBuilt { field: "mcp" });
}

#[test]
fn the_shell_takes_the_documented_defaults() {
    let text = format!("{MINIMAL}[tools]\nlocal = [\"shell\"]\nshell_allow = [\"git log\"]\n");
    let profile = Profile::parse(&text).expect("the profile is valid");
    assert_eq!(
        profile.shell,
        Some(ShellSettings {
            allow: vec![vec!["git".to_owned(), "log".to_owned()]],
            // The system's own: seatbelt on macOS, landlock on Linux.
            sandbox: Sandbox::native().expect("tests run where Jakkals has a sandbox"),
            sandbox_read: Vec::new(),
            timeout_s: 30,
        })
    );
}

#[test]
fn shell_settings_the_harness_cant_run_with_are_refused() {
    let shell = |extra: &str| {
        format!("{MINIMAL}[tools]\nlocal = [\"shell\"]\nshell_allow = [\"git log\"]\n{extra}\n")
    };
    let without_shell = |extra: &str| format!("{MINIMAL}[tools]\nlocal = [\"read\"]\n{extra}\n");
    let cases = [
        (
            format!("{MINIMAL}[tools]\nlocal = [\"shell\"]\n"),
            "tools.shell_allow",
        ),
        (
            format!("{MINIMAL}[tools]\nlocal = [\"shell\"]\nshell_allow = []\n"),
            "tools.shell_allow",
        ),
        (
            format!("{MINIMAL}[tools]\nlocal = [\"shell\"]\nshell_allow = [\"git log | head\"]\n"),
            "tools.shell_allow",
        ),
        (
            format!("{MINIMAL}[tools]\nlocal = [\"shell\"]\nshell_allow = [\"  \"]\n"),
            "tools.shell_allow",
        ),
        (
            format!("{MINIMAL}[tools]\nlocal = [\"shell\"]\nshell_allow = [\"git 'log\"]\n"),
            "tools.shell_allow",
        ),
        (shell("shell_timeout_s = 0"), "tools.shell_timeout_s"),
        (
            shell("sandbox_read = [\"opt/homebrew\"]"),
            "tools.sandbox_read",
        ),
        (
            shell("sandbox_read = [\"/opt/../etc\"]"),
            "tools.sandbox_read",
        ),
        (
            shell("sandbox = \"none\"\nsandbox_read = [\"/opt/homebrew\"]"),
            "tools.sandbox_read",
        ),
        (
            without_shell("shell_allow = [\"git log\"]"),
            "tools.shell_allow",
        ),
        (without_shell("sandbox = \"none\""), "tools.sandbox"),
        (
            without_shell("sandbox_read = [\"/opt\"]"),
            "tools.sandbox_read",
        ),
        (
            without_shell("shell_timeout_s = 5"),
            "tools.shell_timeout_s",
        ),
    ];
    for (text, field) in cases {
        let error = invalid(&text);
        assert!(
            matches!(error, ProfileError::Invalid { field: named, .. } if named == field),
            "{text:?}: invalid {field}, not {error:?}"
        );
    }
    let error = invalid(&shell("sandbox = \"container\""));
    assert!(
        matches!(&error, ProfileError::Toml { detail } if detail.contains("unknown variant `container`")),
        "{error:?}"
    );
}

#[test]
fn key_comes_from_the_named_variable() {
    let text = format!("{MINIMAL}[provider]\napi_key_env = \"TEST_PROVIDER_KEY\"\n");
    let profile = Profile::parse(&text).expect("the profile is valid");
    let env_with = |value: Option<&'static str>| {
        move |name: &str| {
            assert_eq!(name, "TEST_PROVIDER_KEY");
            value.map(str::to_owned)
        }
    };

    let config = profile
        .http_config(env_with(Some("test-key")))
        .expect("the key is set");
    assert_eq!(config.api_key.as_deref(), Some("test-key"));
    assert_eq!(config.base_url, OPENROUTER_BASE_URL);
    for unset in [None, Some("")] {
        assert_eq!(
            profile.http_config(env_with(unset)).err(),
            Some(ProfileError::KeyUnset {
                variable: "TEST_PROVIDER_KEY".to_owned()
            })
        );
    }
}

#[test]
fn no_key_variable_sends_no_key_and_reads_no_environment() {
    let profile = Profile::parse(MINIMAL).expect("the profile is valid");
    let config = profile
        .http_config(|name| panic!("read {name} from the environment"))
        .expect("no key is needed");
    assert_eq!(config.api_key, None);
}

/// A file in the system's temporary directory, removed when dropped.
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!("jakkals-test-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("temp file is writable");
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn read_takes_a_file_and_refuses_what_isnt_a_profile() {
    let file = TempFile::new("minimal.toml", MINIMAL.as_bytes());
    assert_eq!(
        Profile::read(&file.0),
        Profile::parse(MINIMAL),
        "reading a file is parsing its text"
    );

    let not_utf8 = TempFile::new("latin1.toml", b"system_prompt = \"caf\xe9\"\n");
    let huge = TempFile::new(
        "huge.toml",
        &vec![b'#'; usize::try_from(PROFILE_CAP_BYTES).expect("fits usize") + 1],
    );
    let missing = std::env::temp_dir().join("jakkals-test-no-such-profile.toml");
    for path in [&not_utf8.0, &huge.0, &missing] {
        let error = Profile::read(path).expect_err("the file is refused");
        assert!(
            matches!(error, ProfileError::Read { .. }),
            "{}: a read fault, not {error:?}",
            path.display()
        );
    }
}
