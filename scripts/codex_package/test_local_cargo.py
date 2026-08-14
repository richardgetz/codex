#!/usr/bin/env python3

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from run_local_cargo import cargo_target


class CargoTargetTest(unittest.TestCase):
    def test_explicit_target_wins_over_other_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_home = root / "cargo-home"
            cargo_home.mkdir()
            (cargo_home / "config").write_text(
                '[build]\ntarget = "x86_64-apple-darwin"\n'
            )

            self.assertEqual(
                cargo_target(
                    ["--target", "aarch64-apple-darwin"],
                    {"CARGO_HOME": str(cargo_home), "CARGO_BUILD_TARGET": "wrong"},
                    root,
                ),
                "aarch64-apple-darwin",
            )

    def test_command_line_config_target_is_used(self) -> None:
        self.assertEqual(
            cargo_target(
                ["--config", 'build.target="aarch64-apple-darwin"'],
                {},
                Path.cwd(),
            ),
            "aarch64-apple-darwin",
        )

    def test_legacy_config_file_wins_when_both_names_exist(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_home = root / "cargo-home"
            cargo_home.mkdir()
            (cargo_home / "config.toml").write_text(
                '[build]\ntarget = "x86_64-apple-darwin"\n'
            )
            (cargo_home / "config").write_text(
                '[build]\ntarget = "aarch64-apple-darwin"\n'
            )

            self.assertEqual(
                cargo_target([], {"CARGO_HOME": str(cargo_home)}, root),
                "aarch64-apple-darwin",
            )

    def test_multi_target_configuration_fails_instead_of_using_host(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_home = root / "cargo-home"
            cargo_home.mkdir()
            (cargo_home / "config.toml").write_text(
                'build.target = ["aarch64-apple-darwin", "x86_64-apple-darwin"]\n'
            )

            with self.assertRaisesRegex(RuntimeError, "multiple build targets"):
                cargo_target([], {"CARGO_HOME": str(cargo_home)}, root)

    def test_target_after_separator_is_not_a_cargo_option(self) -> None:
        target = cargo_target(
            ["run", "--", "--target", "program-argument"], {}, Path.cwd()
        )

        self.assertNotEqual(target, "program-argument")


if __name__ == "__main__":
    unittest.main()
