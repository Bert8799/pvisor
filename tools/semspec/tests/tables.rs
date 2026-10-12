use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_semspec"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn fake_hyperfine(root: &Path) -> String {
    let path = root.join("hyperfine");
    fs::write(&path, r#"#!/bin/bash
set -euo pipefail
test -f fixture
printf 'run\n' >> calls
test "$1" = --export-json
export_file=$2
shift 2
if [[ -f fail ]]; then exit 9; fi
if [[ -f schema.json ]]; then
  cat schema.json > "$export_file"
else
  printf '%s' '{"results":[{"command":"/bin/true","median":0.002,"mean":0.003,"stddev":0.001,"min":0.001,"max":0.004,"times":[0.002,0.002],"exit_codes":[0,0]}]}' > "$export_file"
fi
printf 'hyperfine progress is not JSON\n'
"#).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    shell_words::quote(path.to_str().unwrap()).into_owned()
}

#[test]
fn bench_maps_cells_to_one_fresh_hyperfine_export_and_keeps_order() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("data")).unwrap();
    let data = root.join("data");
    fs::write(data.join("fixture"), "ready").unwrap();
    let command = format!(
        "$ {} --shell=none --runs 2 --export-json raw.json '/bin/true'",
        fake_hyperfine(&data)
    );
    fs::write(data.join("in.json"), format!(r#"{{"Z native":{{"median ms":{command:?},"N":2}},"A native":{{"median ms":{command:?},"N":2}}}}"#)).unwrap();
    fs::write(data.join("raw.json"), "stale").unwrap();
    let output = cli(root, &["bench", "data/in.json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered: Value =
        serde_json::from_slice(&fs::read(data.join("in.render.json")).unwrap()).unwrap();
    assert_eq!(rendered["Z native"]["median ms"], 2.0);
    assert_eq!(rendered["A native"]["N"], 2);
    assert_eq!(
        rendered.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["Z native", "A native"]
    );
    assert_eq!(fs::read_to_string(data.join("calls")).unwrap(), "run\n");
    let evidence: Value =
        serde_json::from_slice(&fs::read(data.join("in.bench.json")).unwrap()).unwrap();
    assert_eq!(evidence["commands"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["cells"].as_array().unwrap().len(), 2);
    assert_eq!(
        evidence["commands"][0]["raw"]["results"][0]["times"],
        json!([0.002, 0.002])
    );
    assert!(
        fs::read_to_string(data.join("raw.json"))
            .unwrap()
            .trim()
            .starts_with('{')
    );
    let output = cli(
        root,
        &[
            "bench",
            "data/in.json",
            "--pointer",
            "/results/0/mean",
            "--scale",
            "1",
        ],
    );
    assert!(output.status.success());
    let rendered: Value =
        serde_json::from_slice(&fs::read(data.join("in.render.json")).unwrap()).unwrap();
    assert_eq!(rendered["Z native"]["median ms"], 0.003);
    fs::write(data.join("fail"), "").unwrap();
    assert_eq!(cli(root, &["bench", "data/in.json"]).status.code(), Some(2));
    assert!(!data.join("in.render.json").exists());
    assert!(!data.join("in.bench.json").exists());
}

#[test]
fn bench_supports_json_scalar_commands_and_rejects_invalid_templates_before_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        &root.join("in.json"),
        &json!({"native": {"value": "$ printf '12.5'", "note": "literal"}}),
    );
    assert!(cli(root, &["bench", "in.json"]).status.success());
    let rendered: Value =
        serde_json::from_slice(&fs::read(root.join("in.render.json")).unwrap()).unwrap();
    assert_eq!(rendered["native"]["value"], 12.5);
    write(
        &root.join("in.json"),
        &json!({"native": {"value": "$ touch executed; printf 1"}, "other": {"wrong": 1}}),
    );
    assert_eq!(cli(root, &["bench", "in.json"]).status.code(), Some(2));
    assert!(!root.join("executed").exists());
    assert!(root.join("in.render.json").exists());
    for command in [
        "$ printf not-json",
        "$ false",
        "$ printf '[1]'",
        "$ printf '\"$ unresolved\"'",
        "$ printf 1; false",
    ] {
        write(
            &root.join("in.json"),
            &json!({"native": {"value": command}}),
        );
        assert_eq!(
            cli(root, &["bench", "in.json"]).status.code(),
            Some(2),
            "{command}"
        );
        assert!(!root.join("in.render.json").exists());
    }
}

#[test]
fn hyperfine_cannot_overwrite_templates_or_ignore_subject_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for args in [
        "--export-json in.json",
        "--export-json in.render.json",
        "--ignore-failure",
        "--export-json",
        "--export-json in.bench.json",
    ] {
        write(
            &root.join("in.json"),
            &json!({"native": {"value": format!("$ hyperfine {args} '/bin/true'")}}),
        );
        let source = fs::read(root.join("in.json")).unwrap();
        assert_eq!(cli(root, &["bench", "in.json"]).status.code(), Some(2));
        assert_eq!(fs::read(root.join("in.json")).unwrap(), source);
    }
}

#[test]
fn render_is_read_only_resolves_relative_json_and_preserves_code_examples() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("data")).unwrap();
    write(
        &root.join("data/in.render.json"),
        &json!({"Z|native": {"median ms": 2.13, "notes": "<tag>\na|b"}, "A": {"median ms": Value::Null, "notes": "plain"}}),
    );
    let source = "# Data\n\n{{ table(\"in.render.json\") }}\n\n```markdown\n{{ table(\"missing.json\") }}\n```\n\n    {{ table(\"missing.json\") }}\n\n`{{ table(\"missing.json\") }}`\n";
    fs::write(root.join("data/page.md"), source).unwrap();
    let output = cli(root, &["render", "data/page.md"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8(output.stdout).unwrap();
    assert!(
        rendered.contains("| Z&#124;native | 2.13 | &lt;tag&gt;<br>a&#124;b |"),
        "{rendered}"
    );
    assert!(rendered.contains("| A | — | plain |"));
    assert!(rendered.contains("```markdown\n{{ table(\"missing.json\") }}\n```"));
    assert!(rendered.contains("    {{ table(\"missing.json\") }}"));
    assert_eq!(
        fs::read_to_string(root.join("data/page.md")).unwrap(),
        source
    );
    assert_eq!(
        cli(
            root,
            &["render", "data/page.md", "--output", "data/page.md"]
        )
        .status
        .code(),
        Some(2)
    );
    assert!(
        cli(root, &["render", "data/page.md", "--output", "result.md"])
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(root.join("result.md")).unwrap(),
        rendered
    );
}

#[test]
fn render_refuses_unresolved_commands_and_missing_or_malformed_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("page.md"), "{{ table(\"in.json\") }}\n").unwrap();
    assert_eq!(cli(root, &["render", "page.md"]).status.code(), Some(2));
    write(
        &root.join("in.json"),
        &json!({"row": {"value": "$ touch executed; echo 1"}}),
    );
    assert_eq!(cli(root, &["render", "page.md"]).status.code(), Some(2));
    assert!(!root.join("executed").exists());
    for value in [
        json!({}),
        json!([]),
        json!({"row": {"x": [1]}}),
        json!({"row": {"x": 1}, "other": {"y": 2}}),
    ] {
        write(&root.join("in.json"), &value);
        assert_eq!(cli(root, &["render", "page.md"]).status.code(), Some(2));
    }
}

#[test]
fn live_render_keeps_valid_tables_and_recovers_after_a_missing_table_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("good.json"), &json!({"native": {"ms": 2}}));
    fs::write(root.join("bad.json"), "not json").unwrap();
    write(
        &root.join("command.json"),
        &json!({"row": {"x": "$ touch executed"}}),
    );
    let source = "# Preview\n\n{{ table(\"missing.json\") }}\n\n{{ table(\"good.json\") }}\n\n{{ table(\"bad.json\") }}\n\n{{ table(\"good.json\", {\"wrong\":true}) }}\n\n{{ table(\"command.json\") }}\n\n{{ table(invalid) }}\n\n```markdown\n{{ table(\"example.json\") }}\n```\n\nTail\n";
    fs::write(root.join("page.md"), source).unwrap();
    assert_eq!(cli(root, &["render", "page.md"]).status.code(), Some(2));
    let preview = cli(root, &["render", "page.md", "--best-effort"]);
    assert!(preview.status.success());
    let text = String::from_utf8(preview.stdout).unwrap();
    let errors = String::from_utf8(preview.stderr).unwrap();
    assert_eq!(text.matches("!!! warning").count(), 5);
    assert!(text.contains("| native | 2 |"));
    assert!(text.contains("```markdown\n{{ table(\"example.json\") }}\n```"));
    assert!(text.ends_with("Tail\n"));
    assert!(errors.contains("page.md:3:"));
    assert!(errors.contains("missing.json"));
    assert!(!root.join("executed").exists());
    assert_eq!(fs::read_to_string(root.join("page.md")).unwrap(), source);
    write(&root.join("missing.json"), &json!({"repaired": {"ms": 3}}));
    let preview = cli(root, &["render", "page.md", "--best-effort"]);
    let text = String::from_utf8(preview.stdout).unwrap();
    assert!(text.contains("| repaired | 3 |"));
    assert_eq!(text.matches("!!! warning").count(), 4);
}

#[test]
fn canonical_json_helper_keeps_sorted_nested_objects() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("data.json"), r#"{"z":0,"a":{"z":1,"a":2}}"#).unwrap();
    let output = cli(tmp.path(), &["helper", "json-get", "data.json", ""]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "{\"a\":{\"a\":2,\"z\":1},\"z\":0}\n"
    );
}

#[test]
fn per_cell_selections_share_execution_and_explicit_cwd_has_fresh_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let work = root.join("working directory");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("fixture"), "ready").unwrap();
    let modern = json!({"schema_version": 2, "results": [{
        "measurements": [{"exit_code": 0}, {"exit_code": 0}],
        "summary": {"time_wall_clock": {"unit": "second", "count": 2, "median": 0.002, "mean": 0.003}}
    }]});
    write(&work.join("schema.json"), &modern);
    let command = format!("$ {} --shell=none /bin/true", fake_hyperfine(&work));
    write(
        &root.join("in.json"),
        &json!({"native": {
            "N": format!("{command} | json /results/0/summary/time_wall_clock/count * 1"),
            "median": format!("{command} | json /results/0/median * 1000"),
            "mean": format!("{command} | json /results/0/mean * 1000")
        }}),
    );
    let output = cli(root, &["bench", "in.json", "--cwd", work.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let table: Value =
        serde_json::from_slice(&fs::read(root.join("in.render.json")).unwrap()).unwrap();
    assert_eq!(table["native"], json!({"N": 2, "median": 2.0, "mean": 3.0}));
    assert_eq!(fs::read_to_string(work.join("calls")).unwrap(), "run\n");
    let evidence: Value =
        serde_json::from_slice(&fs::read(root.join("in.bench.json")).unwrap()).unwrap();
    assert_eq!(evidence["commands"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["cells"].as_array().unwrap().len(), 3);
    let log = Path::new(evidence["commands"][0]["log"].as_str().unwrap());
    assert!(
        fs::read_to_string(log)
            .unwrap()
            .contains("hyperfine progress")
    );
    assert!(
        cli(root, &["bench", "in.json", "--cwd", work.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(log)
            .unwrap()
            .matches("hyperfine progress")
            .count(),
        1
    );
}

#[test]
fn hyperfine_schema_versions_and_failed_exports_are_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("fixture"), "ready").unwrap();
    write(
        &root.join("in.json"),
        &json!({"native": {"ms": format!("$ {} --shell=none /bin/true", fake_hyperfine(root))}}),
    );
    let modern = json!({"schema_version": 2, "results": [{
        "measurements": [{"exit_code": 0}],
        "summary": {"time_wall_clock": {"unit": "second", "median": 0.0123}}
    }]});
    write(&root.join("schema.json"), &modern);
    assert!(cli(root, &["bench", "in.json"]).status.success());
    let rendered: Value =
        serde_json::from_slice(&fs::read(root.join("in.render.json")).unwrap()).unwrap();
    assert_eq!(rendered["native"]["ms"], 12.3);
    assert!(
        cli(
            root,
            &[
                "bench",
                "in.json",
                "--pointer",
                "/results/0/summary/time_wall_clock/median"
            ]
        )
        .status
        .success()
    );
    write(
        &root.join("schema.json"),
        &json!({"results": [{"median": 0.01, "times": [0.01]}]}),
    );
    assert!(cli(root, &["bench", "in.json"]).status.success());
    for schema in [
        json!({"schema_version": 9, "results": [{"median": 0.01, "times": [0.01]}]}),
        json!({"results": [{"median": 0.01, "times": [0.01], "exit_codes": [1]}]}),
        json!({"results": [{"median": 0.01, "times": []}]}),
        json!({"results": [{"median": 0.01, "times": [0.01]}, {"median": 0.02, "times": [0.02]}]}),
        json!({"schema_version": 2, "results": [{"measurements": [{"exit_code": 1}], "summary": {"time_wall_clock": {"unit": "second", "median": 0.01}}}]}),
    ] {
        write(&root.join("schema.json"), &schema);
        assert_eq!(cli(root, &["bench", "in.json"]).status.code(), Some(2));
        assert!(!root.join("in.render.json").exists());
    }
}

#[test]
fn invalid_cell_selection_preserves_previous_results_without_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("fixture"), "ready").unwrap();
    let executable = fake_hyperfine(root);
    for selection in ["/results/~2median", "results/0/median"] {
        write(
            &root.join("in.json"),
            &json!({"Native": {"ms": format!(
                "$ {executable} --shell=none /bin/true | json {selection} * 1000"
            )}}),
        );
        fs::write(root.join("in.render.json"), "previous result").unwrap();
        assert_eq!(cli(root, &["bench", "in.json"]).status.code(), Some(2));
        assert!(!root.join("calls").exists());
        assert_eq!(
            fs::read_to_string(root.join("in.render.json")).unwrap(),
            "previous result"
        );
    }
}

#[test]
fn render_options_preserve_trusted_article_markup_and_duplicate_labels() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        &root.join("data.json"),
        &json!({"first": {"name": "[link](source.csv)", "value": "`a|b` **yes**<br>ok", "value2": "x\\|y\nnext"}}),
    );
    fs::write(root.join("page.md"), "{{ table(\"data.json\", {\"row_names\":false,\"markdown\":true,\"align\":[\"left\",\"right\",\"center\"],\"headers\":[\"Name\",\"Value\",\"Value\"]}) }}\n").unwrap();
    let result = cli(root, &["render", "page.md"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.contains("| Name | Value | Value |\n| :--- | ---: | :---: |"));
    assert!(text.contains("| [link](source.csv) | `a\\|b` **yes**<br>ok | x\\|y<br>next |"));
    for options in [
        r#"{"row_names":false,"align":["left"]}"#,
        r#"{"headers":["one"]}"#,
        r#"{"markdown":"yes"}"#,
        r#"{"unknown":true}"#,
        r#"{"align":["invalid"]}"#,
    ] {
        fs::write(
            root.join("page.md"),
            format!("{{{{ table(\"data.json\", {options}) }}}}\n"),
        )
        .unwrap();
        assert_eq!(cli(root, &["render", "page.md"]).status.code(), Some(2));
    }
}

#[test]
fn script_expands_a_complete_row_once_and_preserves_result_and_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("data")).unwrap();
    fs::create_dir(root.join("work")).unwrap();
    fs::write(root.join("work/fixture"), "ready").unwrap();
    // Stdout is the row; diagnostics go to stderr. Cwd and the hook log path
    // have the same contracts as existing scalar command cells.
    fs::write(root.join("work/measure.py"), "import json, os, sys\nfrom pathlib import Path\nassert Path('fixture').read_text() == 'ready'\nassert os.environ['SEMSPEC_BENCH_LOG']\nwith open('calls', 'a') as f: f.write('run\\n')\nprint('diagnostic', file=sys.stderr)\nprint(json.dumps({'N': 30, 'Median ms': 1.25, 'Mean ms': 1.5, 'SD ms': .1, 'Min ms': 1, 'Max ms': 2}))\n").unwrap();
    write(
        &root.join("data/in.json"),
        &json!({"native-read": "$ python3 measure.py", "same-script": "$ python3 measure.py", "literal": {"N": 30, "Median ms": 3, "Mean ms": 3, "SD ms": 0, "Min ms": 3, "Max ms": 3}}),
    );
    let result = cli(
        root,
        &["bench", "data/in.json", "--cwd", "work", "--scale", "99"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let table: Value =
        serde_json::from_slice(&fs::read(root.join("data/in.render.json")).unwrap()).unwrap();
    assert_eq!(table["native-read"]["Median ms"], 1.25);
    assert_eq!(table["same-script"], table["native-read"]);
    assert_eq!(table["literal"]["N"], 30);
    assert_eq!(
        table.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["native-read", "same-script", "literal"]
    );
    assert_eq!(
        table["native-read"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["N", "Median ms", "Mean ms", "SD ms", "Min ms", "Max ms"]
    );
    assert_eq!(
        fs::read_to_string(root.join("work/calls")).unwrap(),
        "run\n"
    );
    let evidence: Value =
        serde_json::from_slice(&fs::read(root.join("data/in.bench.json")).unwrap()).unwrap();
    assert_eq!(evidence["commands"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["commands"][0]["kind"], "script-row");
    assert_eq!(evidence["commands"][0]["raw"], table["native-read"]);
    assert_eq!(evidence["rows"].as_array().unwrap().len(), 2);
    assert_eq!(evidence["rows"][0]["row"], "native-read");
    assert_eq!(evidence["rows"][0]["expression"], "python3 measure.py");
    let log = evidence["commands"][0]["log"].as_str().unwrap();
    assert!(fs::read_to_string(log).unwrap().contains("diagnostic"));
    fs::write(
        root.join("data/page.md"),
        "{{ table(\"in.render.json\") }}\n",
    )
    .unwrap();
    let result = cli(root, &["render", "data/page.md"]);
    assert!(result.status.success());
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("| native-read | 30 | 1.25 | 1.5 | 0.1 | 1 | 2 |")
    );
}

#[test]
fn script_rows_and_cell_commands_can_share_a_table_but_not_return_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        &root.join("in.json"),
        &json!({"script": "$ printf '%s' '{\"value\":2}'", "cell": {"value": "$ printf 3"}}),
    );
    assert!(cli(root, &["bench", "in.json"]).status.success());
    let table: Value =
        serde_json::from_slice(&fs::read(root.join("in.render.json")).unwrap()).unwrap();
    assert_eq!(table, json!({"script": {"value": 2}, "cell": {"value": 3}}));
    for output in [
        "null",
        "[]",
        "7",
        "{}",
        "{\"value\":[]}",
        "{\"value\":{}}",
        "{\"value\":\"$ touch executed\"}",
        "{\"wrong\":2}",
        "not-json",
        "{} {}",
    ] {
        write(
            &root.join("in.json"),
            &json!({"script": format!("$ printf '%s' '{output}'"), "cell": {"value": 3}}),
        );
        fs::write(root.join("in.render.json"), "stale").unwrap();
        fs::write(root.join("in.bench.json"), "stale").unwrap();
        assert_eq!(
            cli(root, &["bench", "in.json"]).status.code(),
            Some(2),
            "{output}"
        );
        assert!(!root.join("in.render.json").exists());
        assert!(!root.join("in.bench.json").exists());
        assert!(!root.join("executed").exists());
        assert!(root.join("in.bench.logs/0001.log").exists());
    }
    write(
        &root.join("in.json"),
        &json!({"script": "$ printf '{\"value\":1}'; exit 9"}),
    );
    assert_eq!(cli(root, &["bench", "in.json"]).status.code(), Some(2));
    // Rendering a template must reject the unresolved row without running it.
    write(
        &root.join("in.json"),
        &json!({"script": "$ touch executed; printf '{\"value\":1}'"}),
    );
    fs::write(root.join("page.md"), "{{ table(\"in.json\") }}\n").unwrap();
    assert_eq!(cli(root, &["render", "page.md"]).status.code(), Some(2));
    assert!(!root.join("executed").exists());
}

#[test]
fn invalid_static_columns_and_row_command_plans_preserve_previous_results() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for table in [
        json!({"script": "$ touch executed; printf '{}'", "a": {"value": 1}, "b": {"wrong": 2}}),
        json!({"script": "$ hyperfine --runs 2 '/bin/true' | json /results/0/median * 1000"}),
        json!({"script": "$ printf '{\"value\":1}'", "cell": {"value": "$ printf '{\"value\":1}'"}}),
        json!({"script": "$ 'unmatched"}),
    ] {
        write(&root.join("in.json"), &table);
        fs::write(root.join("in.render.json"), "previous result").unwrap();
        fs::write(root.join("in.bench.json"), "previous evidence").unwrap();
        assert_eq!(cli(root, &["bench", "in.json"]).status.code(), Some(2));
        assert_eq!(
            fs::read_to_string(root.join("in.render.json")).unwrap(),
            "previous result"
        );
        assert_eq!(
            fs::read_to_string(root.join("in.bench.json")).unwrap(),
            "previous evidence"
        );
        assert!(!root.join("executed").exists());
    }
}

#[test]
fn script_row_preserves_full_float_precision() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let input = root.join("in.json");
    fs::write(
        &input,
        r#"{"VM":"$ printf '%s' '{\"mean\":196.85512733333334}'"}"#,
    )
    .unwrap();
    let status = cli(root, &["bench", "in.json"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let text = fs::read_to_string(root.join("in.render.json")).unwrap();
    assert!(text.contains("196.85512733333334"), "{text}");
}

#[test]
fn hyperfine_row_generates_all_statistics_from_one_export() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("fixture"), "ready").unwrap();
    let command = format!("$ {} --shell=none --runs 2 /bin/true", fake_hyperfine(root));
    write(
        &root.join("in.json"),
        &json!({"Native": command, "alias": command}),
    );
    let result = cli(root, &["bench", "in.json", "--scale", "99"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let table: Value =
        serde_json::from_slice(&fs::read(root.join("in.render.json")).unwrap()).unwrap();
    assert_eq!(
        table["Native"],
        json!({"N":2,"Median ms":2.0,"Mean ms":3.0,"SD ms":1.0,"Min ms":1.0,"Max ms":4.0})
    );
    assert_eq!(table["Native"], table["alias"]);
    assert_eq!(fs::read_to_string(root.join("calls")).unwrap(), "run\n");
    let evidence: Value =
        serde_json::from_slice(&fs::read(root.join("in.bench.json")).unwrap()).unwrap();
    assert_eq!(evidence["commands"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["commands"][0]["kind"], "hyperfine-row");
    assert_eq!(evidence["commands"][0]["value"], table["Native"]);
    assert_eq!(evidence["rows"].as_array().unwrap().len(), 2);
    assert!(evidence["cells"].as_array().unwrap().is_empty());
}

#[test]
fn hyperfine_rows_validate_v2_counts_units_and_failure_before_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("fixture"), "ready").unwrap();
    let executable = fake_hyperfine(root);
    let valid = json!({"schema_version":2,"results":[{"command":"/bin/true","measurements":[{"exit_code":0},{"exit_code":0}],"summary":{"time_wall_clock":{"unit":"second","count":2,"median":0.002,"mean":0.003,"stddev":0.001,"min":0.001,"max":0.004}}}]});
    write(&root.join("schema.json"), &valid);
    write(
        &root.join("in.json"),
        &json!({"VM":format!("$ {executable} --runs 2 /bin/true")}),
    );
    assert!(cli(root, &["bench", "in.json"]).status.success());
    let result: Value =
        serde_json::from_slice(&fs::read(root.join("in.render.json")).unwrap()).unwrap();
    assert_eq!(result["VM"]["N"], 2);
    assert_eq!(result["VM"]["Median ms"], 2.0);
    for (pointer, value) in [
        ("/results/0/summary/time_wall_clock/count", json!(3)),
        (
            "/results/0/summary/time_wall_clock/unit",
            json!("millisecond"),
        ),
        ("/results/0/measurements/0/exit_code", json!(1)),
        ("/results/0/summary/time_wall_clock/stddev", Value::Null),
    ] {
        let mut bad = valid.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        write(&root.join("schema.json"), &bad);
        fs::write(root.join("in.render.json"), "stale").unwrap();
        assert_eq!(cli(root, &["bench", "in.json"]).status.code(), Some(2));
        assert!(!root.join("in.render.json").exists());
        assert!(!root.join("in.bench.json").exists());
    }
}
