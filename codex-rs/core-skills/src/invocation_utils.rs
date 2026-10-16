use std::collections::HashMap;

use crate::SkillMetadata;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

pub(crate) fn build_implicit_skill_path_indexes(
    skills: Vec<SkillMetadata>,
) -> (
    HashMap<PathUri, SkillMetadata>,
    HashMap<PathUri, SkillMetadata>,
) {
    let mut by_scripts_dir = HashMap::new();
    let mut by_skill_doc_path = HashMap::new();
    for skill in skills {
        let Ok(skill_path) = skill.path_to_skills_md.to_abs_path() else {
            continue;
        };
        let skill_doc_path = canonicalize_if_exists(&skill_path);
        by_skill_doc_path.insert(PathUri::from_abs_path(&skill_doc_path), skill.clone());

        if let Some(skill_dir) = skill_path.parent() {
            let scripts_dir = canonicalize_if_exists(&skill_dir.join("scripts"));
            by_scripts_dir.insert(PathUri::from_abs_path(&scripts_dir), skill);
        }
    }

    (by_scripts_dir, by_skill_doc_path)
}

fn canonicalize_if_exists(path: &AbsolutePathBuf) -> AbsolutePathBuf {
    path.canonicalize().unwrap_or_else(|_| path.clone())
}
