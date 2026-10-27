# Vulkan llama.cpp server

This Compose option runs the official `llama.cpp` Vulkan server as a local VLM
endpoint for MinerU. It supports AMD and Intel through Linux DRM, and NVIDIA
through the NVIDIA Container Toolkit. Docker Desktop on macOS does not forward
the host GPU to this Linux container; use a native Linux host.

## Requirements

- Docker Compose v2 and a working Linux Vulkan/DRM setup.
- For AMD/Intel, access to `/dev/dri` (or configure `VULKAN_DRI_PATH`).
- For NVIDIA, an installed/configured NVIDIA Container Toolkit.
- A local vision-language GGUF model and its matching multimodal projector
  (`mmproj`) file. The defaults expect `model.gguf` and `mmproj.gguf` in
  `./models`; this does not make arbitrary GGUF models compatible with MinerU.

The model directory is mounted read-only, and Compose fails if it is missing.
Set `LLAMA_MODEL_DIR` to its absolute path and set the file names to the exact
files in that directory:

```sh
export LLAMA_MODEL_DIR="/absolute/path/to/models"
export LLAMA_MODEL_FILE="vision-model.gguf"
export LLAMA_MMPROJ_FILE="vision-model-mmproj.gguf"
```

All provider services use explicit profiles. Select one backend and target its
service; a bare `docker compose up` starts no provider service.

To start the vLLM provider instead of Vulkan:

```sh
docker compose --profile openai-server up -d mineru-openai-server
```

Do not combine provider profiles unless you intend to run multiple providers.

AMD or Intel:

```sh
docker compose --profile vulkan up -d llama-server-vulkan
```

NVIDIA:

```sh
docker compose --profile vulkan-nvidia up -d llama-server-vulkan-nvidia
```

The default image reference is `ghcr.io/ggml-org/llama.cpp:server-vulkan`.
It is a floating tag; for repeatable deployments set `LLAMA_VULKAN_IMAGE` to a
verified image digest. No model is downloaded by Compose. `--n-gpu-layers auto`
requests automatic offload but does not guarantee that the entire model fits
in GPU memory or that all operations run on the GPU.

The current official image runs as root by default. Device access still depends
on host permissions and rootless/user-namespace settings. For a non-root user, add the host's numeric DRM
device group IDs with `group_add` in a Compose override. Do not hard-code a GID
that may differ on another host.

## Check the service

The explicit service command below overrides the normal model-serving command
and lists devices without downloading a model:

```sh
docker compose --profile vulkan run --rm --no-deps llama-server-vulkan --list-devices
```

After starting the selected service, check the OpenAI-compatible endpoint:

```sh
curl http://127.0.0.1:30001/v1/models
```

The image includes a health check for `http://localhost:8080/health`; Compose
uses it to report server readiness.

Confirm GPU offload in the container logs, then test a real image request with
the selected model and verify it follows the MinerU VLM protocol. A responsive
`/v1/models` endpoint alone does not establish model compatibility. See
[Docker images](https://github.com/ggml-org/llama.cpp/blob/master/docs/docker.md),
[build guidance](https://github.com/ggml-org/llama.cpp/blob/master/docs/build.md),
[multimodal behavior](https://github.com/ggml-org/llama.cpp/blob/master/docs/multimodal.md),
and upstream [llama.cpp server arguments](https://github.com/ggml-org/llama.cpp/blob/master/common/arg.cpp)
and [NVIDIA graphics container capabilities](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/docker-specialized.html#driver-capabilities).

For MinerU, configure the client as follows:

```sh
export MINERU_VL_SERVER="http://127.0.0.1:30001"
export MINERU_VL_MODEL_NAME="mineru-vlm"
```
