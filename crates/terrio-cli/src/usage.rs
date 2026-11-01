//! Versioned, metadata-only command catalog built from the same clap tree as MCP.

use clap::{Arg, ArgAction, Command, CommandFactory};
use serde_json::{Value, json};
use std::any::TypeId;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

fn root() -> Command {
    let mut command = crate::Cli::command();
    command.build();
    command
}

fn exposed(command: &Command) -> bool {
    !matches!(command.get_name(), "mcp" | "help")
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
        "schema_version": "0.1",
        "unstable": true,
        "program": "terrio",
        "version": env!("CARGO_PKG_VERSION"),
        "command": describe(command, &path),
    }))
}

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
    let arguments: Vec<_> = command.get_arguments().map(|arg| argument(arg, path)).collect();
    json!({
        "name": command.get_name(),
        "path": path,
        "description": command.get_long_about().or_else(|| command.get_about()).map(ToString::to_string).unwrap_or_default(),
        "aliases": command.get_all_aliases().collect::<Vec<_>>(),
        "arguments": arguments,
        "subcommands": children,
    })
}

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

fn argument(arg: &Arg, path: &[String]) -> Value {
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
        "global": arg.is_global_set(),
        "value_names": arg.get_value_names().unwrap_or_default().iter().map(ToString::to_string).collect::<Vec<_>>(),
        "value_delimiter": arg.get_value_delimiter().map(|c| c.to_string()),
        "allow_hyphen_values": arg.is_allow_hyphen_values_set(),
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
        let arg = |name: &str| args.iter().find(|a| a["name"] == name).unwrap();
        assert_eq!(arg("paths")["type"], "path");
        assert_eq!(arg("paths")["num_args"], json!({"min":2,"max":null}));
        assert_eq!(arg("paths")["required"], true);
        assert_eq!(arg("tr")["type"], "number");
        assert_eq!(arg("tr")["num_args"], json!({"min":2,"max":2}));
        assert_eq!(arg("ts")["type"], "integer");
        assert_eq!(arg("tap")["type"], "boolean");
        assert_eq!(arg("tap")["default"], json!(["false"]));
        assert_eq!(arg("tap")["choices"], json!([]));
        assert_eq!(arg("resampling")["default"], json!(["near"]));
        let result = usage(&["terrio", "validate-cog", "--json-usage"]).unwrap();
        let profile = result["command"]["arguments"].as_array().unwrap().iter().find(|a| a["name"] == "profile").unwrap();
        assert_eq!(profile["choices"], json!(["rio-cogeo", "gdal"]));
    }
}
