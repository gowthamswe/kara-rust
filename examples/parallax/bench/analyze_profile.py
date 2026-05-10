#!/usr/bin/env python3
"""Extract per-function self/inclusive sample counts from a samply profile.

samply emits Firefox Profiler JSON. Top-of-stack (self) samples = where
the program is *actually executing*; on-stack-anywhere (inclusive)
samples = where it spent time including time blocked in callees.

Usage: analyze_profile.py <profile.json.gz>
"""

import gzip
import json
import sys
from collections import defaultdict


def main():
    path = sys.argv[1]
    with gzip.open(path, "rt") as f:
        prof = json.load(f)

    # Aggregate across all threads.
    self_counts = defaultdict(int)        # func_label -> top-of-stack samples
    inclusive_counts = defaultdict(int)   # func_label -> on-stack samples
    total_samples = 0

    for thread in prof.get("threads", []):
        thread_name = thread.get("name", "<unknown>")
        is_main = thread.get("isMainThread", False)
        # If you want to filter out OS-level threads, gate on isMainThread.
        # For our purposes, profile everything — fan-out workers may live
        # in non-main threads, that's the whole point.

        strings = thread["stringArray"]
        func_table = thread["funcTable"]
        frame_table = thread["frameTable"]
        stack_table = thread["stackTable"]
        samples = thread["samples"]

        # Quick lookups.
        func_names = func_table["name"]                  # list of string indices
        func_resource = func_table.get("resource", [-1] * len(func_names))
        frame_func = frame_table["func"]
        stack_frame = stack_table["frame"]
        stack_prefix = stack_table["prefix"]

        # resourceTable maps lib indices → string indices for module names.
        resource_table = thread.get("resourceTable", {})
        resource_names = resource_table.get("name", [])

        def func_label(func_idx):
            name = strings[func_names[func_idx]] if func_names[func_idx] is not None else "<anon>"
            res_idx = func_resource[func_idx] if func_idx < len(func_resource) else -1
            if res_idx is not None and res_idx >= 0 and res_idx < len(resource_names):
                lib = strings[resource_names[res_idx]]
                lib = lib.split("/")[-1] if lib else ""
                return f"{name}  [{lib}]"
            return name

        # Walk samples.
        sample_stacks = samples.get("stack", [])
        for stack_idx in sample_stacks:
            if stack_idx is None:
                continue
            total_samples += 1
            # Top of stack = self.
            top_frame = stack_frame[stack_idx]
            top_func = frame_func[top_frame]
            self_counts[func_label(top_func)] += 1
            # Walk up the stack for inclusive.
            seen = set()
            cur = stack_idx
            while cur is not None:
                fr = stack_frame[cur]
                fn = frame_func[fr]
                lbl = func_label(fn)
                if lbl not in seen:
                    inclusive_counts[lbl] += 1
                    seen.add(lbl)
                cur = stack_prefix[cur]

    print(f"Total samples: {total_samples}")
    print(f"Threads: {len(prof.get('threads', []))}")
    print()

    def render(title, counts, n=30):
        print(f"=== {title} ===")
        items = sorted(counts.items(), key=lambda kv: -kv[1])[:n]
        for label, count in items:
            pct = 100.0 * count / total_samples if total_samples else 0
            print(f"  {count:7d}  ({pct:5.1f}%)  {label}")
        print()

    render("TOP SELF (top-of-stack — where time is actually spent)", self_counts)
    render("TOP INCLUSIVE (on-stack anywhere — including blocked-in-callee)", inclusive_counts)


if __name__ == "__main__":
    main()
