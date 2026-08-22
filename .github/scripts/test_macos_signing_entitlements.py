import plistlib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SIGNING_DIR = ROOT / ".github" / "scripts" / "macos-signing"
ALLOW_JIT = "com.apple.security.cs.allow-jit"
ALLOW_UNSIGNED_EXECUTABLE_MEMORY = (
    "com.apple.security.cs.allow-unsigned-executable-memory"
)


class MacosSigningEntitlementsTest(unittest.TestCase):
    def load(self, binary: str) -> dict[str, bool]:
        path = SIGNING_DIR / f"{binary}.entitlements.plist"
        with path.open("rb") as file:
            return plistlib.load(file)

    def test_v8_binaries_allow_unsigned_executable_memory(self) -> None:
        expected = {
            ALLOW_JIT: True,
            ALLOW_UNSIGNED_EXECUTABLE_MEMORY: True,
        }
        for binary in ["codex", "codex-app-server", "codex-code-mode-host"]:
            with self.subTest(binary=binary):
                self.assertEqual(self.load(binary), expected)

    def test_fork_release_applies_v8_entitlements_before_packaging(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "fork-release.yml").read_text()

        self.assertIn(
            'entitlements_file="${GITHUB_WORKSPACE}/.github/scripts/macos-signing/${binary}.entitlements.plist"',
            workflow,
        )
        self.assertIn("for binary in codex codex-code-mode-host; do", workflow)
        self.assertIn('--entitlements "${entitlements_file}"', workflow)
        self.assertIn('verify_signed_binary "${binary_path}" "${binary}"', workflow)
        self.assertIn(
            'verify_signed_binary "${package_dir}/bin/${binary}" "${binary}"',
            workflow,
        )

    def test_responses_proxy_keeps_existing_entitlements(self) -> None:
        self.assertEqual(
            self.load("codex-responses-api-proxy"),
            {ALLOW_JIT: True},
        )


if __name__ == "__main__":
    unittest.main()
