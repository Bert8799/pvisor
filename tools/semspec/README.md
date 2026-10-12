# semspec

A standalone Rust CLI that extracts human-reviewed, black-box checks from user
Markdown documentation. Requires Rust 1.89+, Unix and Bash. Specifications are
trusted executable code; PASS does not establish complete coverage or human approval.
See [the design](DESIGN.md) and [engine contract](ENGINE.md).

```sh
cargo install --path tools/semspec --locked
semspec init cases                   # creates only cases/*.md in a new project
semspec lint cases
semspec list cases
semspec run cases
semspec run cases/example.md --subject-bin /usr/bin/true
semspec review cases --strict
```

No configuration file or external script is needed. Paths are mandatory: semspec
never searches docs/src/zh/cases, cases or cwd implicitly. `run`, `lint`, `list`
and `review` accept one or more Markdown files/directories, and `show ITEM`
accepts paths after the item. `--spec-dir PATH` also supplies explicit inputs and
can be repeated, including for human approval. Directories are scanned recursively;
overlapping files are processed once, while duplicate IDs in distinct files fail.

```sh
semspec run cases/first.md cases/second.md
semspec lint docs/src/zh/cases docs/src/zh/reference
```

~~~~markdown
## Record a successful command

The public command must succeed.

<!-- semspec: case id=S-EXAMPLE-001 timeout=10s -->
```bash
true
```
~~~~

The comment occupies one line immediately before the Bash fence. The ID is required;
`timeout`, paired `xfail-on`/`xfail-reason` are optional. Headings are ordinary prose
and unmarked fences are never executed. One section may contain multiple cases.
The surrounding section, including prose, parameters and examples, is sealed.

Shared preparation is a normal Bash fence preceded by `<!-- semspec: setup -->`.
A case sources preparation from its sibling `index.md`, then its own document,
in source order, in a fresh temporary workspace. Definitions, fixture services and
assertions belong in these Markdown blocks. Their complete documents participate
in review digests. `helper setup FILE.md` prints the extracted preparation for
manual use. Checks call installed commands through PATH; optional `--subject-bin`
(or SEMSPEC_SUBJECT_BIN) supplies an absolute SUBJECT_BIN to checks that use it.
`--timeout` sets the default per-case timeout, otherwise 180 seconds; a comment’s
timeout takes precedence. Environment prerequisites are Bash checks; exit 77 skips.

Each check runs with `set -euo pipefail`, null stdin and its own process group.
Timeout sends TERM, then KILL after five seconds; ordinary exit also kills remaining
group descendants. Do not detach persistent processes. Failure directories and
logs are retained, including XFAIL; `--keep` retains successful checks too.
PASS/SKIP/XFAIL exit 0 by default; `--require-pass` makes every non-PASS fail.
FAIL/XPASS, inventory errors or required pending review exit 1;
usage/spec errors exit 2; ERROR verdicts exit 3. Human and JSON reports are supported.
SSH signing, JUnit and parallel execution remain unsupported.

The runner requires the report to contain exactly the selected case IDs, once each.
Explicit `--case` IDs must be unique and belong to `--domain` when specified.
With `--output`, valid selection clears the previous report before execution and
publishes the fresh report atomically, even on case failure. Invalid selections
preserve an existing report; launch failure/interruption leaves no stale result.
Output must not replace loaded specifications, preparation or the review ledger.
The USE entry runs semspec with `--require-pass`; no separate catalog parser or
report-validation script is used. Strict PASS does not approve cases and can be
combined with `--require-reviewed` for human review requirements.

The nearest common directory of the supplied directories (or file parents) is
the review root: preparation names are relative to it and REVIEWED.toml lives there.
With a single input this is its directory, as before. A missing ledger means UNREVIEWED. Only a person
may approve from an interactive terminal:

```sh
semspec show S-EXAMPLE-001 cases
semspec --spec-dir cases approve @engine @vocab:index.md S-EXAMPLE-001 --reviewer YOUR_NAME
```

Approval shows current content/dependencies, requires typing the item and atomically
updates only the ledger. Tests never create approval records. Changed case prose,
preparation documents or engine semantics make existing approvals STALE.
TTY checks cannot authenticate a reviewer; repository review rules must protect
specifications, the engine and human ledgers. AI must not execute approve or edit
real ledgers/snapshots, or weaken claims/checks/xfail to obtain PASS.

From pVisor:

```sh
just test-semspec
just semspec lint docs/src/zh/cases
just semspec run docs/src/zh/cases/01-first-job.md --subject-bin target/release/pvisor
just cases --suite use               # builds product; semspec requires every selected case to PASS
just semspec run docs/src/zh/cases --domain USE --require-pass --subject-bin target/release/pvisor
just semspec review docs/src/zh/cases --strict         # fails until human review
```

STAGE checks live in docs/src/zh/cases/06-stage-apply.md. DOC and VM appendices
use the same Markdown format and preparation from reference/index.md:

```sh
just semspec lint docs/src/zh/reference
just semspec lint tools/semspec/semantics
```

Self-specifications in semantics/review.md are drafts. AI must not execute
S-REVIEW-004. Conventional tests cover Markdown inputs, parsing, digest golden
values, review transitions, verdicts, timeout cleanup and exact content/tree helpers.

## Executable benchmark tables

`bench` and `render` work independently of semantic cases and human review.
Define a JSON object whose keys are row names; each row maps column names to
scalar values, or a `$ ` script command that returns the entire row. All resolved rows must have the same columns. JSON row order and the first
row's column order are preserved. Numbers, strings, booleans and null are literal
data; only strings beginning with `$ ` are commands.

```json
{
  "Native": {
    "Median ms": "$ hyperfine --shell=none --warmup 3 --runs 60 '/bin/true'",
    "N": 60
  }
}
```

```sh
semspec bench in.json                 # creates in.render.json and in.bench.json
semspec bench --pointer /results/0/mean --scale 1000 in.json
semspec render article.md             # expanded Markdown on stdout
semspec render article.md --output article.render.md
```

Direct hyperfine commands are parsed as argv, without an outer shell. Hyperfine
alone measures its subject; semspec startup and JSON processing are outside that
interval. Each cell must measure exactly one subject. The default extraction is
`/results/0/median` multiplied by 1000 (milliseconds); `--pointer` is RFC 6901 JSON
Pointer, and `--scale` applies to every hyperfine cell in that invocation.
`--ignore-failure` is rejected, and sample exit codes must all be zero.
Hyperfine 1.x and 2.x exports are supported. In 2.x, the familiar time pointers
`/results/0/{mean,median,stddev,min,max}` select `summary.time_wall_clock`; native
2.x pointers also work. For older exports without sample exit codes, successful
hyperfine execution establishes success because ignoring failures is disabled.

Commands run in the template's directory, or the directory selected by `--cwd`,
and inherit the caller's environment. A cell may override extraction with
`$ hyperfine ... | json /results/0/summary/time_wall_clock/mean * 1000`.
The suffix is semspec syntax, not a shell pipeline. Identical base hyperfine argv
execute once per invocation; different selectors share that raw experiment.
Use `| json /results/0/summary/time_wall_clock/count * 1` for sample counts.
Each command writes a fresh log in `in.bench.logs/`; hooks receive its path via
`SEMSPEC_BENCH_LOG`. Logs remain available after failure.
Hyperfine always writes a fresh temporary JSON export. If the command specifies
`--export-json native.json`, semspec also saves the successful export to that path.
Otherwise raw samples are still preserved in `in.bench.json`, together with the
original commands, extraction parameters and row/column mappings. A failed
execution removes the old resolved table and evidence file; invalid templates
are rejected before execution and leave existing results unchanged. The template
is never overwritten, and completed tables are published atomically.

Other trusted cell commands execute in Bash with `-euo pipefail` and must print exactly
one JSON scalar; these values are inserted directly without pointer extraction
or scaling. This supports explicit calculations or other JSON-producing tools.
Command cells are executable code, like semantic specification Bash blocks.

In Markdown, put a table reference on its own line:

~~~~markdown
## Results

{{ table("in.render.json") }}
~~~~

Paths resolve relative to the Markdown file. `render` replaces references with
ordinary Markdown tables, using an unnamed first column for row names. It escapes
cell text and pipes, renders newlines as `<br>` and null as `—`. Fenced/indented
code blocks and inline examples remain literal. Missing/malformed JSON or
unresolved command cells fail; `render` never executes commands or edits its
source. `--stdin` accepts preprocessed content while the input filename supplies
the base path, for documentation integrations.

`render --best-effort` is intended for live previews: a missing, malformed or
unresolved table becomes a visible warning, its source location and error are
printed to stderr, and other tables and prose continue to render. It does not
execute unresolved commands or change inputs. A later render reads repaired
files normally. Strict rendering remains the default for published builds.

pVisor's native Zensical configurations register `docs.semspec_tables`, a thin
Python-Markdown preprocessor invoking `semspec render --stdin`. `just docs-build`
and `just docs-serve` build the CLI and provide its path. Benchmark execution
remains an explicit `semspec bench` operation before documentation rendering.
`docs-serve` opts into `--best-effort`; `docs-build` keeps strict rendering.

A direct hyperfine row command also expands all six statistics in one experiment:

```json
{"Native": "$ hyperfine --shell=none --runs 60 --warmup 3 --command-name native /bin/true"}
```

The row contains `N`, `Median ms`, `Mean ms`, `SD ms`, `Min ms` and `Max ms`,
all selected from the same export. Raw exports and the `hyperfine-row` kind are
retained in evidence, with the row/column mapping. Hyperfine 1.x and 2.x are
supported; counts must match samples. Row units are fixed to milliseconds;
`--pointer` and `--scale` affect scalar cells only. A scalar `| json` suffix on
an entire hyperfine row is rejected before execution.

A script can also produce all columns of a row in one execution:

```json
{
  "native-read": "$ python3 native_read.py"
}
```

The script's stdout must contain one nonempty JSON object, with column names
as keys and resolved scalar data as values, for example:

```json
{"N": 30, "Median ms": 1.25, "Mean ms": 1.5, "SD ms": 0.1, "Min ms": 1.0, "Max ms": 2.0}
```

`semspec bench in.json` fills this object into `native-read` in
`in.render.json`; ordinary rows and scalar command cells can coexist. Identical
script command strings run once, even when referenced by multiple rows. Row
and column insertion order are retained. Row results receive no pointer
extraction or scaling: the script defines the values and units. Scripts run in
the same template directory/`--cwd` as cell commands. Write diagnostics to stderr;
if the script invokes hyperfine, redirect its progress away from stdout and
retain its raw export separately.

Arrays, empty objects, nested cells, unresolved `$ ` values, nonzero exits and
inconsistent columns fail without publishing results. Returned commands are
never recursively executed. Known template errors are checked before execution;
columns determined by scripts are checked after execution. A script command
cannot be used simultaneously as a row and a scalar cell. Rendering an unresolved
row fails without running it.

Evidence includes each script's raw returned object and log, `kind: script-row`,
and a `rows` mapping with its row name, expression and columns. Script numbers
are script-provided data; semspec does not time the script or certify its internal
measurement/statistics as hyperfine results. Direct hyperfine evidence remains
separate. This feature does not change pVisor's strict hyperfine-only preparation
and publication checks in `benchmark.new`.

Table options preserve existing article layouts when moving literal data to JSON:

```markdown
{{ table("data.render.json", {"row_names": false, "markdown": true, "align": ["left", "right"]}) }}
```

`row_names` defaults to true; false displays only the named data columns.
`markdown` defaults to false (escaped text); true explicitly trusts repository
markup such as links, code and emphasis. Pipes and newlines remain table-safe.
`align` is optional and lists `left`, `right`, `center` or `default` for every
rendered column, including row names when enabled. Optional `headers` supplies
one displayed label per rendered column, allowing repeated labels while JSON
keys remain unique. Unknown options, invalid types and incorrect counts fail.
