use super::*;
use crate::config::ConfigBuilder;
use crate::config::ConfigOverrides;
use codex_config::loader::project_trust_key;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::TempDir;

const EXPERT: &str = "experts/pricing";

struct Fixture {
    _tmp: TempDir,
    codex_home: PathBuf,
    root: AbsolutePathBuf,
}

impl Fixture {
    /// A trusted git project whose `experts/pricing` subdirectory has its own `.codex` config.
    fn new(user_config: &str, expert_config: &str, trusted: bool) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let base = dunce::canonicalize(tmp.path()).expect("canonical tempdir");
        let codex_home = base.join("home");
        let root = base.join("project");
        let expert = root.join(EXPERT);
        fs::create_dir_all(&codex_home).expect("codex home");
        fs::create_dir_all(root.join(".git")).expect("git marker");
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("git HEAD");
        fs::create_dir_all(root.join(".codex")).expect("root .codex");
        fs::create_dir_all(expert.join(".codex")).expect("expert .codex");
        let trust = if trusted {
            format!(
                "\n[projects.{key:?}]\ntrust_level = \"trusted\"\n",
                key = project_trust_key(&root)
            )
        } else {
            String::new()
        };
        fs::write(
            codex_home.join("config.toml"),
            format!("{user_config}\n{trust}"),
        )
        .expect("user config");
        fs::write(
            root.join(".codex/config.toml"),
            "model_verbosity = \"low\"\n",
        )
        .expect("root project config");
        fs::write(expert.join(".codex/config.toml"), expert_config).expect("expert config");
        fs::write(expert.join("AGENTS.md"), "Pricing expert instructions.\n").expect("agents");
        Self {
            _tmp: tmp,
            codex_home,
            root: AbsolutePathBuf::from_absolute_path(&root).expect("absolute root"),
        }
    }

    async fn parent_config(&self) -> Config {
        ConfigBuilder::without_managed_config_for_tests()
            .codex_home(self.codex_home.clone())
            .harness_overrides(ConfigOverrides {
                cwd: Some(self.root.to_path_buf()),
                ..Default::default()
            })
            .build()
            .await
            .expect("parent config")
    }

    /// Runs the spawn sequence: directory settings, the parent's runtime policy, then clamping.
    async fn child_config(&self, parent: &Config) -> (Config, Vec<String>) {
        let mut directory_config =
            load_child_directory_config(parent, &self.root, Some(Path::new(EXPERT)))
                .await
                .expect("child directory config")
                .expect("child directory applies");
        let mut child = parent.clone();
        directory_config
            .apply_settings(&mut child)
            .expect("directory settings");
        // `apply_spawn_agent_runtime_overrides` restores the parent's live policy and cwd.
        child
            .permissions
            .approval_policy
            .set(parent.permissions.approval_policy.value())
            .expect("parent approval");
        child
            .permissions
            .set_permission_profile_from_session_snapshot(
                parent.permissions.permission_profile_state().snapshot(),
            )
            .expect("parent permissions");
        child.cwd = parent.cwd.clone();
        directory_config
            .apply_runtime(&mut child)
            .expect("directory runtime");
        (child, directory_config.into_warnings())
    }
}

const EXPERT_CONFIG: &str = r#"
model = "gpt-expert"
developer_instructions = "You are the pricing expert."

[mcp_servers.pricing]
command = "pricing-mcp"

[[hooks.PreToolUse]]
matcher = "^Bash$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "python3 pricing-hook.py"
"#;

#[tokio::test]
async fn child_directory_loads_subdirectory_config_layers() {
    let fixture = Fixture::new("model = \"gpt-parent\"\n", EXPERT_CONFIG, true);
    let parent = fixture.parent_config().await;
    let (child, warnings) = fixture.child_config(&parent).await;

    let expert = fixture.root.join(EXPERT);
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(child.cwd, expert);
    assert_eq!(child.model.as_deref(), Some("gpt-expert"));
    assert_eq!(
        child.developer_instructions.as_deref(),
        Some("You are the pricing expert.")
    );
    assert!(child.mcp_servers.get().contains_key("pricing"));
    assert!(!parent.mcp_servers.get().contains_key("pricing"));
    let expert_layer = child
        .config_layer_stack
        .layers_low_to_high()
        .find(|layer| {
            layer.name
                == ConfigLayerSource::Project {
                    dot_codex_folder: expert.join(".codex"),
                }
        })
        .expect("expert project layer should be enabled");
    assert!(expert_layer.config.get("hooks").is_some());
}

#[tokio::test]
async fn child_directory_values_override_and_unset_values_inherit() {
    let fixture = Fixture::new(
        "model = \"gpt-parent\"\npersonality = \"none\"\n",
        "model = \"gpt-expert\"\n",
        true,
    );
    let parent = fixture.parent_config().await;
    let (child, _) = fixture.child_config(&parent).await;

    assert_eq!(parent.model.as_deref(), Some("gpt-parent"));
    assert_eq!(child.model.as_deref(), Some("gpt-expert"));
    // Set only by the user config and the shared project root, so both are inherited.
    assert_eq!(child.personality, parent.personality);
    assert_eq!(child.model_verbosity, parent.model_verbosity);
    assert_eq!(
        child.model_verbosity,
        Some(codex_protocol::config_types::Verbosity::Low)
    );
}

#[tokio::test]
async fn child_directory_cannot_loosen_parent_permissions() {
    let fixture = Fixture::new(
        "approval_policy = \"on-request\"\nsandbox_mode = \"read-only\"\n",
        "approval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n",
        true,
    );
    let parent = fixture.parent_config().await;
    let (child, warnings) = fixture.child_config(&parent).await;

    assert_eq!(
        child.permissions.approval_policy.value(),
        AskForApproval::OnRequest
    );
    assert_eq!(
        child.permissions.effective_permission_profile(),
        parent.permissions.effective_permission_profile()
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("approval_policy")),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn child_directory_can_tighten_parent_permissions() {
    let fixture = Fixture::new(
        "approval_policy = \"never\"\nsandbox_mode = \"workspace-write\"\n",
        "approval_policy = \"on-request\"\nsandbox_mode = \"read-only\"\n",
        true,
    );
    let parent = fixture.parent_config().await;
    let (child, warnings) = fixture.child_config(&parent).await;

    let expert = fixture.root.join(EXPERT);
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(
        child.permissions.approval_policy.value(),
        AskForApproval::OnRequest
    );
    assert!(
        parent
            .permissions
            .file_system_sandbox_policy()
            .can_write_local_path_with_cwd(expert.as_path(), expert.as_path())
    );
    assert!(
        !child
            .permissions
            .file_system_sandbox_policy()
            .can_write_local_path_with_cwd(expert.as_path(), expert.as_path())
    );
}

#[tokio::test]
async fn untrusted_child_directory_layers_stay_disabled_and_warn() {
    let fixture = Fixture::new("model = \"gpt-parent\"\n", EXPERT_CONFIG, false);
    let parent = fixture.parent_config().await;
    let (child, warnings) = fixture.child_config(&parent).await;

    assert_eq!(child.cwd, fixture.root.join(EXPERT));
    assert_eq!(child.model.as_deref(), Some("gpt-parent"));
    assert!(!child.mcp_servers.get().contains_key("pricing"));
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("was not loaded")),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn child_directory_rejects_invalid_cwd() {
    let fixture = Fixture::new("", "", true);
    fs::write(fixture.root.join("README.md").as_path(), "readme").expect("file");
    let cases = [
        ("experts/missing", "cannot be used"),
        ("README.md", "is not a directory"),
        ("../outside", "must be a relative path inside"),
        ("/etc", "must be a relative path inside"),
    ];
    for (requested, expected) in cases {
        let err = ChildDirectory::resolve(&fixture.root, Path::new(requested))
            .expect_err("invalid cwd should be rejected");
        assert!(err.contains(expected), "{requested}: {err}");
    }
    #[cfg(unix)]
    {
        let outside = fixture
            .root
            .as_path()
            .parent()
            .expect("parent")
            .join("home");
        std::os::unix::fs::symlink(&outside, fixture.root.join("escape").as_path())
            .expect("symlink");
        let err = ChildDirectory::resolve(&fixture.root, Path::new("escape"))
            .expect_err("symlink escape should be rejected");
        assert!(err.contains("resolves outside"), "{err}");
    }
}

#[tokio::test]
async fn omitted_or_parent_cwd_keeps_todays_child_config() {
    let fixture = Fixture::new("", EXPERT_CONFIG, true);
    let parent = fixture.parent_config().await;
    for requested in [None, Some(Path::new(".")), Some(Path::new("./"))] {
        let directory_config = load_child_directory_config(&parent, &fixture.root, requested)
            .await
            .expect("no directory should be valid");
        assert!(directory_config.is_none(), "{requested:?}");
    }
}

#[test]
fn rebases_only_environments_at_the_parent_cwd() {
    let tmp = TempDir::new().expect("tempdir");
    let parent_cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute");
    let directory = ChildDirectory {
        relative: PathBuf::from(EXPERT),
        cwd: parent_cwd.join(EXPERT),
        parent_cwd: parent_cwd.clone(),
    };
    assert_eq!(
        directory.rebase_environment_cwd(&PathUri::from_abs_path(&parent_cwd)),
        Some(PathUri::from_abs_path(&parent_cwd.join(EXPERT)))
    );
    assert_eq!(
        directory.rebase_environment_cwd(&PathUri::from_abs_path(&parent_cwd.join("other"))),
        None
    );
}
