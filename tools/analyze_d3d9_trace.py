import argparse
from collections import Counter, defaultdict
import json
import math
from pathlib import Path
import re


def distribution(values):
    values = sorted(values)
    if not values:
        return None
    return {"count": len(values), "total": sum(values), **{
        name: values[max(0, math.ceil(len(values) * fraction) - 1)]
        for name, fraction in [("p50", .5), ("p95", .95), ("p99", .99), ("max", 1)]}}


def analyze(path, start, count):
    stop = None if count is None else start + count
    frames = {}
    draws = {}
    programs = {}
    families = {}
    events = []
    identities = []
    context = None
    with path.open(errors="replace") as source:
        for line in source:
            prefix, found, record = line.partition("[d3d9-compile] ")
            if not found:
                continue
            kind, _, body = record.rstrip().partition(" ")
            fields = dict(re.findall(r"(\w+)=(\S+)", body))
            if kind == "identity":
                identities.append(body)
                if len(identities) > 1:
                    raise ValueError("multiple device identities; split the log into device sessions")
            elif kind == "program":
                programs[fields["code"]] = body
            elif kind == "observed-family":
                families[int(fields["id"])] = body
            elif kind == "draw-programs":
                context = (int(fields["frame"]), int(fields["draw"]))
                draws[context] = {"programs": [fields["vs"], fields["ps"]]}
            elif kind == "draw-family":
                key = (int(fields["frame"]), int(fields["draw"]))
                draws.setdefault(key, {}).update(family=int(fields["family"]), recipe=fields["recipe"])
            elif kind == "frame":
                frame = int(fields.pop("frame"))
                if frame in frames:
                    raise ValueError("repeated frame numbers; split the log into device sessions")
                frames[frame] = {key: float(value) if key.endswith("_ms") else int(value)
                                 for key, value in fields.items()}
                context = None
            elif kind in ("module", "pipeline-end"):
                frame = int(fields["frame"])
                draw = context if context and context[0] == frame else None
                events.append((frame, draw, kind, float(fields["ms"]), fields.get("ok", "true")))
    selected = {frame: data for frame, data in frames.items()
                if frame >= start and (stop is None or frame < stop)}
    if not selected:
        raise ValueError("no completed frame records in the selected window")
    selected_draws = {key: value for key, value in draws.items() if key[0] in selected}
    counts = Counter(value["family"] for value in selected_draws.values() if "family" in value)
    pairs = defaultdict(Counter)
    for draw in selected_draws.values():
        if "family" in draw and "programs" in draw:
            pairs[draw["family"]][tuple(draw["programs"])] += 1
    costs = defaultdict(float)
    event_times = defaultdict(list)
    failures = 0
    for frame, draw, kind, ms, ok in events:
        if frame not in selected:
            continue
        event_times[kind].append(ms)
        failures += ok == "false"
        family = selected_draws.get(draw, {}).get("family")
        costs[family] += ms
    total_draws = sum(frame["draws"] for frame in selected.values())
    if sum(counts.values()) != total_draws:
        raise ValueError("draw-family records do not match successful draws; incomplete capture or failed draw preparation")
    total_compile_ms = sum(costs.values())
    return {
        "input": str(path.resolve()),
        "identity": identities,
        "window": {"start": min(selected), "end_exclusive": max(selected) + 1,
                   "completed_frames": len(selected), "draws": total_draws,
                   "draws_with_family": sum(counts.values())},
        "frame_counters": {key: distribution([frame[key] for frame in selected.values() if key in frame])
                           for key in sorted({key for frame in selected.values() for key in frame})},
        "compile_events_ms": {key: distribution(values) for key, values in event_times.items()},
        "compile_failures": failures,
        "unattributed_compile_ms": costs[None],
        "observed_families": [
            {"id": family, "draws": draws_count,
             "draw_fraction": draws_count / total_draws if total_draws else None,
             "compile_ms": costs[family],
             "compile_fraction": costs[family] / total_compile_ms if total_compile_ms else None,
             "descriptor": families.get(family),
             "program_pairs": [{"programs": pair, "draws": n} for pair, n in pairs[family].most_common()]}
            for family, draws_count in counts.most_common()],
        "programs": programs,
        "limits": ["Observed families do not establish interpreter eligibility or prepared coverage.",
                   "CPU draw and compilation timings overlap; neither measures frame duration or GPU execution.",
                   "Only completed frames are included; texture transfers and failed draw preparation are excluded.",
                   "Compile cost is attributed to the draw that caused each miss, including shared module creation."],
    }


def main():
    parser = argparse.ArgumentParser(description="Summarize an opt-in D3D9 pipeline trace without running the renderer.")
    parser.add_argument("log", type=Path)
    parser.add_argument("--start", type=int, default=0)
    parser.add_argument("--count", type=int)
    args = parser.parse_args()
    if args.start < 0 or (args.count is not None and args.count <= 0):
        parser.error("start must be nonnegative and count must be positive")
    try:
        result = analyze(args.log, args.start, args.count)
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"{error}\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
