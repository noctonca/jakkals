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
            mcp: Vec::new(),
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

const NOTES: &str = "[mcp.notes]\nurl = \"https://notes.example/mcp\"\ntools = [\"search\"]\n";

#[test]
fn an_mcp_server_takes_the_documented_defaults() {
    let profile = Profile::parse(&with_limits(NOTES)).expect("the profile is valid");
    assert_eq!(
        profile.mcp,
        [McpSettings {
            name: "notes".to_owned(),
            url: "https://notes.example/mcp".to_owned(),
            tools: vec!["search".to_owned()],
            key: None,
            call_timeout_s: 60,
        }]
    );
}

#[test]
fn mcp_servers_come_ordered_by_name_with_their_tools_as_written() {
    let text = with_limits(
        "[mcp.zebra]\nurl = \"http://z.example/\"\ntools = [\"b\", \"a\"]\n\
         [mcp.apple]\nurl = \"http://a.example/\"\ntools = [\"x\"]\n",
    );
    let profile = Profile::parse(&text).expect("the profile is valid");
    let names: Vec<&str> = profile
        .mcp
        .iter()
        .map(|server| server.name.as_str())
        .collect();
    assert_eq!(names, ["apple", "zebra"]);
    assert_eq!(profile.mcp[1].tools, ["b", "a"]);
}

#[test]
fn an_mcp_key_goes_in_authorization_unless_a_header_is_named() {
    let text = with_limits(&format!("{NOTES}key_env = \"NOTES_KEY\"\n"));
    let profile = Profile::parse(&text).expect("the profile is valid");
    assert_eq!(
        profile.mcp[0].key,
        Some(KeySettings {
            env: "NOTES_KEY".to_owned(),
            header: "authorization".to_owned()
        })
    );
    let text = with_limits(&format!(
        "{NOTES}key_env = \"NOTES_KEY\"\nkey_header = \"X-Api-Key\"\n"
    ));
    let profile = Profile::parse(&text).expect("the profile is valid");
    assert_eq!(
        profile.mcp[0].key.as_ref().map(|key| key.header.as_str()),
        Some("x-api-key")
    );
}

#[test]
fn an_mcp_key_is_read_from_its_variable() {
    let text = with_limits(&format!("{NOTES}key_env = \"NOTES_KEY\"\n"));
    let profile = Profile::parse(&text).expect("the profile is valid");
    let server = &profile.mcp[0];
    let key = mcp_key(server, |name| {
        assert_eq!(name, "NOTES_KEY");
        Some("secret".to_owned())
    });
    assert_eq!(key, Ok(Some("secret".to_owned())));
    for unset in [None, Some(String::new())] {
        assert_eq!(
            mcp_key(server, |_| unset.clone()),
            Err(ProfileError::KeyUnset {
                field: "mcp.notes.key_env".to_owned(),
                variable: "NOTES_KEY".to_owned()
            })
        );
    }
    let without = Profile::parse(&with_limits(NOTES)).expect("the profile is valid");
    assert_eq!(
        mcp_key(&without.mcp[0], |name| panic!("read {name}")),
        Ok(None)
    );
}

#[test]
fn bad_mcp_servers_are_refused_naming_the_field() {
    let server = |name: &str, extra: &str| {
        with_limits(&format!(
            "[mcp.{name}]\nurl = \"https://notes.example/mcp\"\n{extra}"
        ))
    };
    let tools = "tools = [\"search\"]\n";
    let cases: [(String, Option<&str>); 13] = [
        (server("Notes", tools), None),
        (server("\"\"", tools), None),
        (server("a-very-long-server-name", tools), None),
        (server("notes_2", tools), None),
        (
            with_limits("[mcp.notes]\nurl = \"ftp://notes.example/\"\ntools = [\"search\"]"),
            Some("url"),
        ),
        (server("notes", "tools = []\n"), Some("tools")),
        (server("notes", "tools = [\"a\", \"a\"]\n"), Some("tools")),
        (server("notes", "tools = [\"read.note\"]\n"), Some("tools")),
        (
            server("notes", &format!("tools = [\"{}\"]\n", "t".repeat(60))),
            Some("tools"),
        ),
        (
            server("notes", &format!("{tools}key_header = \"x-key\"\n")),
            Some("key_header"),
        ),
        (
            server("notes", &format!("{tools}key_env = \"\"\n")),
            Some("key_env"),
        ),
        (
            server(
                "notes",
                &format!("{tools}key_env = \"K\"\nkey_header = \"mcp-session-id\"\n"),
            ),
            Some("key_header"),
        ),
        (
            server("notes", &format!("{tools}call_timeout_s = 0\n")),
            Some("call_timeout_s"),
        ),
    ];
    for (text, field) in cases {
        match invalid(&text) {
            ProfileError::InvalidServer { field: found, .. } => {
                assert_eq!(found, field, "{text:?}");
            }
            other => panic!("{text:?}: {other:?}"),
        }
    }
}

#[test]
fn an_mcp_server_needs_its_url_and_tools_and_no_unknown_field() {
    for extra in [
        "[mcp.notes]\ntools = [\"search\"]",
        "[mcp.notes]\nurl = \"https://notes.example/mcp\"",
        "[mcp.notes]\nurl = \"https://notes.example/mcp\"\ntools = [\"search\"]\ndeny = []",
    ] {
        assert!(
            matches!(invalid(&with_limits(extra)), ProfileError::Toml { .. }),
            "{extra:?}"
        );
    }
}
