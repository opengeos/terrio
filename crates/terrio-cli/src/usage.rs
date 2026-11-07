//! Versioned, metadata-only command catalog built from the same clap tree as MCP.

use clap::{Arg, ArgAction, Command, CommandFactory};
use serde_json::{Value, json};
use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Builds the root clap command with parser tables ready for inspection.
fn root() -> Command {
    let mut command = crate::Cli::command();
    command.build();
    command
}

/// Returns whether `command` is part of the exposed public command tree.
fn exposed(command: &Command) -> bool {
    !matches!(command.get_name(), "mcp" | "ui" | "view" | "help")
}

/// The top-level commands served by both the JSON catalog and MCP tools/list.
pub(crate) fn commands() -> impl Iterator<Item = Command> {
    root().get_subcommands().filter(|c| exposed(c)).cloned().collect::<Vec<_>>().into_iter()
}

/// Detect the flag with clap itself, preserving option-value, variadic and `--` semantics.
/// Ignoring parse errors here permits missing required operands; normal execution still uses
/// the original strict parser when the flag was not parsed as a flag.
pub(crate) fn requested(args: &[OsString]) -> Option<Value> {
    if !args.iter().any(|a| a == OsStr::new("--json-usage")) {
        return None;
    }
    // Set the parser setting before build: otherwise clap has already propagated the
    // original settings to nested commands, and a missing nested operand loses matches.
    let mut root = crate::Cli::command();
    let matches = match root.clone().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error)
            if matches!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument | clap::error::ErrorKind::MissingSubcommand) =>
        {
            root.clone().ignore_errors(true).try_get_matches_from(args).ok()?
        }
        Err(_) => return None,
    };
    root.build();
    let mut requested = matches.get_flag("json_usage");
    let (mut command, mut matches) = (&root, &matches);
    let mut path = vec!["terrio".to_string()];
    while let Some((name, child_matches)) = matches.subcommand() {
        command = command.find_subcommand(name)?;
        if !exposed(command) {
            return None;
        }
        path.push(command.get_name().to_string());
        requested |= child_matches.get_flag("json_usage");
        matches = child_matches;
    }
    if !requested {
        return None;
    }
    Some(json!({
        "schema_version": "0.1.1",
        "unstable": true,
        "program": "terrio",
        "version": env!("CARGO_PKG_VERSION"),
        "command": describe(command, &path),
    }))
}

/// Recursively describes `command` and its subcommands as a JSON catalog object.
fn describe(command: &Command, path: &[String]) -> Value {
    let mut children: Vec<_> =
        if path.len() == 1 { commands().collect() } else { command.get_subcommands().filter(|c| exposed(c)).cloned().collect() };
    children.sort_by(|a, b| a.get_name().cmp(b.get_name()));
    let children: Vec<_> = children
        .into_iter()
        .map(|child| {
            let mut path = path.to_vec();
            path.push(child.get_name().to_string());
            describe(&child, &path)
        })
        .collect();
    let arg_conflicts = conflicts(command);
    let arguments: Vec<_> = command.get_arguments().map(|arg| argument(arg, path, &arg_conflicts)).collect();
    json!({
        "name": command.get_name(),
        "path": path,
        "description": command.get_long_about().or_else(|| command.get_about()).map(ToString::to_string).unwrap_or_default(),
        "aliases": command.get_all_aliases().collect::<Vec<_>>(),
        "arguments": arguments,
        "subcommands": children,
    })
}

/// Determines the schema type string for `arg` within the given command `path`.
fn value_type(arg: &Arg, path: &[String]) -> &'static str {
    let id = arg.get_value_parser().type_id();
    if id == TypeId::of::<bool>()
        || matches!(arg.get_action(), ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version)
    {
        return "boolean";
    }
    if id == TypeId::of::<PathBuf>()
        || matches!(arg.get_value_hint(), clap::ValueHint::AnyPath | clap::ValueHint::FilePath | clap::ValueHint::DirPath)
    {
        return "path";
    }
    if id == TypeId::of::<String>() || id == TypeId::of::<OsString>() {
        // These String operands are dataset locations (local paths, URLs or VSI names).
        // CRS definitions, expressions, field names and NAME=PATH pairs remain strings.
        let location = matches!(
            arg.get_id().as_str(),
            "input"
                | "output"
                | "path"
                | "paths"
                | "source"
                | "destination"
                | "cutline"
                | "like"
                | "mask"
                | "reference"
                | "template"
                | "rasters"
        ) || (arg.get_id() == "inputs" && !path.get(1).is_some_and(|p| p == "calc"))
            || (path.get(1).is_some_and(|p| p == "lineref") && matches!(arg.get_id().as_str(), "lines" | "points" | "parts"));
        return if location { "path" } else { "string" };
    }
    macro_rules! any_type {
        ($($ty:ty),*) => { $(id == TypeId::of::<$ty>())||* };
    }
    if any_type!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize) {
        "integer"
    } else if any_type!(f32, f64) {
        "number"
    } else {
        "unknown"
    }
}

/// Conflicts of each argument of `command`, keyed and listed by the catalog `name` (the clap id).
/// clap records a conflict on one argument only, and exclusive argument groups do not report
/// member-to-member conflicts through `get_arg_conflicts_with`. Both directions and exclusive
/// group pairs are recorded here. Only arguments of this command are listed.
fn conflicts(command: &Command) -> BTreeMap<String, BTreeSet<String>> {
    let own: Vec<&Arg> = command.get_arguments().collect();
    let ids: BTreeSet<&str> = own.iter().map(|arg| arg.get_id().as_str()).collect();
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for arg in &own {
        for other in command.get_arg_conflicts_with(arg) {
            if other.get_id() == arg.get_id() || !ids.contains(other.get_id().as_str()) {
                continue;
            }
            map.entry(arg.get_id().as_str().to_string()).or_default().insert(other.get_id().as_str().to_string());
            map.entry(other.get_id().as_str().to_string()).or_default().insert(arg.get_id().as_str().to_string());
        }
    }
    for group in command.get_groups() {
        if group.clone().is_multiple() {
            continue;
        }
        let members: Vec<&str> = group.get_args().filter(|id| ids.contains(id.as_str())).map(|id| id.as_str()).collect();
        for (i, &a) in members.iter().enumerate() {
            for &b in &members[i + 1..] {
                if a != b {
                    map.entry(a.to_string()).or_default().insert(b.to_string());
                    map.entry(b.to_string()).or_default().insert(a.to_string());
                }
            }
        }
    }
    map
}

/// Builds the catalog argument JSON representation for `arg`.
fn argument(arg: &Arg, path: &[String], conflicts: &BTreeMap<String, BTreeSet<String>>) -> Value {
    let range = arg.get_num_args().unwrap_or_else(|| if arg.get_action().takes_values() { 1.into() } else { 0.into() });
    let max = range.max_values();
    let choices: Vec<_> = if arg.get_action().takes_values() {
        arg.get_value_parser().possible_values().into_iter().flatten().map(|v| v.get_name().to_string()).collect()
    } else {
        Vec::new()
    };
    let action = match arg.get_action() {
        ArgAction::Set => "set",
        ArgAction::Append => "append",
        ArgAction::SetTrue => "set_true",
        ArgAction::SetFalse => "set_false",
        ArgAction::Count => "count",
        ArgAction::Help => "help",
        ArgAction::HelpShort => "help_short",
        ArgAction::HelpLong => "help_long",
        ArgAction::Version => "version",
        _ => "unknown",
    };
    // clap accepts Append and Count again; a second Set, SetTrue or SetFalse is an
    // ArgumentConflict unless args_override_self or overrides_with applies (terrio sets neither).
    let repeatable = matches!(arg.get_action(), ArgAction::Append | ArgAction::Count);
    json!({
        "name": arg.get_id().as_str(),
        "short": arg.get_short().map(|c| c.to_string()),
        "long": arg.get_long(),
        "short_aliases": arg.get_all_short_aliases().unwrap_or_default().into_iter().map(|c| c.to_string()).collect::<Vec<_>>(),
        "long_aliases": arg.get_all_aliases().unwrap_or_default(),
        "position": arg.get_index(),
        "description": arg.get_long_help().or_else(|| arg.get_help()).map(ToString::to_string).unwrap_or_default(),
        "type": value_type(arg, path),
        "default": arg.get_default_values().iter().map(|v| v.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "choices": choices,
        "required": arg.is_required_set(),
        "num_args": {"min": range.min_values(), "max": (max != usize::MAX).then_some(max)},
        "action": action,
        "repeatable": repeatable,
        "global": arg.is_global_set(),
        "hidden": arg.is_hide_set(),
        "value_names": arg.get_value_names().unwrap_or_default().iter().map(ToString::to_string).collect::<Vec<_>>(),
        "value_delimiter": arg.get_value_delimiter().map(|c| c.to_string()),
        "allow_hyphen_values": arg.is_allow_hyphen_values_set(),
        "conflicts_with": conflicts.get(arg.get_id().as_str()).map(|names| names.iter().cloned().collect::<Vec<_>>()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn usage(args: &[&str]) -> Option<Value> {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(crate::STACK_SIZE)
                .spawn_scoped(scope, || requested(&args.iter().map(OsString::from).collect::<Vec<_>>()))
                .unwrap()
                .join()
                .unwrap()
        })
    }

    /// The catalog entry for the argument named `name` in a command's `arguments` array.
    fn catalog_argument<'a>(args: &'a [Value], name: &str) -> &'a Value {
        args.iter().find(|a| a["name"] == name).unwrap()
    }

    #[test]
    fn missing_required_arguments_do_not_prevent_catalogs() {
        for path in [
            vec![],
            vec!["warp"],
            vec!["mosaic"],
            vec!["vsi", "sync"],
            vec!["vsi", "sozip", "create"],
            vec!["filter", "smrf"],
            vec!["lineref", "create"],
        ] {
            let mut args = vec!["terrio"];
            args.extend(path.iter());
            args.push("--json-usage");
            let result = usage(&args).unwrap_or_else(|| panic!("{args:?}"));
            assert_eq!(result["command"]["path"], json!([vec!["terrio"], path].concat()));
        }
    }

    #[test]
    fn literal_values_are_not_metadata_requests() {
        for args in [
            vec!["terrio", "info", "--", "--json-usage"],
            vec!["terrio", "warp", "--srcnodata", "--json-usage"],
            vec!["terrio", "info", "--layer=--json-usage", "file.gpkg"],
        ] {
            assert!(usage(&args).is_none(), "{args:?}");
        }
    }

    #[test]
    fn unknown_selectors_are_not_silently_replaced_by_parent_catalogs() {
        for args in [
            vec!["terrio", "--json-usage", "not-a-command"],
            vec!["terrio", "--json-usage", "vsi", "not-a-command"],
            vec!["terrio", "warp", "--json-usage", "--unknown-option"],
        ] {
            assert!(usage(&args).is_none(), "{args:?}");
        }
    }

    #[test]
    fn catalog_uses_mcp_commands_and_every_leaf_has_arguments() {
        let result = usage(&["terrio", "--json-usage"]).unwrap();
        let top = result["command"]["subcommands"].as_array().unwrap();
        let mut expected: Vec<_> = std::thread::Builder::new()
            .stack_size(crate::STACK_SIZE)
            .spawn(|| commands().map(|c| c.get_name().to_string()).collect::<Vec<_>>())
            .unwrap()
            .join()
            .unwrap();
        expected.sort();
        assert_eq!(top.iter().map(|c| c["name"].as_str().unwrap()).collect::<Vec<_>>(), expected);
        fn check(command: &Value) {
            assert!(!matches!(command["name"].as_str().unwrap(), "help" | "mcp"));
            assert!(command["path"].is_array());
            assert!(command["description"].is_string());
            assert!(!command["arguments"].as_array().unwrap().is_empty());
            for child in command["subcommands"].as_array().unwrap() {
                check(child);
            }
        }
        for command in top {
            check(command);
        }
    }

    #[test]
    fn every_catalog_path_can_be_requested_without_operands() {
        let catalog = usage(&["terrio", "--json-usage"]).unwrap();
        fn paths(command: &Value, out: &mut Vec<Vec<String>>) {
            out.push(command["path"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect());
            for child in command["subcommands"].as_array().unwrap() {
                paths(child, out);
            }
        }
        let mut all = Vec::new();
        paths(&catalog["command"], &mut all);
        for mut path in all {
            let expected = path.clone();
            path.push("--json-usage".into());
            let argv: Vec<_> = path.iter().map(String::as_str).collect();
            assert_eq!(usage(&argv).unwrap_or_else(|| panic!("{argv:?}"))["command"]["path"], json!(expected));
        }
    }

    #[test]
    fn types_defaults_choices_and_arities_follow_clap() {
        let result = usage(&["terrio", "warp", "--json-usage"]).unwrap();
        let args = result["command"]["arguments"].as_array().unwrap();
        assert_eq!(catalog_argument(args, "paths")["type"], "path");
        assert_eq!(catalog_argument(args, "paths")["num_args"], json!({"min":2,"max":null}));
        assert_eq!(catalog_argument(args, "paths")["required"], true);
        assert_eq!(catalog_argument(args, "tr")["type"], "number");
        assert_eq!(catalog_argument(args, "tr")["num_args"], json!({"min":2,"max":2}));
        assert_eq!(catalog_argument(args, "ts")["type"], "integer");
        assert_eq!(catalog_argument(args, "tap")["type"], "boolean");
        assert_eq!(catalog_argument(args, "tap")["default"], json!(["false"]));
        assert_eq!(catalog_argument(args, "tap")["choices"], json!([]));
        assert_eq!(catalog_argument(args, "resampling")["default"], json!(["near"]));
        let result = usage(&["terrio", "validate-cog", "--json-usage"]).unwrap();
        let profile = result["command"]["arguments"].as_array().unwrap().iter().find(|a| a["name"] == "profile").unwrap();
        assert_eq!(profile["choices"], json!(["rio-cogeo", "gdal"]));
    }

    #[test]
    fn repeatable_follows_clap_occurrence_rules() {
        let result = usage(&["terrio", "warp", "--json-usage"]).unwrap();
        let args = result["command"]["arguments"].as_array().unwrap();
        // `--tr` is Append with two values per occurrence: clap keeps every occurrence.
        assert_eq!(catalog_argument(args, "tr")["action"], "append");
        assert_eq!(catalog_argument(args, "tr")["num_args"], json!({"min":2,"max":2}));
        assert_eq!(catalog_argument(args, "tr")["repeatable"], true);
        assert_eq!(catalog_argument(args, "paths")["repeatable"], true);
        // A second Set or SetTrue occurrence is an ArgumentConflict in clap.
        assert_eq!(catalog_argument(args, "srcnodata")["action"], "set");
        assert_eq!(catalog_argument(args, "srcnodata")["repeatable"], false);
        assert_eq!(catalog_argument(args, "tap")["action"], "set_true");
        assert_eq!(catalog_argument(args, "tap")["repeatable"], false);
    }

    #[test]
    fn conflicts_with_lists_both_sides_of_a_declared_pair() {
        // `ts` declares `conflicts_with = "tr"` in clap; `like` declares `conflicts_with_all`
        // over tr, ts and te. The catalog lists both sides of each declared pair.
        let result = usage(&["terrio", "rasterize", "--json-usage"]).unwrap();
        let args = result["command"]["arguments"].as_array().unwrap();
        assert_eq!(catalog_argument(args, "ts")["conflicts_with"], json!(["like", "tr"]));
        assert_eq!(catalog_argument(args, "tr")["conflicts_with"], json!(["like", "ts"]));
        // `tap` declares only `requires = "tr"`; requires is deferred, so it lists no conflicts.
        assert_eq!(catalog_argument(args, "tap")["conflicts_with"], json!([]));

        // `vector-clip` groups `bbox`, `geometry` and `like` into an exclusive ArgGroup:
        // members mutually conflict pairwise even though no direct `conflicts_with` is declared on each Arg.
        let result = usage(&["terrio", "vector-clip", "--json-usage"]).unwrap();
        let args = result["command"]["arguments"].as_array().unwrap();
        assert_eq!(catalog_argument(args, "bbox")["conflicts_with"], json!(["geometry", "like"]));
        assert_eq!(catalog_argument(args, "geometry")["conflicts_with"], json!(["bbox", "like"]));
        assert_eq!(catalog_argument(args, "like")["conflicts_with"], json!(["bbox", "geometry"]));
    }

    #[test]
    fn conflicts_with_is_symmetric_and_names_existing_arguments() {
        fn check(command: &Value) {
            let args = command["arguments"].as_array().unwrap();
            let identifier = |a: &Value| a["name"].as_str().unwrap().to_string();
            for a in args {
                for other in a["conflicts_with"].as_array().unwrap() {
                    let other = other.as_str().unwrap();
                    let target = args
                        .iter()
                        .find(|b| identifier(b) == other)
                        .unwrap_or_else(|| panic!("{} conflicts with unknown {other}", command["path"]));
                    let back: Vec<_> = target["conflicts_with"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
                    assert!(back.contains(&identifier(a).as_str()), "{other} does not list {} back", identifier(a));
                }
            }
            for child in command["subcommands"].as_array().unwrap() {
                check(child);
            }
        }
        let result = usage(&["terrio", "--json-usage"]).unwrap();
        check(&result["command"]);
    }
}
