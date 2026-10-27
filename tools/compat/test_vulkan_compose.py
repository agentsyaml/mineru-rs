import json
import os
from pathlib import Path
import shutil
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]


def compose_available():
    return bool(shutil.which("docker")) and subprocess.run(
        ["docker", "compose", "version"], capture_output=True, check=False
    ).returncode == 0


@unittest.skipUnless(compose_available(), "Docker Compose plugin is unavailable")
class VulkanComposeTest(unittest.TestCase):
    def config(self, profiles=(), compose_profiles=None, **overrides):
        env = {key: os.environ[key] for key in ("PATH", "HOME") if key in os.environ}
        env["COMPOSE_PROFILES"] = compose_profiles or ""
        env.update(overrides)
        args = ["docker", "compose", "--env-file", os.devnull]
        for profile in profiles:
            args.extend(("--profile", profile))
        args.extend(("-f", str(ROOT / "docker-compose.yaml"), "config", "--format", "json"))
        result = subprocess.run(
            args,
            cwd=ROOT, env=env, check=True, capture_output=True, text=True,
        )
        return json.loads(result.stdout)["services"]

    def test_default_services_and_vllm_are_unchanged(self):
        services = self.config(profiles=("vulkan", "vulkan-nvidia", "openai-server"))
        vllm = services["mineru-openai-server"]
        self.assertEqual(vllm["image"], "alexsuntop/mineru:3.4.2")
        self.assertEqual(vllm["profiles"], ["openai-server"])
        self.assertEqual(vllm["ports"][0]["published"], "30000")
        self.assertEqual(vllm["ports"][0]["host_ip"], "127.0.0.1")
        self.assertEqual(services["llama-server-vulkan"]["profiles"], ["vulkan"])
        self.assertEqual(services["llama-server-vulkan-nvidia"]["profiles"], ["vulkan-nvidia"])

    def test_backend_devices_and_safe_model_defaults(self):
        services = self.config(profiles=("vulkan", "vulkan-nvidia", "openai-server"))
        amd = services["llama-server-vulkan"]
        nvidia = services["llama-server-vulkan-nvidia"]
        self.assertEqual(amd["devices"][0]["source"], "/dev/dri")
        self.assertEqual(amd["devices"][0]["target"], "/dev/dri")
        self.assertNotIn("devices", nvidia)
        self.assertEqual(nvidia["deploy"]["resources"]["reservations"]["devices"][0]["driver"], "nvidia")
        self.assertEqual(nvidia["environment"]["NVIDIA_DRIVER_CAPABILITIES"], "graphics,utility")
        for service in (amd, nvidia):
            self.assertFalse(service.get("privileged", False))
            self.assertEqual(service["ports"][0]["published"], "30001")
            self.assertEqual(service["ports"][0]["host_ip"], "127.0.0.1")
            self.assertTrue(service["volumes"][0]["read_only"])
            self.assertFalse(service["volumes"][0].get("bind", {}).get("create_host_path", False))
            self.assertIn("/models/model.gguf", service["command"])
            self.assertIn("/models/mmproj.gguf", service["command"])
            self.assertIn("mineru-vlm", service["command"])

    def test_environment_overrides(self):
        services = self.config(profiles=("vulkan", "vulkan-nvidia"),
            MINERU_PORT_OVERRIDE_LLAMA="32123", LLAMA_MODEL_DIR="/srv/models",
            LLAMA_MODEL_FILE="vision.gguf", LLAMA_MMPROJ_FILE="projector.gguf",
            LLAMA_MODEL_ALIAS="custom-vlm", LLAMA_GPU_LAYERS="24",
            LLAMA_VULKAN_IMAGE="example/llama@sha256:abc",
            VULKAN_DRI_PATH="/dev/dri/by-path/gpu", VULKAN_NVIDIA_DEVICE_ID="2",
        )
        service = services["llama-server-vulkan"]
        self.assertEqual(service["image"], "example/llama@sha256:abc")
        self.assertEqual(service["ports"][0]["published"], "32123")
        self.assertEqual(service["devices"][0]["source"], "/dev/dri/by-path/gpu")
        self.assertIn("/models/vision.gguf", service["command"])
        self.assertIn("/models/projector.gguf", service["command"])
        self.assertIn("custom-vlm", service["command"])
        self.assertIn("24", service["command"])
        self.assertEqual(service["volumes"][0]["source"], "/srv/models")
        devices = services["llama-server-vulkan-nvidia"]["deploy"]["resources"]["reservations"]["devices"]
        self.assertEqual(devices[0]["device_ids"], ["2"])

    def test_vulkan_compose_profile_does_not_activate_vllm(self):
        services = self.config(compose_profiles="vulkan")
        self.assertEqual(set(services), {"llama-server-vulkan"})

    def test_nvidia_compose_profile_does_not_activate_other_providers(self):
        services = self.config(compose_profiles="vulkan-nvidia")
        self.assertEqual(set(services), {"llama-server-vulkan-nvidia"})


if __name__ == "__main__":
    unittest.main()
