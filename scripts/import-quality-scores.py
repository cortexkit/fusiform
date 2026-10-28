#!/usr/bin/env python3
"""Import operator grades without converting decimal text to binary floats."""

import argparse
import csv
import json
from pathlib import Path
import re


DESTINATION = Path(__file__).resolve().parents[1] / "crates/fusiform-module/data/quality-scores.json"
PURPOSE = "Operator-assigned model quality scores for routing selection, not upstream facts."
DECIMAL = re.compile(r"-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?\Z")
MODEL = re.compile(r"[^/\s]+/[^\s]+\Z")


def read_rows(source):
    rows = []
    seen = set()
    reader = csv.DictReader(source)
    if not {"model", "ELO", "EQ"}.issubset(reader.fieldnames or []):
        raise ValueError("CSV header must contain model,ELO,EQ (AA is ignored)")
    for row in reader:
        line = reader.line_num
        if None in row or any(value is None for value in row.values()):
            raise ValueError(f"line {line}: wrong number of CSV fields")
        model = row["model"].strip()
        if not MODEL.fullmatch(model):
            raise ValueError(f"line {line}: model must be provider/model without whitespace")
        if model in seen:
            raise ValueError(f"line {line}: duplicate model {model!r}")
        seen.add(model)
        scores = []
        for column in ("ELO", "EQ"):
            value = row[column]
            if value and not DECIMAL.fullmatch(value):
                raise ValueError(f"line {line}: {column} must be a plain decimal or empty: {value!r}")
            scores.append(value or "null")
        if scores != ["null", "null"]:
            rows.append((model, *scores))
    return sorted(rows)


def render(rows, graded_at_ms):
    lines = [
        "{",
        '  "graded_by": "operator",',
        f'  "graded_at_ms": {graded_at_ms},',
        f'  "purpose": {json.dumps(PURPOSE)},',
    ]
    if rows:
        lines.append('  "rows": [')
        for index, (model, elo, eq) in enumerate(rows):
            comma = "," if index + 1 < len(rows) else ""
            # Decimal tokens are validated before being written verbatim as JSON numbers.
            lines.append(f'    {{"model": {json.dumps(model)}, "elo": {elo}, "eq": {eq}}}{comma}')
        lines.append("  ]")
    else:
        lines.append('  "rows": []')
    return "\n".join([*lines, "}", ""])


def epoch_ms(value):
    if not re.fullmatch(r"0|[1-9][0-9]*", value) or int(value) > 2**63 - 1:
        raise argparse.ArgumentTypeError("graded-at must be a nonnegative signed 64-bit epoch millisecond integer")
    return int(value)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csv", type=Path)
    # Required rather than defaulted: the value is served to consumers as when
    # the operator graded, so a placeholder such as 0 would read as a real date
    # in 1970, and the current time would make two runs on the same sheet differ.
    parser.add_argument("--graded-at", type=epoch_ms, required=True,
                        help="epoch milliseconds when the operator finished grading")
    args = parser.parse_args()
    try:
        with args.csv.open(encoding="utf-8-sig", newline="") as source:
            rows = read_rows(source)
        DESTINATION.write_text(render(rows, args.graded_at), encoding="utf-8")
    except (OSError, ValueError, csv.Error) as error:
        parser.exit(1, f"{error}\n")
    print(f"Wrote {len(rows)} scored rows")


if __name__ == "__main__":
    main()
