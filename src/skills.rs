use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub base_dir: PathBuf,
    pub disable_model_invocation: bool,
}

impl Skill {
    /// Render the skill for the model, including the resource base directory.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "<skill name=\"{}\" base=\"{}\">\n{}\n</skill>\n\nResource base directory: {}\nResolve relative paths in the instructions against this directory.",
            self.name,
            self.base_dir.display(),
            self.body.trim(),
            self.base_dir.display()
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Skill>,
}

impl SkillRegistry {
    /// Discover global skills first, then project skills (project wins on a
    /// name collision).
    #[must_use]
    pub fn discover(workspace: &Path) -> Self {
        let global = dirs::home_dir().map(|home| home.join(".agents").join("skills"));
        Self::from_dirs(global.as_deref(), &workspace.join(".agents").join("skills"))
    }

    fn from_dirs(global: Option<&Path>, project: &Path) -> Self {
        let mut skills = BTreeMap::new();
        if let Some(global) = global {
            collect(global, &mut skills);
        }
        collect(project, &mut skills);
        Self { skills }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    #[must_use]
    pub fn list(&self) -> Vec<&Skill> {
        self.skills.values().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

fn collect(dir: &Path, out: &mut BTreeMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        match parse_skill(&path) {
            Ok(skill) => {
                out.insert(skill.name.clone(), skill);
            }
            Err(error) => eprintln!("warning: skipping {}: {error}", path.display()),
        }
    }
}

fn parse_skill(path: &Path) -> Result<Skill> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (front, body) = split_frontmatter(&text);
    let fallback_name = path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "unnamed".to_string());
    let name = front
        .get("name")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or(fallback_name);
    let description = front.get("description").cloned().unwrap_or_default();
    let disable_model_invocation = front
        .get("disable-model-invocation")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    Ok(Skill {
        name,
        description,
        body: body.to_string(),
        base_dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        disable_model_invocation,
    })
}

fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut front = BTreeMap::new();
    let Some(rest) = text.strip_prefix("---") else {
        return (front, text);
    };
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let Some(end) = rest.find("\n---") else {
        return (front, text);
    };
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        front.insert(key.trim().to_string(), value);
    }
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);
    (front, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("skills-")
            .tempdir_in(&base)
            .unwrap()
            .keep()
    }

    fn write_skill(root: &Path, dir: &str, front: &str, body: &str) {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\n{front}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn parses_frontmatter_and_flags() {
        let root = scratch();
        let project = root.join(".agents").join("skills");
        write_skill(
            &project,
            "grill-me",
            "name: grill-me\ndescription: interview\ndisable-model-invocation: true",
            "Call the Skill tool with \"grilling\".",
        );
        let registry = SkillRegistry::from_dirs(None, &project);
        let skill = registry.get("grill-me").unwrap();
        assert_eq!(skill.description, "interview");
        assert!(skill.disable_model_invocation);
        assert!(skill.render().contains("Resource base directory"));
        assert!(skill.render().contains("Call the Skill tool"));
    }

    #[test]
    fn project_skill_overrides_global() {
        let root = scratch();
        write_skill(
            &root,
            "demo",
            "name: demo\ndescription: global",
            "global body",
        );
        write_skill(
            &root.join(".agents").join("skills"),
            "demo",
            "name: demo\ndescription: project",
            "project body",
        );
        let registry = SkillRegistry::discover(&root);
        assert_eq!(registry.get("demo").unwrap().description, "project");
    }

    #[test]
    fn missing_frontmatter_is_error_tolerant() {
        let root = scratch();
        let project = root.join(".agents").join("skills");
        write_skill(&project, "plain", "", "just body");
        let registry = SkillRegistry::from_dirs(None, &project);
        let skill = registry.get("plain").unwrap();
        assert_eq!(skill.body.trim(), "just body");
    }
}
