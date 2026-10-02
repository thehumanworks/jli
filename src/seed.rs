use crate::store::NewSkill;
use crate::types::{Origin, Risk, Slot, SlotKind};

/// The decision matrix that used to be hard-coded in the proof of concept.
/// It now only seeds an empty brain; everything after this is learned.
pub fn seed_skills() -> Vec<NewSkill> {
    let skill = |label: &str, step: &str, scenario: &str, risk: Risk| NewSkill {
        label: label.into(),
        steps: vec![step.into()],
        scenario: scenario.into(),
        risk,
        origin: Origin::Seed,
        examples: vec![],
        slots: vec![],
    };
    vec![
        NewSkill {
            examples: vec!["What changes are unstaged in this repository?".into()],
            ..skill(
                "git-status",
                "git status",
                "the user wants to know the staged and unstaged changes in a repo",
                Risk::Safe,
            )
        },
        skill("git-stage-all", "git add -A", "the user wants all changes staged to git", Risk::Modifies),
        NewSkill {
            slots: vec![Slot {
                name: "message".into(),
                kind: SlotKind::Text,
                description: "the commit message".into(),
                default: None,
            }],
            ..skill(
                "git-commit",
                "git commit -m {message}",
                "the user wants to commit the staged changes with a commit message",
                Risk::Modifies,
            )
        },
        skill(
            "git-push",
            "git push",
            "the user wants to push the committed work to the default remote",
            Risk::Modifies,
        ),
        skill(
            "git-push-main",
            "git push -u origin main",
            "the user wants to push to remote main and set it as upstream",
            Risk::Modifies,
        ),
        skill(
            "list-files",
            "ls",
            "the user wants a plain list of files/directories in the current directory, no particular order or detail",
            Risk::Safe,
        ),
        skill(
            "list-files-by-time",
            "ls -ltr",
            "the user wants files listed and ordered in reverse order of last modification date",
            Risk::Safe,
        ),
        skill(
            "list-files-with-sizes",
            "ls -lh",
            "the user wants files listed with the size of each file in human readable format",
            Risk::Safe,
        ),
        skill(
            "print-working-directory",
            "pwd",
            "the user wants to know which directory they are in",
            Risk::Safe,
        ),
    ]
}
