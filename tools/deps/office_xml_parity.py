import shutil
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PARSER = ROOT / ".deps/office-xml041/office2pdf/src/parser"
HARNESS = ROOT / "tools/deps/parity-harness"


def main():
    with tempfile.TemporaryDirectory() as temporary:
        project = Path(temporary)
        source = project / "src"
        source.mkdir()
        shutil.copy2(HARNESS / "Cargo.toml", project / "Cargo.toml")
        shutil.copy2(HARNESS / "Cargo.lock", project / "Cargo.lock")
        shutil.copy2(PARSER / "xml_util.rs", source / "xml_util.rs")
        shutil.copy2(PARSER / "xml_util_tests.rs", source / "xml_util_tests.rs")
        (source / "lib.rs").write_text(
            "mod ir {\n"
            "    #[derive(Debug, PartialEq, Eq)]\n"
            "    pub(crate) struct Color(u8, u8, u8);\n"
            "    impl Color {\n"
            "        pub(crate) fn new(r: u8, g: u8, b: u8) -> Self { Self(r, g, b) }\n"
            "    }\n"
            "}\n"
            "mod xml_util;\n",
            encoding="utf-8",
        )
        manifest = str(project / "Cargo.toml")
        subprocess.run(["cargo", "+1.89.0", "fetch", "--locked", "--manifest-path", manifest], cwd=ROOT, check=True)
        result = subprocess.run(
            ["cargo", "+1.89.0", "test", "--locked", "--offline", "--manifest-path", manifest, "--lib"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        )
        output = result.stdout + result.stderr
        print(output, end="")
        if "22 passed" not in output:
            raise SystemExit("expected all 22 Office XML parity assertions")


if __name__ == "__main__":
    main()
