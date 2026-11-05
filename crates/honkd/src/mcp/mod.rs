pub mod catalog;
pub mod ops;
pub mod probe;
pub mod project;
pub mod redact;
pub mod wrapper;

use crate::mcp::catalog::Catalog;
use crate::mcp::ops::{OpError, Policy};
use crate::mcp::project::ProjectConfig;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmMcpConfig {
    pub bin: PathBuf,
    pub pigeon_url: String,
    pub pigeon_token_file: Option<PathBuf>,
    pub honk_url: String,
    pub honk_secret_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub catalog_path: Option<PathBuf>,
    pub policy: Policy,
    pub swarm: Option<SwarmMcpConfig>,
    /// Read the session cwd's `opencode.json[c]` MCP servers (`--project-mcp`).
    pub project: bool,
}

/// The catalog one session sees, and what went into it.
#[derive(Debug, Clone, Default)]
pub struct SessionCatalog {
    /// User catalog, plus the project overlay, plus `swarm`.
    pub catalog: Catalog,
    /// What the project's config files declared; `None` when project MCP is
    /// off or the session has no usable cwd.
    pub project: Option<ProjectConfig>,
    /// Project entries hidden because the user catalog has the same name.
    pub shadowed: Vec<String>,
}

impl McpConfig {
    /// Builds the catalog for a session whose cwd is `cwd`.
    ///
    /// The user catalog (empty without `--mcp-catalog`) comes first. With
    /// project MCP on and an absolute `cwd`, the project's entries are added
    /// beneath it (a user entry of the same name wins), then `swarm` is merged
    /// over both. Project discovery reads files, so it runs off the async
    /// threads. Only an unreadable user catalog is an error; project files
    /// that cannot be used are reported in [`ProjectConfig::problems`].
    pub async fn session_catalog(
        &self,
        cwd: Option<&str>,
        session_id: &str,
    ) -> Result<SessionCatalog, OpError> {
        let user = match &self.catalog_path {
            Some(path) => Catalog::load(path).map_err(OpError::catalog_unreadable)?,
            None => Catalog::default(),
        };

        let mut project = None;
        let mut shadowed = Vec::new();
        let mut catalog = user;
        if self.project
            && let Some(cwd) = cwd.filter(|c| Path::new(c).is_absolute())
        {
            let dir = PathBuf::from(cwd);
            let found = match tokio::task::spawn_blocking(move || project::discover(&dir)).await {
                Ok(found) => found,
                Err(e) => ProjectConfig {
                    problems: vec![format!("{cwd}: project discovery failed: {e}")],
                    ..Default::default()
                },
            };
            let eff = project::effective_catalog(catalog, &found);
            catalog = eff.catalog;
            shadowed = eff.shadowed;
            project = Some(found);
        }

        if let Some(swarm) = &self.swarm {
            catalog.merge_swarm_entry(swarm, session_id);
        }
        Ok(SessionCatalog {
            catalog,
            project,
            shadowed,
        })
    }
}

/// What `session/new` does about a project's MCP servers, and what it tells
/// the client about the ones it leaves alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectLaunchPlan {
    /// Entries to enable now: `(name, source label)`.
    pub eligible: Vec<(String, String)>,
    /// Project entries left alone, as `(name, reason)`.
    pub skipped: Vec<(String, String)>,
    /// Config files that could not be used.
    pub problems: Vec<String>,
}

impl SessionCatalog {
    /// Sorts the project's entries into those to enable at once (trusted, see
    /// [`catalog::CatalogEntry::auto_eligible`]) and those to leave to a person.
    /// `None` when the session has no project config worth mentioning: no
    /// project lookup, no entries and no problems.
    pub fn project_launch_plan(&self) -> Option<ProjectLaunchPlan> {
        let project = self.project.as_ref()?;
        if project.entries.is_empty() && project.problems.is_empty() {
            return None;
        }
        let mut plan = ProjectLaunchPlan {
            problems: project.problems.clone(),
            ..Default::default()
        };
        for entry in &project.entries {
            let name = entry.name.clone();
            if self.shadowed.contains(&name) {
                plan.skipped
                    .push((name, "shadowed by user catalog".to_string()));
            } else if !entry.enabled {
                plan.skipped
                    .push((name, "disabled in project config".to_string()));
            } else if let Err(reason) = &entry.supported {
                plan.skipped.push((name, reason.clone()));
            } else if entry.auto_eligible() {
                plan.eligible.push((name, entry.source.label()));
            } else {
                let reason = format!("not a loopback url; enable with /mcp enable {name}!");
                plan.skipped.push((name, reason));
            }
        }
        Some(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::catalog::EntrySource;

    #[tokio::test]
    async fn launch_plan_sorts_entries_by_trust() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir(td.path().join(".git")).unwrap();
        std::fs::write(
            td.path().join("opencode.json"),
            r#"{"mcp":{
                "tec":{"type":"remote","url":"http://localhost:4006/mcp"},
                "far":{"type":"remote","url":"https://example.com/mcp"},
                "off":{"type":"remote","url":"http://127.0.0.1:1/mcp","enabled":false},
                "authed":{"type":"remote","url":"http://127.0.0.1:2/mcp","headers":{"A":"b"}},
                "cmd":{"type":"local","command":["x"]},
                "mine":{"type":"remote","url":"http://127.0.0.1:3/mcp"}
            }}"#,
        )
        .unwrap();
        let user = td.path().join("user.json");
        std::fs::write(
            &user,
            r#"{"mcp":{"mine":{"type":"local","command":["y"]}}}"#,
        )
        .unwrap();
        let mut cfg = McpConfig {
            catalog_path: Some(user),
            policy: Policy::default(),
            swarm: None,
            project: true,
        };
        let sc = cfg
            .session_catalog(Some(td.path().to_str().unwrap()), "s")
            .await
            .unwrap();
        let plan = sc.project_launch_plan().unwrap();

        assert_eq!(plan.eligible.len(), 1);
        assert_eq!(plan.eligible[0].0, "tec");
        assert!(plan.eligible[0].1.starts_with("project:"));
        let reason = |n: &str| {
            plan.skipped
                .iter()
                .find(|(name, _)| name == n)
                .map(|(_, r)| r.as_str())
                .unwrap_or_else(|| panic!("{n} not skipped: {:?}", plan.skipped))
        };
        assert_eq!(
            reason("far"),
            "not a loopback url; enable with /mcp enable far!"
        );
        assert_eq!(reason("off"), "disabled in project config");
        assert!(reason("authed").contains("headers"), "{}", reason("authed"));
        assert!(reason("cmd").contains("stdio"), "{}", reason("cmd"));
        assert_eq!(reason("mine"), "shadowed by user catalog");
        assert!(plan.problems.is_empty());

        // No project entries and no problems: nothing to say.
        let empty = tempfile::tempdir().unwrap();
        cfg.catalog_path = None;
        let sc = cfg
            .session_catalog(Some(empty.path().to_str().unwrap()), "s")
            .await
            .unwrap();
        assert_eq!(sc.project_launch_plan(), None);

        // A bad file alone is worth a mention.
        std::fs::write(empty.path().join("opencode.json"), "{ nope").unwrap();
        let sc = cfg
            .session_catalog(Some(empty.path().to_str().unwrap()), "s")
            .await
            .unwrap();
        let plan = sc.project_launch_plan().unwrap();
        assert!(plan.eligible.is_empty() && plan.skipped.is_empty());
        assert_eq!(plan.problems.len(), 1);

        // Project MCP off: no plan.
        cfg.project = false;
        let sc = cfg
            .session_catalog(Some(empty.path().to_str().unwrap()), "s")
            .await
            .unwrap();
        assert_eq!(sc.project_launch_plan(), None);
    }

    const TEC: &str = r#"{"mcp":{"tec":{"type":"remote","url":"http://127.0.0.1:4006/mcp"}}}"#;

    fn project_dir() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir(td.path().join(".git")).unwrap();
        std::fs::write(td.path().join("opencode.json"), TEC).unwrap();
        td
    }

    fn cfg(project: bool) -> McpConfig {
        McpConfig {
            catalog_path: None,
            policy: Policy::default(),
            swarm: None,
            project,
        }
    }

    #[tokio::test]
    async fn project_entries_need_the_flag_and_a_cwd() {
        let td = project_dir();
        let cwd = td.path().to_str().unwrap();

        let off = cfg(false).session_catalog(Some(cwd), "s").await.unwrap();
        assert!(off.catalog.entries.is_empty());
        assert!(off.project.is_none());

        let none = cfg(true).session_catalog(None, "s").await.unwrap();
        assert!(none.catalog.entries.is_empty());

        let relative = cfg(true)
            .session_catalog(Some("relative/dir"), "s")
            .await
            .unwrap();
        assert!(relative.project.is_none());

        let on = cfg(true).session_catalog(Some(cwd), "s").await.unwrap();
        let tec = on.catalog.get("tec").expect("tec from the project");
        assert!(matches!(tec.source, EntrySource::Project(_)));
        assert!(tec.auto_eligible());
        assert_eq!(on.project.unwrap().entries.len(), 1);
    }

    #[tokio::test]
    async fn user_catalog_wins_and_swarm_is_merged() {
        let td = project_dir();
        let user = td.path().join("user.json");
        std::fs::write(
            &user,
            r#"{"mcp":{"tec":{"type":"local","command":["/bin/true"]}}}"#,
        )
        .unwrap();
        let mut c = cfg(true);
        c.catalog_path = Some(user);
        c.swarm = Some(SwarmMcpConfig {
            bin: "/bin/sh".into(),
            pigeon_url: "http://127.0.0.1:1".into(),
            pigeon_token_file: None,
            honk_url: "http://127.0.0.1:2/acp".into(),
            honk_secret_file: "/nonexistent/secret".into(),
        });
        let got = c
            .session_catalog(Some(td.path().to_str().unwrap()), "ses_1")
            .await
            .unwrap();
        assert_eq!(got.shadowed, vec!["tec".to_string()]);
        assert_eq!(got.catalog.get("tec").unwrap().source, EntrySource::User);
        assert_eq!(
            got.catalog.get("swarm").unwrap().source,
            EntrySource::Builtin
        );
    }

    #[tokio::test]
    async fn unreadable_user_catalog_is_an_error() {
        let mut c = cfg(true);
        c.catalog_path = Some("/nonexistent/catalog.json".into());
        let err = c.session_catalog(None, "s").await.unwrap_err();
        assert!(err.message.starts_with("MCP catalog unreadable"));
    }
}
