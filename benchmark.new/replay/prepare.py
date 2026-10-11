"""B-REPLAY: freeze inputs and preflight; hyperfine measures the replay CLI.

One fixed synthetic task per pinned adapter, no tools/model, 60 runs, no
warmups. Exact source, native prefix, arguments, historical observation and
unchanged workspace are verified outside timing for every execution.
"""

import argparse
import json
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import ROOT, create_batch, digest, materialize, save
from replay_fixtures import trajectory
from replay_hooks import SENTINEL, conclude, prepare


def prepare_batch(template, binary, output=None):
    batch = create_batch(template, output)
    frozen = batch / "pvisor-replay"
    shutil.copy2(binary.resolve(strict=True), frozen)
    agents = json.loads(template.read_text())
    protocol = {}
    inputs = []
    for agent in agents:
        fixture = batch / "fixtures" / agent
        (fixture / "workspace").mkdir(parents=True)
        (fixture / "workspace/existing").write_text(SENTINEL)
        command = "printf fixture-0 > marker-0.txt"
        source, jsonl = trajectory(agent, 0, command)
        path = fixture / "trajectory.json"
        path.write_text(
            ("\n".join(json.dumps(item) for item in source) if jsonl else json.dumps(source)) + "\n"
        )
        protocol[agent] = {
            "task": 0,
            "command": command,
            "next_command": "cat marker-0.txt",
            "sha256": digest(path),
        }
        inputs.append(path)
    save(batch / "protocol.json", protocol)
    materialize(
        template,
        batch,
        {"BINARY": frozen},
        artifacts=[frozen, *inputs, batch / "protocol.json"],
        cwd=batch,
        helpers=[
            ROOT / "replay" / name
            for name in ("prepare.py", "replay_fixtures.py", "replay_hooks.py")
        ],
    )
    manifest_path = batch / "manifest.json"
    manifest = json.loads(manifest_path.read_text())
    manifest["protocol_sha256"] = digest(batch / "protocol.json")
    manifest["fixture_scope"] = (
        "one fixed synthetic task 0 per adapter; distinct from historical twenty-task cohort"
    )
    save(manifest_path, manifest)
    table = json.loads((batch / "in.json").read_text())
    for agent, row in table.items():
        argv = shlex.split(row[2:] if isinstance(row, str) else row["Median ms"][2:])
        end = argv.index("|") if "|" in argv else len(argv)
        prepare(batch, agent)
        with (batch / f"preflight-{agent}.log").open("w") as log:
            subprocess.run(
                shlex.split(argv[end - 1]),
                cwd=batch,
                check=True,
                timeout=30,
                stdout=log,
                stderr=subprocess.STDOUT,
            )
        conclude(batch, agent)
    return batch


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("template", type=Path)
    p.add_argument("--binary", type=Path, required=True)
    args = p.parse_args()
    print(prepare_batch(args.template.resolve(), args.binary))
