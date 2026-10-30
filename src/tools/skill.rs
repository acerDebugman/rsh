use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};

use super::{Tool, ToolCtx, ToolOutput, function_schema};

pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Load a skill by name and follow its instructions. Use the skill tool again when a skill tells you to load another skill."
    }

    fn schema(&self) -> Value {
        function_schema(
            "skill",
            self.description(),
            json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
                "additionalProperties": false
            }),
        )
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult<ToolOutput> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("name is required".into()))?;
        let Some(skills) = &ctx.skills else {
            return Ok(ToolOutput::error("no skills are loaded in this session"));
        };
        let Some(skill) = skills.get(name) else {
            return Ok(ToolOutput::error(format!("unknown skill: {name}")));
        };
        if skill.disable_model_invocation {
            return Ok(ToolOutput::error(format!(
                "skill {name} is user-invocable only; ask the user to run /skill {name}"
            )));
        }
        Ok(ToolOutput::ok(skill.render()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::*;
    use crate::skills::SkillRegistry;

    fn registry() -> (Arc<SkillRegistry>, PathBuf) {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("skill-tool-")
            .tempdir_in(&base)
            .unwrap()
            .keep();
        let dir = root.join(".agents").join("skills").join("grilling");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: grilling\ndescription: interview\n---\n\nAsk one question at a time.\n",
        )
        .unwrap();
        (Arc::new(SkillRegistry::discover(&root)), root)
    }

    #[tokio::test]
    async fn renders_a_known_skill() {
        let (skills, root) = registry();
        let ctx = ToolCtx::new(root).with_skills(Some(skills));
        let output = SkillTool
            .run(json!({"name": "grilling"}), &ctx)
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("Ask one question at a time"));
        assert!(output.content.contains("Resource base directory"));
    }

    #[tokio::test]
    async fn unknown_skill_is_an_error_result() {
        let (skills, root) = registry();
        let ctx = ToolCtx::new(root).with_skills(Some(skills));
        let output = SkillTool
            .run(json!({"name": "missing"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
    }
}
