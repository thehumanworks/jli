//! Which project a directory belongs to: the nearest `.jlirc` at or above it. See
//! docs/adr/0017-project-scope.md.
//!
//! The file names the project by an id, never by a path, so another checkout of the same project,
//! on this host or another, or the same directory after a move, is the same project:
//!
//! ```text
//! # jli project: what jli learns here is kept for this project.
//! project = shop-api-4k2x9q
//! ```

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};

use crate::store::slug;
use crate::types::{coded, CONFIGURATION};

pub const FILE: &str = ".jlirc";

/// The project a directory is in, and the file that says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub id: String,
    pub path: PathBuf,
}

/// A project id as written: a short slug, held to the same rule as a user id.
pub fn project_id(text: &str) -> Result<String> {
    let mut bytes = text.bytes();
    let starts_well = bytes.next().is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    let rest_well = bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_');
    if starts_well && rest_well && text.len() <= 32 {
        Ok(text.to_string())
    } else {
        Err(anyhow!(
            "a project id is 1 to 32 lowercase letters, digits, - or _, starting with a letter or digit, got \"{text}\""
        ))
    }
}

/// The project id in the text of a `.jlirc`. Blank lines and `#` comments are skipped, a value may
/// be quoted, and keys other than `project` are ignored so that a later jli can add some.
pub fn parse(text: &str) -> Result<String> {
    let mut id = None;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            bail!("line {} is not `key = value`: {line}", number + 1);
        };
        if key.trim() != "project" {
            continue;
        }
        if id.is_some() {
            bail!("line {} names the project a second time", number + 1);
        }
        let value = value.trim();
        let unquoted = ['"', '\'']
            .iter()
            .find_map(|quote| value.strip_prefix(*quote).and_then(|inner| inner.strip_suffix(*quote)))
            .unwrap_or(value);
        id = Some(project_id(unquoted)?);
    }
    id.ok_or_else(|| anyhow!("it has no `project = <id>` line"))
}

/// The nearest `.jlirc` at or above `cwd`. Under `home` the search stops at `home` itself, so a
/// stray file above it never makes every directory one project; elsewhere it goes up to `/`.
/// A file that is there but cannot be read as a project stops the run: ignoring it would put what
/// is learned in the wrong scope.
pub fn find(cwd: &Path, home: Option<&Path>) -> Result<Option<Found>> {
    for dir in cwd.ancestors() {
        let path = dir.join(FILE);
        if path.is_file() {
            let text = std::fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
            // A configuration error for `--json` callers: the fix is in the file, not in jli.
            let id = parse(&text)
                .map_err(|error| coded(CONFIGURATION, format!("{} is not a valid {FILE}: {error:#}", path.display())))?;
            return Ok(Some(Found { id, path }));
        }
        if home.is_some_and(|home| home == dir) {
            break;
        }
    }
    Ok(None)
}

/// A fresh id for a project in `dir`: its name, made a slug, and six random characters, so two
/// projects with the same directory name do not meet.
pub fn new_id(dir: &Path) -> String {
    let name = dir.file_name().map(|name| slug(&name.to_string_lossy())).filter(|name| name != "skill");
    let name: String = name.unwrap_or_else(|| "project".into()).chars().take(20).collect();
    let mut random = RandomState::new().build_hasher();
    random.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let mut n = random.finish();
    let mut suffix = String::new();
    for _ in 0..6 {
        let digit = (n % 36) as u32;
        suffix.push(char::from_digit(digit, 36).expect("below 36"));
        n /= 36;
    }
    format!("{}-{suffix}", name.trim_end_matches('-'))
}

/// Write `dir/.jlirc` for `id`, or a new id. An existing file is never overwritten.
pub fn init(dir: &Path, id: Option<&str>) -> Result<Found> {
    let id = match id {
        Some(id) => project_id(id)?,
        None => new_id(dir),
    };
    let path = dir.join(FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("could not create {}", path.display()))?;
    write!(
        file,
        "# jli project: what jli learns in this directory and below it is kept for this project.\n\
         # The id is not a path, so every checkout of the project is the same project. Safe to commit.\n\
         project = {id}\n"
    )?;
    Ok(Found { id, path })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "jli-project-{name}-{}-{}",
                std::process::id(),
                crate::store::now_iso().replace([':', '.'], "")
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_nearest_jlirc_names_the_project() {
        let scratch = Scratch::new("find");
        let deep = scratch.0.join("repo/src/parser/tests");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find(&deep, None).unwrap(), None, "no file anywhere: no project");

        std::fs::write(scratch.0.join("repo/.jlirc"), "project = shop-api\n").unwrap();
        let found = find(&deep, None).unwrap().unwrap();
        assert_eq!((found.id.as_str(), found.path.clone()), ("shop-api", scratch.0.join("repo/.jlirc")));
        assert_eq!(find(&scratch.0.join("repo"), None).unwrap().unwrap().id, "shop-api", "its own directory counts");
        assert_eq!(find(&scratch.0, None).unwrap(), None, "a directory above it is not in it");

        std::fs::write(scratch.0.join("repo/src/.jlirc"), "# nested\nproject = \"shop-core\"\n").unwrap();
        assert_eq!(find(&deep, None).unwrap().unwrap().id, "shop-core", "the nearest one wins");

        // The search stops at home: a file above it does not count.
        let home = scratch.0.join("home");
        let work = home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(scratch.0.join(".jlirc"), "project = everything\n").unwrap();
        assert_eq!(find(&work, Some(&home)).unwrap(), None);
        assert_eq!(find(&work, None).unwrap().unwrap().id, "everything", "outside home it goes to the root");
        std::fs::write(home.join(".jlirc"), "project = home\n").unwrap();
        assert_eq!(find(&work, Some(&home)).unwrap().unwrap().id, "home", "home itself is checked");
    }

    #[test]
    fn a_malformed_jlirc_stops_the_run() {
        let scratch = Scratch::new("malformed");
        for (text, error) in [
            ("", "it has no `project = <id>` line"),
            ("# only a comment\n", "it has no `project = <id>` line"),
            ("project shop\n", "line 1 is not `key = value`: project shop"),
            ("project = Shop API\n", "a project id is 1 to 32 lowercase letters"),
            ("project = /home/me/shop\n", "a project id is 1 to 32"),
            ("project = a\nproject = b\n", "line 2 names the project a second time"),
        ] {
            std::fs::write(scratch.0.join(".jlirc"), text).unwrap();
            let failed = find(&scratch.0, None).unwrap_err();
            let message = format!("{failed:#}");
            assert!(message.contains("is not a valid .jlirc") && message.contains(error), "{text:?}: {message}");
            assert_eq!(crate::types::error_code(&failed), CONFIGURATION, "--json reports it as configuration");
        }
        std::fs::write(scratch.0.join(".jlirc"), "colour = blue\nproject = 'shop'\n").unwrap();
        assert_eq!(find(&scratch.0, None).unwrap().unwrap().id, "shop", "an unknown key is ignored");
    }

    #[test]
    fn init_writes_a_file_and_never_overwrites_one() {
        let scratch = Scratch::new("init");
        let dir = scratch.0.join("My Shop.API");
        std::fs::create_dir_all(&dir).unwrap();
        let made = init(&dir, None).unwrap();
        assert!(made.id.starts_with("my-shop-api-") && made.id.len() == "my-shop-api-".len() + 6, "{}", made.id);
        assert_eq!(project_id(&made.id).unwrap(), made.id);
        assert_eq!(find(&dir, None).unwrap(), Some(made.clone()), "what init writes, find reads");
        assert!(init(&dir, Some("other")).is_err(), "an existing file is never overwritten");
        assert_eq!(find(&dir, None).unwrap().unwrap().id, made.id);
        assert!(init(&scratch.0, Some("Not An Id")).is_err());
        assert!(!scratch.0.join(".jlirc").exists(), "an invalid id writes nothing");
        assert_eq!(init(&scratch.0, Some("shop")).unwrap().id, "shop");
        assert_ne!(new_id(&dir), new_id(&dir), "two new ids differ");
        assert!(new_id(Path::new("/")).starts_with("project-"));
    }
}
