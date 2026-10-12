//! Executable JSON tables and read-only Markdown substitution. These commands
//! are independent of semantic cases, verdicts, review ledgers and digests.
use anyhow::{Context, Result, ensure};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::project::atomic_write;

/// Resolve a row-name → column-name → scalar JSON template. `$ ` cells execute
/// once per distinct command in the template's directory. Direct hyperfine
/// commands use a fresh export and select `pointer`, multiplied by `scale`;
/// other trusted Bash cells must print one JSON scalar. A `$ ` row command
/// prints a nonempty object of scalar columns and replaces that entire row.
/// Returned command strings are rejected, never recursively executed. Results are published
/// atomically only after all commands succeed; a failed run leaves no old result.
/// Raw exports and cell-to-command mappings are preserved in INPUT.bench.json.
pub fn bench(input: &Path, pointer: &str, scale: f64) -> Result<PathBuf> {
    bench_at(input, pointer, scale, None)
}

/// Resolve a template with an optional explicit command working directory.
/// Hyperfine command cells may end in `| json POINTER [* SCALE]`; selections
/// share one execution of the same command. Per-command logs are retained in
/// INPUT.bench.logs and exposed to hooks as SEMSPEC_BENCH_LOG.
pub fn bench_at(input: &Path, pointer: &str, scale: f64, cwd: Option<&Path>) -> Result<PathBuf> {
    ensure!(scale.is_finite(), "scale must be finite");
    validate_pointer(pointer)?;
    let input = input.canonicalize().context("read benchmark template")?;
    ensure!(
        input.extension().is_some_and(|ext| ext == "json"),
        "bench input must be a .json file"
    );
    let output = input.with_extension("render.json");
    let evidence = input.with_extension("bench.json");
    let mut table: Value = serde_json::from_slice(&fs::read(&input)?)?;
    validate_table(&table, true)?;
    let cwd = cwd
        .unwrap_or(input.parent().unwrap())
        .canonicalize()
        .context("command cwd must exist")?;
    ensure!(cwd.is_dir(), "command cwd must be a directory");
    let protected = [&input, &output, &evidence];
    let mut plans = HashMap::new();
    let mut row_shapes = HashMap::new();
    let mut export_paths = Vec::new();
    // Validate every command before clearing an existing result or running code.
    for row in table.as_object().unwrap().values() {
        let values = if let Some(row) = row.as_object() {
            row.values().collect::<Vec<_>>()
        } else {
            vec![row]
        };
        for cell in values {
            if let Some(command) = cell.as_str().and_then(|s| s.strip_prefix("$ ")) {
                let is_row = !row.is_object();
                if let Some(previous) = row_shapes.insert(command.to_owned(), is_row) {
                    ensure!(
                        previous == is_row,
                        "one script command cannot produce both a row object and a scalar cell"
                    );
                }
                if plans.contains_key(command) {
                    continue;
                }
                let plan = plan(command, &cwd, &protected)?;
                ensure!(
                    row.is_object() || plan.pointer.is_none() && plan.scale.is_none(),
                    "hyperfine row commands cannot select a scalar pointer or scale"
                );
                if let Some(path) = &plan.export {
                    ensure!(
                        !export_paths
                            .iter()
                            .any(|(existing, key)| existing == path && key != &plan.key),
                        "different commands share export file {}",
                        path.display()
                    );
                    export_paths.push((path.clone(), plan.key.clone()));
                }
                plans.insert(command.to_owned(), plan);
            }
        }
    }
    for path in [&output, &evidence] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let mut cache = HashMap::<String, Value>::new();
    let log_dir = input.with_extension("bench.logs");
    fs::create_dir_all(&log_dir)?;
    let mut records = Vec::new();
    let mut cells = Vec::new();
    let mut rows = Vec::new();
    for (row_name, row) in table.as_object_mut().unwrap() {
        if let Some(command) = row
            .as_str()
            .and_then(|s| s.strip_prefix("$ "))
            .map(str::to_owned)
        {
            let plan = &plans[&command];
            let raw = if let Some(raw) = cache.get(&plan.key) {
                raw.clone()
            } else {
                eprintln!("bench: {row_name}: {}", plan.key);
                let log = log_dir.join(format!("{:04}.log", records.len() + 1));
                let raw = execute(plan, &cwd, &log)
                    .with_context(|| format!("benchmark row {row_name:?}"))?;
                let value = row_value(&raw, plan)?;
                records.push(json!({"command": plan.key, "kind": if plan.shell.is_some() {"script-row"} else {"hyperfine-row"}, "raw": raw, "value": value, "log": log}));
                cache.insert(plan.key.clone(), raw.clone());
                raw
            };
            let raw = row_value(&raw, plan)?;
            // Validate as data before insertion; emitted commands are not plans.
            validate_table(&json!({row_name: raw}), false)
                .with_context(|| format!("script output for row {row_name:?}"))?;
            rows.push(
                json!({"row": row_name, "command": plan.key, "expression": command,
                "columns": raw.as_object().unwrap().keys().collect::<Vec<_>>()}),
            );
            *row = raw;
            continue;
        }
        for (column, cell) in row.as_object_mut().unwrap() {
            let Some(command) = cell
                .as_str()
                .and_then(|s| s.strip_prefix("$ "))
                .map(str::to_owned)
            else {
                continue;
            };
            let plan = &plans[&command];
            let raw = if let Some(raw) = cache.get(&plan.key) {
                raw.clone()
            } else {
                eprintln!("bench: {row_name}: {}", plan.key);
                let log = log_dir.join(format!("{:04}.log", records.len() + 1));
                let raw = execute(plan, &cwd, &log)
                    .with_context(|| format!("benchmark cell {row_name:?} / {column:?}"))?;
                let value = select(&raw, plan, pointer, scale)?;
                records.push(json!({"command": plan.key, "kind": if plan.shell.is_some() {"script-cell"} else {"hyperfine"}, "raw": raw, "value": value, "log": log}));
                cache.insert(plan.key.clone(), raw.clone());
                raw
            };
            *cell = select(&raw, plan, pointer, scale)?;
            cells.push(
                json!({"row": row_name, "column": column, "command": plan.key,
                "expression": command, "pointer": plan.pointer.as_deref().unwrap_or(pointer),
                "scale": plan.scale.unwrap_or(scale)}),
            );
        }
    }
    // Script rows may determine the schema only at runtime. Reject mismatches
    // before either result or evidence is published.
    validate_table(&table, false)?;
    atomic_write(
        &evidence,
        &json_bytes(&json!({
            "format": 1, "template": input, "cwd": cwd, "pointer": pointer, "scale": scale,
            "commands": records, "cells": cells, "rows": rows
        }))?,
    )?;
    atomic_write(&output, &json_bytes(&table)?)?;
    Ok(output)
}

fn row_value(raw: &Value, plan: &Plan) -> Result<Value> {
    if plan.shell.is_some() {
        return Ok(raw.clone());
    }
    validate_export(raw, "")?;
    let result = &raw["results"][0];
    let v2 = raw["schema_version"].as_u64() == Some(2);
    let count = if v2 {
        let summary = &result["summary"]["time_wall_clock"];
        ensure!(
            summary["unit"] == "second",
            "hyperfine wall time must be in seconds"
        );
        let count = summary["count"].as_u64().context("missing sample count")?;
        ensure!(
            Some(count as usize) == result["measurements"].as_array().map(Vec::len),
            "hyperfine summary count differs from samples"
        );
        count
    } else {
        result["times"].as_array().context("missing times")?.len() as u64
    };
    let mut columns = Map::new();
    columns.insert("N".into(), json!(count));
    for (column, field) in [
        ("Median ms", "median"),
        ("Mean ms", "mean"),
        ("SD ms", "stddev"),
        ("Min ms", "min"),
        ("Max ms", "max"),
    ] {
        let value = if v2 {
            &result["summary"]["time_wall_clock"][field]
        } else {
            &result[field]
        };
        let ms = value
            .as_f64()
            .with_context(|| format!("missing numeric hyperfine {field}"))?
            * 1000.0;
        ensure!(ms.is_finite() && ms >= 0.0, "invalid hyperfine statistic");
        columns.insert(column.into(), json!(ms));
    }
    Ok(Value::Object(columns))
}

fn validate_pointer(pointer: &str) -> Result<()> {
    ensure!(
        pointer.is_empty() || pointer.starts_with('/'),
        "pointer must be an RFC 6901 JSON Pointer"
    );
    for token in pointer.split('/').skip(1) {
        let mut chars = token.chars();
        while let Some(c) = chars.next() {
            if c == '~' {
                ensure!(
                    matches!(chars.next(), Some('0' | '1')),
                    "invalid JSON Pointer escape"
                );
            }
        }
    }
    Ok(())
}

struct Plan {
    key: String,
    argv: Vec<String>,
    shell: Option<String>,
    export: Option<PathBuf>,
    pointer: Option<String>,
    scale: Option<f64>,
}

fn plan(command: &str, cwd: &Path, protected: &[&PathBuf]) -> Result<Plan> {
    ensure!(!command.trim().is_empty(), "empty command cell");
    let mut argv = shell_words::split(command).context("invalid command quoting")?;
    let hyperfine = argv.first().is_some_and(|s| {
        Path::new(s)
            .file_name()
            .is_some_and(|name| name == "hyperfine")
    });
    if !hyperfine {
        return Ok(Plan {
            key: command.to_owned(),
            argv: Vec::new(),
            shell: Some(command.to_owned()),
            export: None,
            pointer: None,
            scale: None,
        });
    }
    let mut pointer = None;
    let mut scale = None;
    if let Some(index) = argv.iter().position(|arg| arg == "|") {
        let suffix = argv.split_off(index);
        ensure!(
            matches!(suffix.len(), 3 | 5) && suffix[1] == "json",
            "expected | json POINTER [* SCALE]"
        );
        let selection = &suffix[2];
        validate_pointer(selection)?;
        pointer = Some(selection.clone());
        if suffix.len() == 5 {
            ensure!(suffix[3] == "*", "expected | json POINTER * SCALE");
            let factor: f64 = suffix[4].parse().context("invalid cell scale")?;
            ensure!(factor.is_finite(), "cell scale must be finite");
            scale = Some(factor);
        }
    }
    let key = shell_words::join(&argv);
    // Shell operators outside the measured command are not argv parameters.
    ensure!(
        !argv
            .iter()
            .any(|a| matches!(a.as_str(), "|" | "&&" | ";" | ">" | "2>")),
        "use a direct hyperfine command; shell pipelines can be placed inside an explicit Bash command cell"
    );
    ensure!(
        !argv
            .iter()
            .any(|a| a == "--ignore-failure" || a.starts_with("--ignore-failure=") || a == "-i"),
        "hyperfine --ignore-failure is not allowed"
    );
    let mut export = None;
    let mut index = 1;
    while index < argv.len() {
        let path = if argv[index] == "--export-json" {
            ensure!(index + 1 < argv.len(), "--export-json needs a path");
            let path = argv.remove(index + 1);
            argv.remove(index);
            Some(path)
        } else if let Some(path) = argv[index].strip_prefix("--export-json=") {
            let path = path.to_owned();
            argv.remove(index);
            Some(path)
        } else {
            index += 1;
            None
        };
        if let Some(path) = path {
            ensure!(export.is_none(), "only one --export-json is allowed");
            let path = cwd.join(path);
            let parent = path
                .parent()
                .context("export file requires a parent directory")?
                .canonicalize()
                .context("export parent must exist")?;
            let path = parent.join(path.file_name().context("export requires a filename")?);
            ensure!(
                !protected.iter().any(|p| path == **p),
                "export must not overwrite template or generated results"
            );
            ensure!(
                !path.is_symlink() && (!path.exists() || path.is_file()),
                "export must be a regular file"
            );
            export = Some(path);
        }
    }
    Ok(Plan {
        key,
        argv,
        shell: None,
        export,
        pointer,
        scale,
    })
}

fn execute(plan: &Plan, cwd: &Path, log: &Path) -> Result<Value> {
    let temporary = tempfile::tempdir()?;
    let raw_path = temporary.path().join("hyperfine.json");
    let mut process = if let Some(shell) = &plan.shell {
        let mut command = Command::new("bash");
        command.args(["-euo", "pipefail", "-c", shell]);
        command
    } else {
        let mut command = Command::new(&plan.argv[0]);
        command
            .arg("--export-json")
            .arg(&raw_path)
            .args(&plan.argv[1..]);
        command
    };
    process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env("SEMSPEC_BENCH_LOG", log);
    if plan.shell.is_some() {
        let output = process.output().context("launch command")?;
        fs::write(
            log,
            [output.stdout.as_slice(), output.stderr.as_slice()].concat(),
        )?;
        ensure!(
            output.status.success(),
            "command exited {}; see {}",
            output.status,
            log.display()
        );
        let value: Value = serde_json::from_slice(&output.stdout)
            .context("command stdout must be one JSON value")?;
        return Ok(value);
    }
    let stream = fs::File::create(log)?;
    let status = process
        .stdout(stream.try_clone()?)
        .stderr(stream)
        .status()
        .context("launch hyperfine")?;
    ensure!(
        status.success(),
        "hyperfine exited {status}; see {}",
        log.display()
    );
    let raw: Value = serde_json::from_slice(
        &fs::read(&raw_path).context("hyperfine did not write its JSON export")?,
    )?;
    let results = raw["results"]
        .as_array()
        .context("hyperfine export has no results")?;
    ensure!(
        results.len() == 1,
        "one benchmark cell must measure exactly one hyperfine command"
    );
    validate_export(&raw, "")?;
    if let Some(path) = &plan.export {
        atomic_write(path, &json_bytes(&raw)?)?;
    }
    Ok(raw)
}

fn select(raw: &Value, plan: &Plan, pointer: &str, scale: f64) -> Result<Value> {
    if plan.shell.is_some() {
        ensure!(
            is_scalar(raw) && !raw.as_str().is_some_and(|s| s.starts_with("$ ")),
            "cell command must produce a resolved JSON scalar"
        );
        return Ok(raw.clone());
    }
    let pointer = plan.pointer.as_deref().unwrap_or(pointer);
    let scale = plan.scale.unwrap_or(scale);
    let effective_pointer = validate_export(raw, pointer)?;
    let selected = raw
        .pointer(&effective_pointer)
        .with_context(|| format!("missing hyperfine pointer {pointer}"))?;
    let value = selected
        .as_f64()
        .context("selected hyperfine field must be numeric")?;
    let value = value * scale;
    ensure!(value.is_finite(), "scaled hyperfine result must be finite");
    Ok(if scale == 1.0 {
        selected.clone()
    } else {
        json!(value)
    })
}

fn validate_export(raw: &Value, pointer: &str) -> Result<String> {
    let result = &raw["results"][0];
    if raw
        .get("schema_version")
        .is_some_and(|v| v.as_u64() == Some(2))
    {
        let measurements = result["measurements"]
            .as_array()
            .context("hyperfine v2 export has no measurements")?;
        ensure!(!measurements.is_empty(), "hyperfine export has no samples");
        ensure!(
            measurements
                .iter()
                .all(|m| m["exit_code"].as_i64() == Some(0)),
            "hyperfine contains failed or incomplete samples"
        );
        if let Some(field) = pointer.strip_prefix("/results/0/")
            && matches!(field, "mean" | "median" | "stddev" | "min" | "max")
        {
            ensure!(
                result["summary"]["time_wall_clock"]["unit"] == "second",
                "hyperfine wall time must be in seconds"
            );
            return Ok(format!("/results/0/summary/time_wall_clock/{field}"));
        }
    } else {
        ensure!(
            raw.get("schema_version")
                .is_none_or(|v| v.as_u64() == Some(1)),
            "unsupported hyperfine JSON schema version"
        );
        let times = result["times"]
            .as_array()
            .context("hyperfine export has no times")?;
        ensure!(!times.is_empty(), "hyperfine export has no samples");
        // Hyperfine 1.x may omit exit_codes; successful hyperfine without
        // --ignore-failure then establishes successful subjects.
        if let Some(exits) = result.get("exit_codes") {
            let exits = exits
                .as_array()
                .context("hyperfine exit_codes must be an array")?;
            ensure!(
                exits.len() == times.len() && exits.iter().all(|code| code.as_i64() == Some(0)),
                "hyperfine contains failed or incomplete samples"
            );
        }
    }
    Ok(pointer.to_owned())
}

fn is_scalar(value: &Value) -> bool {
    !value.is_object() && !value.is_array()
}

fn validate_table(table: &Value, commands: bool) -> Result<()> {
    let rows = table
        .as_object()
        .context("table must be an object of named rows")?;
    ensure!(!rows.is_empty(), "table must have at least one row");
    let columns = rows.values().find_map(Value::as_object);
    for (name, row) in rows {
        if row.as_str().is_some_and(|s| s.starts_with("$ ")) {
            ensure!(
                commands,
                "unresolved row command at {name:?}; run semspec bench first"
            );
            continue;
        }
        let row = row
            .as_object()
            .with_context(|| format!("row {name:?} must be an object"))?;
        ensure!(!row.is_empty(), "table must have at least one column");
        let columns = columns.unwrap();
        ensure!(
            row.len() == columns.len() && columns.keys().all(|key| row.contains_key(key)),
            "row {name:?} has different columns"
        );
        for (column, value) in row {
            ensure!(
                is_scalar(value),
                "cell {name:?} / {column:?} must be a scalar"
            );
            ensure!(
                commands || !value.as_str().is_some_and(|s| s.starts_with("$ ")),
                "unresolved command at {name:?} / {column:?}; run semspec bench first"
            );
        }
    }
    Ok(())
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Substitute standalone `{{ table("relative.json") }}` lines with Markdown
/// tables. JSON paths are relative to `input`; code blocks and inline examples
/// remain literal. This function only reads files and never executes commands.
/// Rows and columns retain JSON insertion order. In strict mode, table errors
/// fail the render. With `best_effort`, each failed placeholder becomes a visible
/// warning, diagnostics go to stderr and other tables continue to render.
pub fn render(source: &str, input: &Path, best_effort: bool) -> Result<String> {
    let mut protected = Vec::new();
    let mut start = None;
    for (event, range) in Parser::new(source).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => start = Some(range.start),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = start.take() {
                    protected.push(start..range.end);
                }
            }
            Event::Code(_) => protected.push(range),
            _ => {}
        }
    }
    let mut text = String::new();
    let mut offset = 0;
    for (index, line) in source.split_inclusive('\n').enumerate() {
        let trimmed = line.trim();
        if !protected
            .iter()
            .any(|r| r.start < offset + line.len() && offset < r.end)
            && trimmed.starts_with("{{ table(")
        {
            let rendered = match render_placeholder(trimmed, input) {
                Ok(rendered) => rendered,
                Err(error) if best_effort => {
                    eprintln!(
                        "semspec: warning: {}:{}: {error:#}",
                        input.display(),
                        index + 1
                    );
                    format!(
                        "!!! warning \"Table unavailable during live preview\"\n\n    {}",
                        escape(&format!("{error:#}"))
                    )
                }
                Err(error) => return Err(error),
            };
            // Blank lines separate a table from adjacent paragraphs.
            if !text.is_empty() && !text.ends_with("\n\n") {
                text.push('\n');
            }
            let indent = &line[..line.len() - line.trim_start().len()];
            for line in rendered.lines() {
                text.push_str(indent);
                text.push_str(line);
                text.push('\n');
            }
            text.push('\n');
        } else {
            text.push_str(line);
        }
        offset += line.len();
    }
    Ok(text)
}

fn render_placeholder(placeholder: &str, input: &Path) -> Result<String> {
    let path = placeholder
        .strip_prefix("{{ table(")
        .unwrap()
        .strip_suffix(") }}")
        .context("table placeholder must be {{ table(\"file.json\") }} on its own line")?;
    let args: Vec<Value> = serde_json::from_str(&format!("[{path}]"))
        .context("expected a JSON filename and optional table options")?;
    ensure!(
        matches!(args.len(), 1 | 2),
        "expected filename and optional table options"
    );
    let path = args[0]
        .as_str()
        .context("table path must be a JSON string")?;
    let options: TableOptions = if args.len() == 2 {
        serde_json::from_value(args[1].clone()).context("invalid table options")?
    } else {
        TableOptions::default()
    };
    let file = input.parent().unwrap_or(Path::new(".")).join(path);
    let table: Value = serde_json::from_slice(
        &fs::read(&file).with_context(|| format!("read table {}", file.display()))?,
    )?;
    validate_table(&table, false).with_context(|| format!("table {}", file.display()))?;
    markdown_table(table.as_object().unwrap(), &options)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\\', "&#92;")
        .replace('|', "&#124;")
        .replace('`', "&#96;")
        .replace('*', "&#42;")
        .replace('_', "&#95;")
        .replace('[', "&#91;")
        .replace(']', "&#93;")
        .replace("\r\n", "\n")
        .replace(['\n', '\r'], "<br>")
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TableOptions {
    row_names: bool,
    markdown: bool,
    align: Option<Vec<Alignment>>,
    headers: Option<Vec<String>>,
}

impl Default for TableOptions {
    fn default() -> Self {
        Self {
            row_names: true,
            markdown: false,
            align: None,
            headers: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Alignment {
    Left,
    Right,
    Center,
    Default,
}

fn table_text(text: &str, markdown: bool) -> String {
    if !markdown {
        return escape(text);
    }
    // Trusted article markup is opt-in. Escape table delimiters even inside
    // code spans; preserve existing escaped pipes and all other Markdown.
    let mut output = String::new();
    let mut slashes = 0;
    for c in text.replace("\r\n", "\n").chars() {
        if c == '|' && slashes % 2 == 0 {
            output.push('\\');
        }
        match c {
            '\n' | '\r' => output.push_str("<br>"),
            _ => output.push(c),
        }
        slashes = if c == '\\' { slashes + 1 } else { 0 };
    }
    output
}

fn markdown_table(rows: &Map<String, Value>, options: &TableOptions) -> Result<String> {
    let columns = rows.values().next().unwrap().as_object().unwrap();
    let count = columns.len() + usize::from(options.row_names);
    if let Some(align) = &options.align {
        ensure!(
            align.len() == count,
            "alignment count must match rendered columns"
        );
    }
    let mut headers = Vec::new();
    if options.row_names {
        headers.push(String::new());
    }
    headers.extend(columns.keys().map(|key| table_text(key, options.markdown)));
    if let Some(labels) = &options.headers {
        ensure!(
            labels.len() == count,
            "header count must match rendered columns"
        );
        headers = labels
            .iter()
            .map(|label| table_text(label, options.markdown))
            .collect();
    }
    let separators: Vec<_> = (0..count)
        .map(|i| match options.align.as_ref().map(|a| &a[i]) {
            Some(Alignment::Left) => ":---",
            Some(Alignment::Right) => "---:",
            Some(Alignment::Center) => ":---:",
            _ => "---",
        })
        .collect();
    let mut text = format!(
        "| {} |\n| {} |\n",
        headers.join(" | "),
        separators.join(" | ")
    );
    for (name, row) in rows {
        let mut cells = Vec::new();
        if options.row_names {
            cells.push(table_text(name, options.markdown));
        }
        cells.extend(columns.keys().map(|key| match &row[key] {
            Value::String(s) => table_text(s, options.markdown),
            Value::Null => "—".into(),
            value => value.to_string(),
        }));
        text.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    Ok(text)
}
