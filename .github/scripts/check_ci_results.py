#!/usr/bin/env python3

"""Fail a terminal CI job unless every serialized dependency succeeded.

Parent workflows pass GitHub's `toJSON(needs)` object through the NEEDS
environment variable. Treat skipped and cancelled dependencies as failures by
default: for a required fan-in job, only an explicit success is safe to accept.
Callers may set ALLOWED_SKIPPED_DEPENDENCIES to a comma-separated list for jobs
that are intentionally unavailable in that repository context.
"""

import json
import os


def allowed_skipped_dependencies() -> set[str]:
    return {
        dependency.strip()
        for dependency in os.environ.get("ALLOWED_SKIPPED_DEPENDENCIES", "").split(",")
        if dependency.strip()
    }


def main() -> None:
    # Keep result policy in one script so blocking-ci and postmerge-ci cannot
    # drift in how they interpret dependency conclusions.
    needs = json.loads(os.environ["NEEDS"])
    allowed_skipped = allowed_skipped_dependencies()
    failures = sorted(
        (name, dependency["result"])
        for name, dependency in needs.items()
        if dependency["result"] != "success"
        and not (dependency["result"] == "skipped" and name in allowed_skipped)
    )

    if failures:
        print("CI dependencies did not succeed:")
        for name, result in failures:
            print(f"{name}: {result}")
        raise SystemExit(1)

    if allowed_skipped:
        print("All CI dependencies succeeded or were intentionally skipped.")
    else:
        print("All CI dependencies succeeded.")


if __name__ == "__main__":
    main()
