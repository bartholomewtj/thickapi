#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! The captured Claude Code tool schemas (K2, #220).
//!
//! Lists, for each tool the dialect may expose, the property names the installed CLI
//! sends. A later CLI that renames one fails schema_diff against this fixture.

use std::collections::BTreeSet;

use thickapi_core::thin::claude_tools::{load_newest, property_sets, schema_diff};

/// Tools the dialect (K3) names. Agent is what 2.1.288 sends; Task is the old name.
const DIALECT: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Glob",
    "Grep",
    "TodoWrite",
    "Agent",
    "Task",
    "AskUserQuestion",
];

#[test]
fn newest_set_lists_property_names_for_each_dialect_tool() {
    let set = load_newest().expect("committed claude-tools set");
    let props = property_sets(&set);
    let names: BTreeSet<&str> = props.iter().map(|(n, _)| n.as_str()).collect();
    // The agent tool's actual name (Task was renamed). Printed for the K4 brief.
    let agent = if names.contains("Agent") {
        "Agent"
    } else if names.contains("Task") {
        "Task"
    } else {
        "absent"
    };
    eprintln!("claude-tools {} agent tool: {agent}", set.cli_version);
    for (name, properties) in &props {
        if DIALECT.contains(&name.as_str()) {
            let list: Vec<&str> = properties.iter().map(String::as_str).collect();
            eprintln!("{name}: {}", list.join(", "));
        }
    }
    for required in ["Read", "Write", "Edit", "Bash", "Glob", "Grep"] {
        assert!(names.contains(required), "fixture missing {required}");
    }
    assert!(
        names.contains("Agent") || names.contains("Task"),
        "fixture has neither Agent nor Task"
    );
    // The committed set is the baseline: diffing it with itself must be empty.
    assert!(schema_diff(&set, &set).is_empty());
}
