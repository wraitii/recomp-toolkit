#!/usr/bin/env python3
"""Summarize a JIT run's hottest interpreter encounters and compilation outcomes."""

import argparse
from collections import Counter
import json
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path, help="per-process JIT output directory")
    parser.add_argument("--top", type=int, default=25)
    args = parser.parse_args()
    encounters_path = args.run / "encounters.json"
    if not encounters_path.exists():
        parser.error("encounters.json is written on normal guest shutdown; finish the run first")
    encounters = json.loads(encounters_path.read_text())
    requests = json.loads((args.run / "requests.json").read_text())
    batches = {}
    compiled = {}
    runtime = {}
    native_calls = {}
    handoffs = {}
    for path in sorted(args.run.glob("batch-*/jit.json")):
        batch = int(path.parent.name.removeprefix("batch-"))
        manifest = json.loads(path.read_text())
        batches[batch] = manifest
        state_path = path.parent / "runtime.json"
        if state_path.exists():
            runtime[batch] = json.loads(state_path.read_text())
            for entry in runtime[batch]["entries"]:
                native_calls[entry["target"]] = native_calls.get(entry["target"], 0) + entry["native_calls"]
                handoffs[entry["target"]] = handoffs.get(entry["target"], 0) + entry.get("branch_handoffs", 0)
        if manifest.get("compilation", {}).get("exit_code") == 0:
            for address in manifest.get("exported_entries", []) or re.findall(
                r"\{0x([0-9a-fA-F]{8})u, [^,]+, jit_entry_", (path.parent / "module.c").read_text()
            ):
                compiled.setdefault(address.lower(), batch)

    def outcome(encounter):
        target = encounter["target"]
        if target in compiled:
            batch = compiled[target]
            state = runtime.get(batch, {})
            status = state.get("status", "compiled; publication unknown")
            if status == "published":
                status = "native entered" if native_calls.get(target, 0) else "published; unused"
            detail = state.get("reason") or ""
            if status == "published; unused" and encounter.get("calls") == 0 and encounter.get("branches", 0):
                if state.get("branch_handoff_supported"):
                    detail = "branch-only target; no eligible backedge observed after publication"
                else:
                    detail = "branch-only target; " + (state.get("branch_handoff_restriction") or "run predates branch handoff")
            return status, batch_detail(batch, detail, target)
        decision = requests.get(target)
        if decision:
            batch = decision.get("batch")
            manifest = batches.get(batch, {})
            reasons = manifest.get("eligibility", {}).get("reasons", [])
            status = decision.get("reason", decision["status"])
            detail = ""
            if reasons:
                detail = ", ".join(reasons)
            elif decision.get("error"):
                detail = decision["error"].splitlines()[0]
            return status, batch_detail(batch, detail, target) if batch is not None else detail
        if encounter["queued"]:
            return "queued; not processed", "shutdown can drop queued work"
        if encounter["hits"] < 8:
            return "below threshold", "fewer than 8 encounters"
        return "never queued", "queue full or worker stopped"

    def batch_detail(batch, detail, target):
        manifest = batches.get(batch, {})
        recovery = manifest.get("recovery", {})
        prefix = f"batch {batch}"
        if recovery:
            prefix += (f": {recovery['instructions']} insns, {recovery['function_count']} regions, "
                       f"{recovery['external_call_sites']} external calls")
            address = int(target, 16)
            owner = next((span['owner'] for span in manifest['ranges']
                          if int(span['start'], 16) <= address < int(span['start'], 16) + span['size']), None)
            if owner:
                prefix += f"; region {owner}"
        return prefix + (f"; {detail}" if detail else "")

    batch_counts = Counter(
        b.get("compilation", {}).get("status", "unfinished") for b in batches.values()
    )
    print("Batches: " + ", ".join(f"{count} {status}" for status, count in batch_counts.items()))
    counts = Counter(outcome(e)[0] for e in encounters)
    print("Targets: " + ", ".join(f"{count} {status}" for status, count in counts.items()))
    print("\nHits count interpreter encounters, including loop branches; they are not native call counts.")
    print("Native counts and final pending states are complete after normal shutdown; older runs lack runtime evidence.")
    print(f"\n{'Target':8} {'Hits':>12} {'Calls':>10} {'Branches':>10} {'Native':>10} {'Handoffs':>9}  {'Outcome':29} Detail")
    for encounter in sorted(encounters, key=lambda e: -e["hits"])[:args.top]:
        status, detail = outcome(encounter)
        target = encounter['target']
        calls = f"{encounter['calls']:,}" if 'calls' in encounter else "?"
        branches = f"{encounter['branches']:,}" if 'branches' in encounter else "?"
        native = str(native_calls.get(target, 0)) if compiled.get(target) in runtime else "?"
        transferred = str(handoffs.get(target, 0)) if compiled.get(target) in runtime else "?"
        print(f"{target} {encounter['hits']:12,} {calls:>10} {branches:>10} {native:>10} {transferred:>9}  {status:29} {detail}")


if __name__ == "__main__":
    main()
