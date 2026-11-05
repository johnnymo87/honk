//! Project-scoped MCP servers, read from the session's `opencode.json[c]` files.
//!
//! Discovery mirrors opencode closely enough that one config serves both
//! harnesses, and is stricter where opencode is loose. See [`discover`].

use crate::mcp::catalog::{
    Catalog, CatalogEntry, EntrySource, GooseExtensionSpec, Transport, parse_entry, sibling_name,
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
    /// Project entries left out because the user catalog outranks them, as
    /// `(name, reason)`, sorted by name.
    pub shadowed: Vec<(String, String)>,
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
/// - Reads are defensive, because the repo picks the paths: a candidate must be
///   a regular file (symlinks followed) of at most 1 MiB, resolve inside the
///   walk root (the cwd with no root), and be owned by the uid honkd runs as;
///   the git root directory must be too, or it is ignored (cwd only). See
///   [`discover_as`].
/// - Each directory is checked for `opencode.jsonc` and `opencode.json`.
///   Like opencode, the list is collected nearest-first as `[jsonc, json]` per
///   directory and then reversed, so files apply farthest directory first and,
///   within one directory, `.json` before `.jsonc`. Later files win.
/// - Files are JSONC. The `mcp` objects are deep-merged (objects recursively,
///   scalars and arrays replaced). A file that cannot be read or parsed is
///   skipped and noted in `problems`; a missing file is not a problem.
pub fn discover(cwd: &Path) -> ProjectConfig {
    discover_as(cwd, current_uid())
}

/// The real user id honkd runs as, when this platform lets us read it without
/// a new dependency (Linux: `/proc/self/status`). `None` turns the ownership
/// checks off.
#[cfg(unix)]
pub(crate) fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    // `Uid:  real  effective  saved  fs`; the effective one decides access.
    line.split_whitespace().nth(2)?.parse().ok()
}

#[cfg(not(unix))]
pub(crate) fn current_uid() -> Option<u32> {
    None
}

#[cfg(unix)]
fn owner_of(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.uid())
}

#[cfg(not(unix))]
fn owner_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// True unless `expected` is known and `meta` names a different owner.
fn owned_by(meta: &std::fs::Metadata, expected: Option<u32>) -> bool {
    match (expected, owner_of(meta)) {
        (Some(want), Some(have)) => want == have,
        _ => true,
    }
}

/// Largest config file that is read.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// What reading one candidate config file came to.
enum Read {
    /// No such file; not worth a word.
    Missing,
    Text(String),
    /// Skipped, with the reason.
    Problem(String),
}

/// Reads `path` if it is safe to: a regular file (checked before opening, so a
/// FIFO or device is never opened), reached through no symlink that leaves
/// `within`, owned by `expected_uid`, and at most [`MAX_CONFIG_BYTES`].
fn read_config_file(path: &Path, within: &Path, expected_uid: Option<u32>) -> Read {
    use std::io::Read as _;

    // `metadata` follows symlinks, so a link to /dev/zero or a FIFO is not a file.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Read::Missing,
        Err(e) => return Read::Problem(format!("{}: {e}", path.display())),
    };
    if !meta.is_file() {
        return Read::Problem(format!("{}: not a regular file", path.display()));
    }
    match path.canonicalize() {
        Ok(real) if real.starts_with(within) => {}
        Ok(_) => {
            return Read::Problem(format!(
                "{}: resolves outside the project directory",
                path.display()
            ));
        }
        Err(e) => return Read::Problem(format!("{}: {e}", path.display())),
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Read::Problem(format!("{}: {e}", path.display())),
    };
    // Judge what was opened, not what was there a moment ago.
    match file.metadata() {
        Ok(m) if m.is_file() && owned_by(&m, expected_uid) => {}
        Ok(m) if m.is_file() => {
            return Read::Problem(format!(
                "{}: not owned by the user honkd runs as",
                path.display()
            ));
        }
        Ok(_) => return Read::Problem(format!("{}: not a regular file", path.display())),
        Err(e) => return Read::Problem(format!("{}: {e}", path.display())),
    }
    let mut text = String::new();
    if let Err(e) = file.take(MAX_CONFIG_BYTES + 1).read_to_string(&mut text) {
        return Read::Problem(format!("{}: {e}", path.display()));
    }
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Read::Problem(format!(
            "{}: larger than {} KiB",
            path.display(),
            MAX_CONFIG_BYTES / 1024
        ));
    }
    Read::Text(text)
}

/// [`discover`] with the expected owner of the files it reads given
/// explicitly: the git root directory and every config file must belong to
/// `expected_uid` (`None` skips the check). A root that does not is ignored
/// and only the session directory is read, which keeps a stranger's
/// `/tmp/.git` from adopting every project under `/tmp`.
pub(crate) fn discover_as(cwd: &Path, expected_uid: Option<u32>) -> ProjectConfig {
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
    if let Some(root) = &cfg.root {
        let owned = std::fs::metadata(root).is_ok_and(|m| owned_by(&m, expected_uid));
        if !owned {
            cfg.problems.push(format!(
                "{}: git root is not owned by the user honkd runs as; reading the session directory only",
                root.display()
            ));
            cfg.root = None;
        }
    }
    if cfg.root.is_none() {
        dirs = vec![dir.clone()];
    }
    // Config files must resolve to somewhere inside this.
    let within = cfg.root.clone().unwrap_or_else(|| dir.clone());

    let mut candidates = Vec::new();
    for d in &dirs {
        candidates.push(d.join("opencode.jsonc"));
        candidates.push(d.join("opencode.json"));
    }
    candidates.reverse();

    let mut merged = serde_json::Map::new();
    let mut last_file: BTreeMap<String, PathBuf> = BTreeMap::new();
    for path in candidates {
        let text = match read_config_file(&path, &within, expected_uid) {
            Read::Text(t) => t,
            Read::Missing => continue,
            Read::Problem(p) => {
                cfg.problems.push(p);
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

/// Names a project may never define: honkd generates them itself.
const RESERVED_NAMES: [&str; 2] = ["swarm", "swarm-ro"];

/// `url` as the URL parser spells it, so `http://LOCALHOST:80/x` and
/// `http://localhost/x` compare equal. `None` for anything that is not a URL.
fn normalized_url(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok().map(|u| u.to_string())
}

/// Why the user catalog outranks the project entry `entry`, if it does.
///
/// A project must not be able to stand in for something the user trusted, so an
/// entry loses to the user catalog when it has the same name, when it is the
/// `-ro` / non-`-ro` sibling of a user entry (the pair would replace each
/// other, and `--mcp-no-confirm` is keyed by those names), or when it points
/// at the same URL as a user remote entry.
fn user_outranks(user: &Catalog, entry: &CatalogEntry) -> Option<String> {
    if RESERVED_NAMES.contains(&entry.name.as_str()) {
        return Some("reserved name".to_string());
    }
    if user.get(&entry.name).is_some() {
        return Some("shadowed by user catalog".to_string());
    }
    let sibling = sibling_name(&entry.name);
    if user.get(&sibling).is_some() {
        return Some(format!("pairs with user catalog entry {sibling}"));
    }
    if let Ok(GooseExtensionSpec {
        transport: Transport::Http { url },
        ..
    }) = &entry.supported
        && let Some(norm) = normalized_url(url)
    {
        for other in &user.entries {
            if let Ok(GooseExtensionSpec {
                transport: Transport::Http { url: other_url },
                ..
            }) = &other.supported
                && normalized_url(other_url).as_deref() == Some(norm.as_str())
            {
                return Some(format!("same url as user catalog entry {}", other.name));
            }
        }
    }
    None
}

/// The user catalog with the project's entries added. The user catalog wins:
/// a project entry that [`user_outranks`] is left out and listed in
/// `shadowed` with the reason, so a repo cannot reuse a name, a sibling name
/// or a URL the user has trusted. `swarm` and `swarm-ro` are reserved and are
/// never taken from a project, whether or not swarm is configured. Merge the
/// swarm entry afterwards.
pub fn effective_catalog(mut user: Catalog, project: &ProjectConfig) -> EffectiveCatalog {
    let mut shadowed = Vec::new();
    let mut added = Vec::new();
    // Judged against the user catalog as it was: project entries do not
    // shadow each other.
    for entry in &project.entries {
        match user_outranks(&user, entry) {
            Some(reason) => shadowed.push((entry.name.clone(), reason)),
            None => added.push(entry.clone()),
        }
    }
    user.entries.extend(added);
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
    fn symlink_to_a_device_is_skipped_quickly() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        // Reading this would never end.
        std::os::unix::fs::symlink("/dev/zero", r.join("opencode.json")).unwrap();
        let started = std::time::Instant::now();
        let cfg = discover(&r);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(cfg.entries.is_empty() && cfg.files.is_empty());
        assert_eq!(cfg.problems.len(), 1, "{:?}", cfg.problems);
        assert!(
            cfg.problems[0].contains("not a regular file"),
            "{:?}",
            cfg.problems
        );
    }

    #[test]
    fn fifo_is_never_opened() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let fifo = r.join("opencode.json");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            return; // no mkfifo here
        }
        let started = std::time::Instant::now();
        let cfg = discover(&r);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(cfg.entries.is_empty());
        assert_eq!(cfg.problems.len(), 1, "{:?}", cfg.problems);
    }

    #[test]
    fn symlink_to_a_file_outside_the_root_is_skipped() {
        let (_td, r) = root();
        let (_other_td, other) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let outside = write(&other, "global.json", TEC);
        std::os::unix::fs::symlink(&outside, r.join("opencode.json")).unwrap();

        let cfg = discover(&r);
        assert!(cfg.entries.is_empty(), "{:?}", cfg.entries);
        assert!(cfg.files.is_empty());
        assert_eq!(cfg.problems.len(), 1, "{:?}", cfg.problems);
        assert!(
            cfg.problems[0].contains("outside the project"),
            "{:?}",
            cfg.problems
        );

        // Without a git root the bound is the session directory.
        let (_td2, r2) = root();
        std::os::unix::fs::symlink(&outside, r2.join("opencode.json")).unwrap();
        let cfg = discover(&r2);
        assert!(cfg.entries.is_empty());
        assert_eq!(cfg.problems.len(), 1, "{:?}", cfg.problems);
    }

    #[test]
    fn symlink_to_a_file_inside_the_root_is_read() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let real = write(&r.join("shared"), "mcp.json", TEC);
        std::os::unix::fs::symlink(&real, r.join("opencode.json")).unwrap();
        let cfg = discover(&r);
        assert!(cfg.problems.is_empty(), "{:?}", cfg.problems);
        assert_eq!(url_of(entry(&cfg, "tec")), "http://localhost:4006/mcp");
    }

    #[test]
    fn oversize_file_is_skipped() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let pad = " ".repeat(1024 * 1024 + 1 - TEC.len());
        write(&r, "opencode.json", &format!("{TEC}{pad}"));
        let cfg = discover(&r);
        assert!(cfg.entries.is_empty());
        assert_eq!(cfg.problems.len(), 1, "{:?}", cfg.problems);
        assert!(
            cfg.problems[0].contains("larger than 1024 KiB"),
            "{:?}",
            cfg.problems
        );

        // Exactly at the limit is still read.
        let pad = " ".repeat(1024 * 1024 - TEC.len());
        write(&r, "opencode.json", &format!("{TEC}{pad}"));
        let cfg = discover(&r);
        assert!(cfg.problems.is_empty(), "{:?}", cfg.problems);
        assert_eq!(cfg.entries.len(), 1);
    }

    fn uid_of(p: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(p).unwrap().uid()
    }

    #[test]
    fn root_and_files_owned_by_someone_else_are_not_trusted() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let sub = r.join("pkg");
        write(&r, "opencode.json", TEC);
        write(
            &sub,
            "opencode.json",
            r#"{"mcp":{"mine":{"type":"remote","url":"http://localhost:5/mcp"}}}"#,
        );
        let me = uid_of(&r);

        // Owner matches: root found, both files read.
        let cfg = discover_as(&sub, Some(me));
        assert_eq!(cfg.root.as_deref(), Some(r.as_path()));
        assert_eq!(cfg.files.len(), 2);

        // Somebody else's root is no root: only the session directory counts,
        // and its file is not ours either.
        let cfg = discover_as(&sub, Some(me.wrapping_add(1)));
        assert_eq!(cfg.root, None);
        assert!(cfg.entries.is_empty() && cfg.files.is_empty());
        assert!(
            cfg.problems
                .iter()
                .any(|p| p.contains("git root is not owned")),
            "{:?}",
            cfg.problems
        );
        assert!(
            cfg.problems
                .iter()
                .any(|p| p.contains("opencode.json") && p.contains("not owned")),
            "{:?}",
            cfg.problems
        );

        // No known uid: no check.
        assert_eq!(discover_as(&sub, None).files.len(), 2);
    }

    #[test]
    fn current_uid_matches_the_owner_of_files_we_create() {
        let (_td, r) = root();
        if let Some(uid) = current_uid() {
            assert_eq!(uid, uid_of(&r));
        }
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

    /// A catalog of the given user entries, parsed from opencode-style json.
    fn user_catalog(json: &str) -> Catalog {
        Catalog::load_from_str(json, None).unwrap()
    }

    /// A project of loopback remote entries on distinct ports.
    fn project_with(names: &[&str]) -> (tempfile::TempDir, ProjectConfig) {
        let (td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        let mut mcp = serde_json::Map::new();
        for (i, n) in names.iter().enumerate() {
            mcp.insert(
                n.to_string(),
                serde_json::json!({"type":"remote","url":format!("http://localhost:{}/mcp", 9000 + i)}),
            );
        }
        write(
            &r,
            "opencode.json",
            &serde_json::json!({ "mcp": mcp }).to_string(),
        );
        let cfg = discover(&r);
        (td, cfg)
    }

    fn shadowed(eff: &EffectiveCatalog) -> Vec<(&str, &str)> {
        eff.shadowed
            .iter()
            .map(|(n, r)| (n.as_str(), r.as_str()))
            .collect()
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
        assert_eq!(shadowed(&eff), vec![("slack", "shadowed by user catalog")]);
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
    fn project_swarm_is_always_dropped() {
        let (_td, project) = project_with(&["swarm", "swarm-ro", "tec"]);

        // Swarm not configured: the name is still not the project's to take.
        let eff = effective_catalog(Catalog::default(), &project);
        assert!(eff.catalog.get("swarm").is_none());
        assert!(eff.catalog.get("swarm-ro").is_none());
        assert!(eff.catalog.get("tec").is_some());
        assert_eq!(
            shadowed(&eff),
            vec![("swarm", "reserved name"), ("swarm-ro", "reserved name")]
        );

        // Swarm configured: the builtin is the only one.
        let mut eff = effective_catalog(Catalog::default(), &project);
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
        assert_eq!(eff.catalog.entries.len(), 2); // swarm, tec
    }

    #[test]
    fn user_sibling_names_shadow_project_entries() {
        // A user `slack` makes a project `slack-ro` a stand-in for its sibling;
        // a user `figma-ro` does the same for a project `figma`.
        let (_td, project) = project_with(&["slack-ro", "figma", "plain", "tec"]);
        let user = user_catalog(
            r#"{"mcp":{
              "slack":{"type":"local","command":["s"]},
              "figma-ro":{"type":"local","command":["f"]},
              "plain-ro-ish":{"type":"local","command":["p"]}
            }}"#,
        );
        let eff = effective_catalog(user, &project);
        assert_eq!(
            shadowed(&eff),
            vec![
                ("figma", "pairs with user catalog entry figma-ro"),
                ("slack-ro", "pairs with user catalog entry slack"),
            ]
        );
        assert!(eff.catalog.get("plain").is_some());
        assert!(eff.catalog.get("tec").is_some());
        assert_eq!(
            eff.catalog.get("slack-ro"),
            None,
            "the project slack-ro is gone"
        );
        assert_eq!(eff.catalog.get("figma").map(|e| e.source.clone()), None);
    }

    #[test]
    fn project_url_equal_to_a_user_remote_url_is_shadowed() {
        let (_td, r) = root();
        fs::create_dir(r.join(".git")).unwrap();
        write(
            &r,
            "opencode.json",
            r#"{"mcp":{
              "alias":{"type":"remote","url":"http://LOCALHOST:4006/mcp"},
              "other":{"type":"remote","url":"http://localhost:4007/mcp"}
            }}"#,
        );
        let user =
            user_catalog(r#"{"mcp":{"tec":{"type":"remote","url":"http://localhost:4006/mcp"}}}"#);
        let eff = effective_catalog(user, &discover(&r));
        assert_eq!(
            shadowed(&eff),
            vec![("alias", "same url as user catalog entry tec")]
        );
        assert!(eff.catalog.get("other").is_some());
        assert!(eff.catalog.get("alias").is_none());
    }
}
