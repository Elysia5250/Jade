"""Pretty-printed Envoy JSON reports stay parseable with a log prefix."""

import unittest

from envoy_agent import EnvoyHarborAgent


class ReportParseTests(unittest.TestCase):
    def test_pretty_json_with_a_log_prefix(self) -> None:
        stdout = 'noise before the report\n{\n  "ok": true,\n  "input_tokens": 3\n}\n'
        report = EnvoyHarborAgent._parse_report(stdout)
        self.assertTrue(report["ok"])
        self.assertEqual(report["input_tokens"], 3)

    def test_last_object_wins(self) -> None:
        stdout = '{"ok": false}\n{"ok": true, "output_tokens": 4}\n'
        report = EnvoyHarborAgent._parse_report(stdout)
        self.assertTrue(report["ok"])
        self.assertEqual(report["output_tokens"], 4)

    def test_build_command_reads_the_prompt_from_stdin(self) -> None:
        agent = EnvoyHarborAgent(logs_dir="/tmp/envoy-agent-test")
        command = agent.build_command("hello -n\nworld", "/app")
        self.assertIn("-p -", command)
        self.assertNotIn("hello", command)
        self.assertIn("--cwd /app", command.replace("'", ""))


if __name__ == "__main__":
    unittest.main()
