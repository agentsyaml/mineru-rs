import unittest
from importlib.metadata import distribution


class CommandAliasesTest(unittest.TestCase):
    def test_project_scripts(self):
        scripts = {
            entry_point.name: entry_point.value
            for entry_point in distribution("mineru-rs").entry_points
            if entry_point.group == "console_scripts"
        }

        self.assertEqual(
            scripts,
            {
                "mineru": "mineru_rs._cli:main",
                "mineru-rs": "mineru_rs._cli:main",
            },
        )


if __name__ == "__main__":
    unittest.main()
