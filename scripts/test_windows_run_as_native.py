"""Pure fixture validation tests. Never starts Windows or Terrarium commands."""
import unittest
from windows_run_as_native import literal, validate_fixture


class NativeFixtureValidation(unittest.TestCase):
    def test_literals_escape_quotes_and_reject_control_characters(self):
        self.assertEqual(literal("C:\\a'b"), "'C:\\a''b'")
        for value in ("a\0b", "a\nb", "a\rb"):
            with self.assertRaises(ValueError):
                literal(value)

    def test_missing_inventory_fails_closed(self):
        for fixture in ({}, {"namespace": "ter272"}, {"daemon_url": "https://example.invalid"}):
            with self.assertRaisesRegex(ValueError, "incomplete"):
                validate_fixture(fixture)
