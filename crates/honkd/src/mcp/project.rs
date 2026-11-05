//! Project-scoped MCP servers, read from the session's `opencode.json[c]` files.
//!
//! Discovery mirrors opencode closely enough that one config serves both
//! harnesses, and is stricter where opencode is loose. See [`discover`].

use crate::mcp::catalog::{
    Catalog, CatalogEntry, EntrySource, GooseExtensionSpec, Transport, parse_entry,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The MCP entries a project declares, plus what was read to get them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    /// The directory holding `.git`, when the walk found one.
    pub root: Option<PathBuf>,
    /// Config files that were read and parsed, in application order.
    pub files: Vec<PathBuf>,
    /// Entries from the merged `mcp` objects, sorted by name.
    pub entries: Vec<CatalogEntry>,
    /// Files or paths that could not be used, as `path: reason`. Never fatal.
    pub problems: Vec<String>,
}

/// A catalog with the project overlay applied, and what the overlay lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveCatalog {
    pub catalog: Catalog,
    /// Names of project entries hidden because the user catalog has the same name.
    pub shadowed: Vec<String>,
}

const PROJECT_STDIO_UNSUPPORTED: &str = "project stdio MCP servers are not supported yet";

/// Deep-merges `over` onto `base`: objects merge key by key, recursively;
/// scalars and arrays in `over` replace whatever `base` held.
fn merge_json(base: &mut serde_json::Value, over: serde_json::Value) {
    match (base, over) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge_json(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// Turns JSONC into JSON: drops `//` and `/* */` comments and trailing commas
/// before `}` or `]`, and a leading byte-order mark. String contents are left
/// alone, so `"http://x"` and `"a, }"` survive.
fn strip_jsonc(src: &str) -> String {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);

    // Pass 1: comments.
    let mut out = String::with_capacity(src.len());
    let mut it = src.chars().peekable();
    let mut in_str = false;
    while let Some(c) = it.next() {
        if in_str {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(n) = it.next() {
                        out.push(n);
                    }
                }
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '/' if it.peek() == Some(&'/') => {
                // Line comment: skip to (not including) the newline.
                while it.peek().is_some_and(|n| *n != '\n') {
                    it.next();
                }
            }
            '/' if it.peek() == Some(&'*') => {
                it.next();
                let mut prev = '\0';
                for n in it.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                // A comment separates tokens like whitespace does.
                out.push(' ');
            }
            _ => out.push(c),
        }
    }

    // Pass 2: trailing commas.
    let chars: Vec<char> = out.chars().collect();
    let mut res = String::with_capacity(out.len());
    let mut in_str = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            res.push(c);
            if c == '\\' {
                if let Some(n) = chars.get(i + 1) {
                    res.push(*n);
                    i += 1;
                }
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            res.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                res.push(c);
            }
        } else {
            res.push(c);
        }
        i += 1;
    }
    res
}

fn has_git_entry(dir: &Path) -> bool {
    // `.git` is a directory in a normal checkout and a file in a worktree or submodule.
    dir.join(".git").symlink_metadata().is_ok()
}

/// Finds the project config for a session started in `cwd`.
///
/// - `cwd` is canonicalised (raw path if that fails). One that is not a
///   directory yields an empty config and a note in `problems`.
/// - The walk goes from `cwd` up to the first directory holding a `.git`
///   entry (file or directory), inclusive. With no `.git` anywhere up to `/`,
///   only `cwd` is read. This is stricter than opencode, which walks to `/`;
///   a shared parent such as `/tmp` must not be able to inject servers.
/// - Each directory is checked for `opencode.jsonc` and `opencode.json`.
///   Like opencode, the list is collected nearest-first as `[jsonc, json]` per
///   directory and then reversed, so files apply farthest directory first and,
///   within one directory, `.json` before `.jsonc`. Later files win.
/// - Files are JSONC. The `mcp` objects are deep-merged (objects recursively,
///   scalars and arrays replaced). A file that cannot be read or parsed is
///   skipped and noted in `problems`; a missing file is not a problem.
pub fn discover(cwd: &Path) -> ProjectConfig {
    let mut cfg = ProjectConfig::default();
    let dir = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if !dir.is_dir() {
        cfg.problems
            .push(format!("{}: not a readable directory", cwd.display()));
        return cfg;
    }

    let mut dirs = Vec::new();
    for d in dir.ancestors() {
        dirs.push(d.to_path_buf());
        if has_git_entry(d) {
            cfg.root = Some(d.to_path_buf());
            break;
        }
    }
    if cfg.root.is_none() {
        dirs = vec![dir.clone()];
    }

    let mut candidates = Vec::new();
    for d in &dirs {
        candidates.push(d.join("opencode.jsonc"));
        candidates.push(d.join("opencode.json"));
    }
    candidates.reverse();

    let mut merged = serde_json::Map::new();
    let mut last_file: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in candidates {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                cfg.problems.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let root: serde_json::Value = match serde_json::from_str(&strip_jsonc(&text)) {
            Ok(v) => v,
            Err(e) => {
                cfg.problems.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        cfg.files.push(path.clone());
        match root.get("mcp") {
            None => {}
            Some(serde_json::Value::Object(mcp)) => {
                for (name, val) in mcp {
                    match merged.get_mut(name) {
                        Some(slot) => merge_json(slot, val.clone()),
                        None => {
                            merged.insert(name.clone(), val.clone());
                        }
                    }
                    last_file.insert(name.clone(), path.clone());
                }
            }
            Some(_) => cfg
                .problems
                .push(format!("{}: \"mcp\" is not an object", path.display())),
        }
    }

    for (name, val) in &merged {
        let mut entry = parse_entry(name, val, None);
        if let Some(file) = last_file.get(name) {
            entry.source = EntrySource::Project(file.clone());
        }
        // A repo must not choose a command honkd runs.
        if matches!(
            &entry.supported,
            Ok(GooseExtensionSpec {
                transport: Transport::Stdio { .. },
                ..
            })
        ) {
            entry.supported = Err(PROJECT_STDIO_UNSUPPORTED.to_string());
        }
        cfg.entries.push(entry);
    }
    cfg.entries.sort_by(|a, b| a.name.cmp(&b.name));
    cfg
}

/// The user catalog with the project's entries added. A user entry beats a
/// project entry of the same name (the project entry is listed in `shadowed`),
/// so a repo cannot reuse a name the user has trusted. Merge the swarm entry
/// afterwards; it overrides both.
pub fn effective_catalog(mut user: Catalog, project: &ProjectConfig) -> EffectiveCatalog {
    let mut shadowed = Vec::new();
    for entry in &project.entries {
        if user.get(&entry.name).is_some() {
            shadowed.push(entry.name.clone());
        } else {
            user.entries.push(entry.clone());
        }
    }
    user.entries.sort_by(|a, b| a.name.cmp(&b.name));
    EffectiveCatalog {
        catalog: user,
        shadowed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::catalog::{EntrySource, Transport};
    use std::fs;
    use tempfile::tempdir;

    const TEC: &str =
        r#"{"mcp":{"tec":{"type":"remote","url":"http://localhost:4006/mcp","enabled":true}}}"#;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    /// A canonical tempdir root, so expected paths match `discover`'s canonicalised ones.
    fn root() -> (tempfile::TempDir, PathBuf) {
        let td = tempdir().unwrap();
        let p = td.path().canonicalize().unwrap();
        (td, p)
    }

    fn entry<'a>(cfg: &'a ProjectConfig, name: &str) -> &'a CatalogEntry {
        cfg.entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("no entry {name} in {:?}", cfg.entries))
    }

    fn url_of(e: &CatalogEntry) -> &str {
        match &e.supported.as_ref().unwrap().transport {
            Transport::Http { url } => url,
            other => panic!("not http: {other:?}"),
        }
    }

    #[test]
    fn reads_git_root_config_from_cwd() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let file = write(&r, "opencode.json", TEC);
        let cfg = discover(&r);
        assert_eq!(cfg.root.as_deref(), Some(r.as_path()));
        assert_eq!(cfg.files, vec![file.clone()]);
        assert!(cfg.problems.is_empty());
        let e = entry(&cfg, "tec");
        assert_eq!(url_of(e), "http://localhost:4006/mcp");
        assert_eq!(e.source, EntrySource::Project(file));
        assert!(e.enabled);
        assert!(e.auto_eligible());
    }

    #[test]
    fn worktree_git_file_stops_the_walk() {
        let (_td, r) = root();
        // Outer repo with its own config; the inner worktree has a .git FILE.
        fs::create_dir(r.join(".git")).unwrap();
        write(
            &r,
            "opencode.json",
            r#"{"mcp":{"outer":{"type":"remote","url":"http://localhost:1/mcp"}}}"#,
        );
        let wt = r.join("wt");
        fs::create_dir(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: /tmp/elsewhere\n").unwrap();
        write(&wt, "opencode.json", TEC);
        let sub = wt.join("src");
        fs::create_dir(&sub).unwrap();

        let cfg = discover(&sub);
        assert_eq!(cfg.root.as_deref(), Some(wt.as_path()));
        assert!(cfg.entries.iter().all(|e| e.name != "outer"));
        assert_eq!(url_of(entry(&cfg, "tec")), "http://localhost:4006/mcp");
    }

    #[test]
    fn nearer_file_overrides_root_and_files_are_ordered() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let root_file = write(
            &r,
            "opencode.json",
            r#"{"mcp":{"tec":{"type":"remote","url":"http://localhost:1/mcp"},"only-root":{"type":"remote","url":"http://localhost:2/mcp"}}}"#,
        );
        let sub = r.join("pkg");
        let sub_file = write(
            &sub,
            "opencode.json",
            r#"{"mcp":{"tec":{"url":"http://localhost:3/mcp"}}}"#,
        );
        let cfg = discover(&sub);
        assert_eq!(cfg.files, vec![root_file.clone(), sub_file.clone()]);
        assert_eq!(url_of(entry(&cfg, "tec")), "http://localhost:3/mcp");
        // Source is the file that last touched the entry.
        assert_eq!(entry(&cfg, "tec").source, EntrySource::Project(sub_file));
        assert_eq!(
            entry(&cfg, "only-root").source,
            EntrySource::Project(root_file)
        );
    }

    #[test]
    fn jsonc_applies_after_json_within_a_directory() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let json = write(
            &r,
            "opencode.json",
            r#"{"mcp":{"a":{"type":"remote","url":"http://localhost:1/mcp"}}}"#,
        );
        let jsonc = write(
            &r,
            "opencode.jsonc",
            r#"{"mcp":{"a":{"url":"http://localhost:2/mcp"}}}"#,
        );
        let cfg = discover(&r);
        assert_eq!(cfg.files, vec![json, jsonc.clone()]);
        assert_eq!(url_of(entry(&cfg, "a")), "http://localhost:2/mcp");
        assert_eq!(entry(&cfg, "a").source, EntrySource::Project(jsonc));
    }

    #[test]
    fn nearer_enabled_false_deep_merges_and_keeps_url() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(&r, "opencode.json", TEC);
        let sub = r.join("pkg");
        write(
            &sub,
            "opencode.json",
            r#"{"mcp":{"tec":{"enabled":false}}}"#,
        );
        let cfg = discover(&sub);
        let e = entry(&cfg, "tec");
        assert!(!e.enabled);
        assert_eq!(url_of(e), "http://localhost:4006/mcp");
        assert!(!e.auto_eligible());
    }

    #[test]
    fn arrays_and_scalars_replace_objects_merge() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(
            &r,
            "opencode.json",
            r#"{"mcp":{"x":{"type":"local","command":["a","b","c"],"environment":{"K":"1","L":"2"}}}}"#,
        );
        let sub = r.join("pkg");
        write(
            &sub,
            "opencode.json",
            r#"{"mcp":{"x":{"command":["z"],"environment":{"K":"9"}}}}"#,
        );
        let merged = discover(&sub);
        // Project stdio is unsupported, so inspect through the reason only;
        // the merge itself is covered by the unit test on `merge_json`.
        assert!(entry(&merged, "x").supported.is_err());
        let mut base = serde_json::json!({"c":["a","b"],"e":{"K":"1","L":"2"},"s":1});
        merge_json(
            &mut base,
            serde_json::json!({"c":["z"],"e":{"K":"9"},"s":{"o":1}}),
        );
        assert_eq!(
            base,
            serde_json::json!({"c":["z"],"e":{"K":"9","L":"2"},"s":{"o":1}})
        );
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_parse() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(
            &r,
            "opencode.jsonc",
            r#"// top comment
{
  /* block
     comment */
  "mcp": {
    "tec": {
      "type": "remote", // trailing comment
      "url": "http://localhost:4006/mcp//not-a-comment", // keep // in strings
      "note": "has /* and , } inside",
      "list": [1, 2,],
    },
  },
}
"#,
        );
        let cfg = discover(&r);
        assert!(cfg.problems.is_empty(), "{:?}", cfg.problems);
        assert_eq!(
            url_of(entry(&cfg, "tec")),
            "http://localhost:4006/mcp//not-a-comment"
        );
    }

    #[test]
    fn strip_jsonc_handles_escapes_and_edge_cases() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(&strip_jsonc(s)).unwrap();
        assert_eq!(v(r#"{"a":"q\"//x",}"#), serde_json::json!({"a":"q\"//x"}));
        assert_eq!(
            v("{\"a\":\"back\\\\\" // c\n}"),
            serde_json::json!({"a":"back\\"})
        );
        assert_eq!(v("\u{feff}{\"a\":[1,/*c*/]}"), serde_json::json!({"a":[1]}));
        assert_eq!(v("{\"a\":1 , \n }"), serde_json::json!({"a":1}));
        // Non-ASCII passes through.
        assert_eq!(v(r#"{"a":"é☃"}"#), serde_json::json!({"a":"é☃"}));
    }

    #[test]
    fn bad_file_is_skipped_and_reported() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let bad = write(&r, "opencode.jsonc", "{ not json");
        write(&r, "opencode.json", TEC);
        let cfg = discover(&r);
        assert_eq!(cfg.problems.len(), 1);
        assert!(cfg.problems[0].contains(&bad.display().to_string()));
        assert_eq!(url_of(entry(&cfg, "tec")), "http://localhost:4006/mcp");
        assert_eq!(cfg.files.len(), 1);
    }

    #[test]
    fn non_object_mcp_is_reported_and_unreadable_file_is_a_problem() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(&r, "opencode.jsonc", r#"{"mcp":[1]}"#);
        // A directory named opencode.json cannot be read as a file.
        fs::create_dir(r.join("opencode.json")).unwrap();
        let cfg = discover(&r);
        assert_eq!(cfg.problems.len(), 2, "{:?}", cfg.problems);
        assert!(cfg.entries.is_empty());
    }

    #[test]
    fn no_git_anywhere_reads_only_cwd() {
        let (_td, r) = root();
        write(&r, "opencode.json", TEC);
        let sub = r.join("sub");
        fs::create_dir(&sub).unwrap();
        let cfg = discover(&sub);
        assert_eq!(cfg.root, None);
        assert!(cfg.files.is_empty());
        assert!(cfg.entries.is_empty());

        let own = write(
            &sub,
            "opencode.json",
            r#"{"mcp":{"mine":{"type":"remote","url":"http://localhost:5/mcp"}}}"#,
        );
        let cfg = discover(&sub);
        assert_eq!(cfg.files, vec![own]);
        assert_eq!(cfg.entries.len(), 1);
        assert_eq!(cfg.entries[0].name, "mine");
    }

    #[test]
    fn symlinked_cwd_resolves_to_the_real_projects_root() {
        let (_td, r) = root();
        let real = r.join("real");
        fs::create_dir_all(real.join("pkg")).unwrap();
        fs::create_dir(real.join(".git")).unwrap();
        write(&real, "opencode.json", TEC);
        let links = r.join("links");
        fs::create_dir(&links).unwrap();
        let link = links.join("pkg-link");
        std::os::unix::fs::symlink(real.join("pkg"), &link).unwrap();

        let cfg = discover(&link);
        assert_eq!(cfg.root.as_deref(), Some(real.as_path()));
        assert_eq!(url_of(entry(&cfg, "tec")), "http://localhost:4006/mcp");
    }

    #[test]
    fn missing_cwd_yields_empty_config_with_a_note() {
        let (_td, r) = root();
        let cfg = discover(&r.join("does-not-exist"));
        assert!(cfg.entries.is_empty());
        assert_eq!(cfg.problems.len(), 1);
    }

    #[test]
    fn project_stdio_entry_is_unsupported() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(
            &r,
            "opencode.json",
            r#"{"mcp":{"loc":{"type":"local","command":["npx","-y","thing"]}}}"#,
        );
        let cfg = discover(&r);
        assert_eq!(
            entry(&cfg, "loc").supported.as_ref().unwrap_err(),
            "project stdio MCP servers are not supported yet"
        );
        assert!(!entry(&cfg, "loc").auto_eligible());
    }

    #[test]
    fn auto_eligibility_cases() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(
            &r,
            "opencode.json",
            r#"{"mcp":{
              "tec":{"type":"remote","url":"http://localhost:4006/mcp"},
              "ext":{"type":"remote","url":"https://example.com/mcp"},
              "hdr":{"type":"remote","url":"http://localhost:1/mcp","headers":{"A":"b"}},
              "off":{"type":"remote","url":"http://localhost:2/mcp","enabled":false},
              "oa":{"type":"remote","url":"http://localhost:3/mcp","oauth":{"clientId":"x"}}
            }}"#,
        );
        let cfg = discover(&r);
        assert!(entry(&cfg, "tec").auto_eligible());
        assert!(!entry(&cfg, "ext").auto_eligible());
        assert!(entry(&cfg, "ext").supported.is_ok());
        assert!(!entry(&cfg, "hdr").auto_eligible());
        assert!(entry(&cfg, "hdr").supported.is_err());
        assert!(!entry(&cfg, "off").auto_eligible());
        assert!(!entry(&cfg, "oa").auto_eligible());
        let names: Vec<_> = cfg.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["ext", "hdr", "oa", "off", "tec"]);
    }

    #[test]
    fn user_entry_shadows_project_entry_of_same_name() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(
            &r,
            "opencode.json",
            r#"{"mcp":{
              "slack":{"type":"remote","url":"http://localhost:9/mcp"},
              "tec":{"type":"remote","url":"http://localhost:4006/mcp"}
            }}"#,
        );
        let project = discover(&r);
        let user = Catalog::load_from_str(
            r#"{"mcp":{"slack":{"type":"local","command":["slack-mcp"]},"zed":{"type":"local","command":["zed"]}}}"#,
            None,
        )
        .unwrap();
        let eff = effective_catalog(user, &project);
        assert_eq!(eff.shadowed, vec!["slack".to_string()]);
        let names: Vec<_> = eff
            .catalog
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["slack", "tec", "zed"]);
        assert_eq!(eff.catalog.get("slack").unwrap().source, EntrySource::User);
        assert!(!eff.catalog.get("slack").unwrap().auto_eligible());
        assert!(matches!(
            eff.catalog.get("tec").unwrap().source,
            EntrySource::Project(_)
        ));
        assert!(eff.catalog.get("tec").unwrap().auto_eligible());
    }

    #[test]
    fn swarm_still_overrides_project_entries() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(
            &r,
            "opencode.json",
            r#"{"mcp":{"swarm":{"type":"remote","url":"http://localhost:9/mcp"}}}"#,
        );
        let mut eff = effective_catalog(Catalog::default(), &discover(&r));
        assert!(eff.catalog.get("swarm").unwrap().auto_eligible());
        eff.catalog.merge_swarm_entry(
            &crate::mcp::SwarmMcpConfig {
                bin: PathBuf::from("/bin/honk"),
                pigeon_url: "http://127.0.0.1:1".into(),
                pigeon_token_file: None,
                honk_url: "http://127.0.0.1:2/acp".into(),
                honk_secret_file: PathBuf::from("/tmp/s"),
            },
            "sid",
        );
        let e = eff.catalog.get("swarm").unwrap();
        assert_eq!(e.source, EntrySource::Builtin);
        assert!(!e.auto_eligible());
        assert_eq!(eff.catalog.entries.len(), 1);
    }
}
