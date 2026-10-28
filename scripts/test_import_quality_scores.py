"""Regression checks for importing the operator's grading sheet."""

import importlib.util
import io
import json
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location(
    "import_quality_scores", Path(__file__).with_name("import-quality-scores.py")
)
importer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(importer)


class ImportQualityScoresTests(unittest.TestCase):
    def test_decimal_text_nulls_sorting_and_aa_ignored(self):
        rows = importer.read_rows(io.StringIO(
            "model,ELO,EQ,AA\nz/m,1284,1599.800,not a number\na/m,,100.0,42\nb/m,,,1\n"
        ))
        output = importer.render(rows, 123)
        self.assertIn('"elo": 1284, "eq": 1599.800', output)
        self.assertIn('"elo": null, "eq": 100.0', output)
        self.assertEqual([row["model"] for row in json.loads(output)["rows"]], ["a/m", "z/m"])
        self.assertNotIn("AA", output)
        self.assertEqual(output, importer.render(importer.read_rows(io.StringIO(
            "model,ELO,EQ,AA\nz/m,1284,1599.800,not a number\na/m,,100.0,42\nb/m,,,1\n"
        )), 123))

    def test_duplicate_names_line_even_without_scores(self):
        with self.assertRaisesRegex(ValueError, "line 3: duplicate model 'a/m'"):
            importer.read_rows(io.StringIO("model,ELO,EQ\na/m,,\na/m,1,\n"))

    def test_rejects_non_decimal_and_invalid_keys(self):
        for value in ("NaN", "Infinity", "1e3", "+1", "01", "1.", " 1", ".5"):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "line 2: ELO"):
                importer.read_rows(io.StringIO(f"model,ELO,EQ\na/m,{value},\n"))
        for model in ("", "a", "a/ m"):
            with self.subTest(model=model), self.assertRaisesRegex(ValueError, "line 2: model"):
                importer.read_rows(io.StringIO(f"model,ELO,EQ\n{model},1,\n"))

    def test_empty_rows_and_epoch_validation(self):
        self.assertEqual(importer.read_rows(io.StringIO(
            "model,ELO,EQ\na/@cf/lab/model,1,\n"
        )), [("a/@cf/lab/model", "1", "null")])
        self.assertEqual(json.loads(importer.render([], 0))["rows"], [])
        self.assertEqual(importer.epoch_ms("123"), 123)
        for value in ("-1", "1.5", "9223372036854775808"):
            with self.subTest(value=value), self.assertRaises(Exception):
                importer.epoch_ms(value)


if __name__ == "__main__":
    unittest.main()
