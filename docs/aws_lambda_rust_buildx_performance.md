# Speeding Up Rust Deployments to AWS Lambda with Docker Buildx

**Target setup:** Apple Silicon (M1/M2/M3/M4) Mac -> AWS Lambda `arm64` -> container image in ECR

This guide focuses on **deployment iteration speed**: Rust compile time, Docker/Buildx time, image push time, Lambda image-update time, and cold-start time. These are separate phases and should be diagnosed separately.

---

## TL;DR for an M1 Mac Deploying to ARM64 Lambda

Your machine and Lambda have matching CPU architectures, so a normal `linux/arm64` build should be **native ARM**, not QEMU-emulated. The highest-value changes are usually:

1. Build **only** `linux/arm64`.
2. Use `--provenance=false` for Lambda container builds.
3. Use `docker buildx build --push` directly; avoid `--load` followed by a separate `docker push` in the deploy path.
4. Preserve Cargo registry, Git, and `target/` caches with BuildKit cache mounts.
5. Keep the Docker build context tiny with `.dockerignore`.
6. Do not `COPY . .` before dependency/cache-friendly steps.
7. Keep the same Buildx builder; do not recreate/prune it every deployment.
8. Use a registry-backed BuildKit cache if local caches are frequently lost or deployments also run in CI.
9. Use a multi-stage image and copy only the final Rust executable into the Lambda runtime image.
10. If you do not actually need a container image, consider **Cargo Lambda + ZIP deployment**. For Rust, this often produces a much tighter edit/build/deploy loop because there is no container image to assemble and push.

A good baseline build command is:

```bash
docker buildx build \
  --platform linux/arm64 \
  --provenance=false \
  --push \
  -t "$ECR_REPO:$TAG" \
  .
```

AWS Lambda requires a container image to target a **single architecture**; do not publish an `amd64,arm64` multi-platform manifest for the Lambda function.

---

# 1. First Determine Which Part Is Slow

Do not treat "deploy is slow" as one operation. Time these separately:

| Phase | Typical symptom | Main fixes |
|---|---|---|
| Docker context transfer | Delay before meaningful build steps | `.dockerignore`, smaller context |
| Rust compilation | `cargo build --release` dominates | Cargo/target cache, fewer invalidations, faster release profile |
| Docker export | Long `exporting layers` / compression | Smaller final image, fewer/lighter layers |
| ECR push | Long `pushing layers` | Smaller changed layers, direct `--push`, network/region |
| Lambda update | Function remains `Pending` / updating | Smaller image, accept Lambda optimization latency, ZIP if containers unnecessary |
| Cold start | First request is slow after deployment/idle | Reduce initialization work, image/binary size, dependencies, network setup |

Run Buildx with plain progress so timing is visible:

```bash
time docker buildx build \
  --progress=plain \
  --platform linux/arm64 \
  --provenance=false \
  --push \
  -t "$ECR_REPO:$TAG" \
  .
```

Look for the step consuming most of the wall-clock time.

---

# 2. M1 -> ARM64 Lambda: Avoid Accidentally Reintroducing QEMU

Because an M1 Mac is ARM64 and your Lambda function is ARM64, this is a favorable setup.

Use:

```bash
--platform linux/arm64
```

Do **not** build both architectures:

```bash
# Bad for a Lambda image and slower:
--platform linux/amd64,linux/arm64
```

Also inspect the Dockerfile for hard-coded x86 stages such as:

```dockerfile
FROM --platform=linux/amd64 rust:...
```

That forces x86 emulation on your ARM Mac and Rust compilation can become dramatically slower.

Check your builder:

```bash
docker buildx inspect --bootstrap
```

And verify the target environment if necessary:

```bash
docker run --rm --platform linux/arm64 alpine uname -m
```

Expected output is normally:

```text
aarch64
```

Docker documents that QEMU emulation can be substantially slower for CPU-heavy work such as compilation and compression. In your M1 -> ARM64 case, there is normally no reason to use it.

---

# 3. Use `--push` Directly Instead of `--load` + `docker push`

A common slow path is:

```bash
docker buildx build --load ...
docker tag ...
docker push ...
```

`--load` exports the image into the local Docker image store. If the only purpose is deployment to ECR, that extra export/load is unnecessary.

Prefer:

```bash
docker buildx build \
  --platform linux/arm64 \
  --provenance=false \
  --push \
  -t "$ECR_REPO:$TAG" \
  .
```

BuildKit can build and push directly to the registry.

Use `--load` only when you actually need the image in the local Docker image store for local testing.

---

# 4. Preserve Cargo Caches Inside BuildKit

Rust recompilation is often the largest cost.

Use BuildKit cache mounts for:

- Cargo registry downloads
- Cargo Git checkouts
- `target/` compiled artifacts

Example:

```dockerfile
# syntax=docker/dockerfile:1

FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS build

RUN dnf install -y \
      gcc \
      gcc-c++ \
      make \
      curl \
      pkgconfig \
    && dnf clean all

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal

ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /app

# Copy manifests before source so dependency download metadata is stable.
COPY Cargo.toml Cargo.lock ./

RUN --mount=type=cache,id=cargo-registry-arm64,target=/root/.cargo/registry \
    --mount=type=cache,id=cargo-git-arm64,target=/root/.cargo/git \
    cargo fetch --locked

# Copy source only after dependency-fetch layer.
COPY src ./src

RUN --mount=type=cache,id=cargo-registry-arm64,target=/root/.cargo/registry \
    --mount=type=cache,id=cargo-git-arm64,target=/root/.cargo/git \
    --mount=type=cache,id=cargo-target-arm64,target=/app/target \
    cargo build --release --locked \
    && mkdir -p /out \
    && cp /app/target/release/my-lambda /out/bootstrap

FROM public.ecr.aws/lambda/provided:al2023
COPY --from=build /out/bootstrap /var/runtime/bootstrap
ENTRYPOINT ["/var/runtime/bootstrap"]
```

Replace `my-lambda` with the actual Cargo binary name.

## Why the `target/` cache matters

Even if changing `src/*.rs` invalidates the Docker `RUN cargo build` layer, Cargo can reuse compiled dependencies from the cached `target/` directory instead of rebuilding the dependency graph from scratch.

Docker's current Rust build guidance also recommends cache mounts for Cargo and `target/` to improve subsequent build speed.

---

# 5. Workspace Pitfall: Copy All Manifests Needed by `cargo fetch`

If this is a Cargo workspace, doing only:

```dockerfile
COPY Cargo.toml Cargo.lock ./
RUN cargo fetch --locked
```

may fail or provide poor cache behavior because workspace members referenced by the root manifest are missing.

Copy the workspace manifests first, for example:

```dockerfile
COPY Cargo.toml Cargo.lock ./
COPY crates/api/Cargo.toml crates/api/Cargo.toml
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/db/Cargo.toml crates/db/Cargo.toml
```

Then run `cargo fetch`.

For large workspaces, **cargo-chef** is another common option for separating the dependency build recipe from application source changes. It can help when Docker-layer/cache management becomes awkward, but it adds another tool to maintain. BuildKit `target/` cache mounts are often sufficient first.

---

# 6. Do Not Send `target/` to Docker

A bad `.dockerignore` can make every deployment feel inexplicably slow.

Recommended baseline:

```gitignore
# .dockerignore
target
.git
.gitignore
.DS_Store
node_modules
coverage
*.log
.env
.env.*
```

Add any large local data, fixtures, model files, generated files, screenshots, caches, or build outputs that the Lambda image does not need.

The `target/` directory can be gigabytes. It should almost never be part of the Docker build context.

You can often spot this problem at the beginning of Buildx output:

```text
transferring context: ...
```

If that transfer is hundreds of MB or GB, fix the build context first.

---

# 7. Avoid `COPY . .` Too Early

This pattern destroys useful Docker cache locality:

```dockerfile
COPY . .
RUN cargo build --release
```

Changing a README, test file, source file, config file, or unrelated asset can invalidate everything after `COPY . .`.

Prefer a dependency-first structure:

```dockerfile
COPY Cargo.toml Cargo.lock ./
RUN ... cargo fetch --locked

COPY src ./src
RUN ... cargo build --release --locked
```

For a workspace, copy the workspace manifests before source as described above.

---

# 8. Keep the Same Buildx Builder and Cache

BuildKit's normal cache belongs to the builder instance.

These habits can accidentally make every build cold:

```bash
docker buildx rm mybuilder
docker builder prune -a
docker system prune -a
```

Also avoid scripts that create a new random Buildx builder on every deployment.

Inspect builders:

```bash
docker buildx ls
```

Inspect cache usage:

```bash
docker buildx du
```

If you routinely purge Docker Desktop data, restart from ephemeral CI runners, or deploy from multiple machines, use an **external cache**.

---

# 9. Use an ECR-Backed BuildKit Cache

Buildx can import/export cache through a registry.

Example using the same ECR repository with a separate `buildcache` tag:

```bash
docker buildx build \
  --platform linux/arm64 \
  --provenance=false \
  --push \
  -t "$ECR_REPO:$TAG" \
  --cache-from "type=registry,ref=$ECR_REPO:buildcache" \
  --cache-to "type=registry,ref=$ECR_REPO:buildcache,mode=max,oci-mediatypes=true,image-manifest=true" \
  .
```

Why the extra ECR cache options?

Docker's registry-cache documentation notes that registries such as ECR need a **single image manifest** rather than an OCI image index for this cache representation; `image-manifest=true` handles that.

## When registry cache helps

It is especially useful when:

- CI uses ephemeral builders.
- You switch machines.
- Docker Desktop caches are frequently pruned.
- Multiple developers share the same dependency graph.

## When it can hurt

A huge `mode=max` cache also has to be uploaded/downloaded. If local builds already have a persistent warm BuildKit cache, a registry cache may add network overhead without much benefit.

Measure it.

---

# 10. Use Multi-Stage Builds and Keep the Runtime Image Small

Do not ship the Rust compiler, Cargo registry, object files, headers, package manager caches, or build tools in the final Lambda image.

The final stage should ideally contain approximately:

- the Lambda/AL2023 runtime base
- the compiled `bootstrap` executable
- only the shared libraries/certificates/config files actually required at runtime

AWS explicitly recommends multi-stage builds for reducing Lambda container image size and activation time.

Example final stage:

```dockerfile
FROM public.ecr.aws/lambda/provided:al2023
COPY --from=build /out/bootstrap /var/runtime/bootstrap
ENTRYPOINT ["/var/runtime/bootstrap"]
```

Amazon Linux 2023's Lambda base is substantially smaller than the older AL2 runtime base, so prefer `provided.al2023` unless you have a compatibility reason not to.

---

# 11. Rust/glibc Compatibility Pitfall

A deployment can be fast and still produce a broken Lambda if the executable was built against a newer glibc than the final image provides.

For example, compiling in an arbitrary recent Debian/Ubuntu builder and copying the dynamically linked executable into `provided.al2023` can result in runtime errors such as missing `GLIBC_2.xx` symbols.

Safer approaches:

1. Build on an AL2023-compatible userspace if the final image is AL2023.
2. Use Cargo Lambda's supported ARM64 build path for ZIP deployments.
3. Use musl/static linking where appropriate, while verifying native dependencies such as OpenSSL.
4. Ensure any dynamically linked native libraries required by the binary also exist in the final image.

AWS's Rust runtime project currently recommends Amazon Linux 2023 / `provided.al2023` for Rust Lambda builds.

---

# 12. Avoid Expensive Rust Release Settings During Every Dev Deploy

Some `Cargo.toml` release optimizations reduce binary size/runtime overhead but make compilation much slower.

Potential build-time offenders:

```toml
[profile.release]
lto = true
codegen-units = 1
```

Full LTO and one codegen unit can substantially increase link/compile time.

For rapid deployment iteration, consider a separate profile:

```toml
[profile.deploy-dev]
inherits = "release"
lto = false
codegen-units = 16
strip = "symbols"
panic = "abort"
```

Build it with:

```bash
cargo build --profile deploy-dev
```

Then reserve your most aggressive production optimization profile for actual production releases.

Important: benchmark before changing profiles. Rust Lambda runtime performance is often dominated by I/O and initialization rather than tiny differences in generated machine code.

---

# 13. Strip Debug Symbols

Rust binaries can contain a large amount of symbol/debug metadata.

For a Lambda production binary:

```toml
[profile.release]
strip = "symbols"
```

or strip the final binary explicitly if appropriate for your debugging workflow.

Benefits:

- smaller Docker layer
- less data to upload to ECR
- less data for Lambda to handle

Tradeoff:

- poorer native crash/backtrace diagnostics unless you preserve symbols separately

Do not blindly combine stripping with every possible LTO/size optimization if your primary complaint is **build time**.

---

# 14. Reduce Rust Dependency Weight

Large Rust graphs can make both clean builds and links slow.

Common issues:

- importing broad crates when only a small component is needed
- unnecessary default features
- multiple TLS stacks
- duplicate major versions of the same dependency
- native dependencies that trigger C/C++ compilation
- large AWS SDK service dependency sets

Inspect the dependency tree:

```bash
cargo tree
```

Look for duplicate versions:

```bash
cargo tree -d
```

For crates with expensive default features, selectively disabling defaults can help:

```toml
some-crate = { version = "...", default-features = false, features = ["needed-feature"] }
```

Do this only when you understand the feature implications.

---

# 15. Native Dependencies Can Destroy Build Cache Performance

Crates wrapping native libraries can trigger expensive builds through `build.rs`, CMake, `cc`, bindgen, OpenSSL, database clients, compression libraries, etc.

Watch `--progress=plain` output and Cargo output for repeated native compilation.

If one native crate recompiles every time, investigate:

- environment variables changing between builds
- generated headers/files changing timestamps/content
- build scripts depending on unstable paths
- missing `target/` cache mount
- feature changes
- target triple changes
- switching Rust versions/toolchains

A stable Rust version and stable base image improve cache hit rate.

---

# 16. Pin Toolchains and Base Images Deliberately

Using moving tags can cause cache misses and unexpected rebuilds:

```dockerfile
FROM rust:latest
```

Prefer a deliberate Rust version and base:

```text
Rust 1.xx + AL2023-compatible builder
```

You do not necessarily have to pin every image to a digest during local iteration, but frequently changing base/toolchain versions invalidates expensive layers.

Also check whether your deploy script runs:

```bash
docker buildx build --pull ...
```

on every build. `--pull` tells BuildKit to check for newer base images and may make the deployment path more network-dependent.

---

# 17. Do Not Use `--no-cache` in Normal Deployment

This seems obvious, but deployment wrappers often contain it after debugging a stale-image issue:

```bash
docker buildx build --no-cache ...
```

That defeats Docker layer cache.

Likewise, routinely pruning BuildKit immediately before deployment guarantees slow Rust builds.

---

# 18. ECR Push Slowness

If compilation is fast but deployment still waits during image push, inspect which layers change.

## Common causes

### Large final binary

Strip symbols and remove unused features.

### Build artifacts accidentally copied into final image

Check the Dockerfile for things such as:

```dockerfile
COPY --from=build /app /app
```

Instead copy only the executable and required runtime assets.

### Huge mutable layer

If one `COPY` layer contains many frequently changing files, Docker must upload a new layer whenever any of them change.

### `--load` before push

Skip local image loading and use Buildx `--push` directly.

### Slow internet uplink

Your Mac is pushing image layers from your local network to ECR. A 500 MB changed layer on a weak upstream connection will dominate deployment regardless of Rust performance.

### ECR in an unexpected region

Keep the ECR repository in the same AWS Region as the Lambda function; Lambda requires this for its normal container-image workflow, and it also avoids needless architecture/deployment confusion.

---

# 19. Lambda Has Its Own Image Optimization Step

After a new or updated container image is assigned to a Lambda function, Lambda performs an internal optimization step before the function becomes active.

AWS states that this can take **a few seconds**, during which the function remains `Pending`.

This is not Docker compilation and is not an ECR push problem.

Check status with:

```bash
aws lambda get-function-configuration \
  --function-name "$FUNCTION_NAME" \
  --query '{State:State,LastUpdateStatus:LastUpdateStatus,Reason:LastUpdateStatusReason}'
```

If your build and push finish quickly but the deploy command waits here, you are measuring Lambda activation/optimization rather than Buildx performance.

---

# 20. Avoid Excessive Image Layers / Manifest Metadata

AWS recommends keeping the image manifest below **25,400 bytes** for optimal performance and suggests minimizing layer count and annotations.

This does **not** mean "squash everything into one giant layer". Good Docker caching still matters.

The practical goal is:

- sensible number of stable build layers
- very small runtime-stage layer set
- no unnecessary annotations/attestations

Also use:

```bash
--provenance=false
```

AWS's Lambda container build examples explicitly require this option for Buildx-built Lambda images.

---

# 21. Cold Start Is Different from Deployment Time

If the image deploys quickly but the first request is slow, investigate Lambda initialization.

Look for CloudWatch `REPORT` lines containing:

```text
Init Duration: ... ms
```

Common Rust cold-start causes include:

- constructing large SDK clients/configuration eagerly
- network calls during initialization
- Secrets Manager / Parameter Store calls before the handler starts
- database connection establishment
- TLS certificate/native library initialization
- loading large embedded assets/configuration
- DNS/VPC/network interface effects

Rust itself generally has a small runtime footprint, so application initialization frequently matters more than the language runtime.

Move reusable initialization outside the request handler when it should be reused across warm invocations, but avoid doing unnecessary remote work at process startup.

---

# 22. Lambda Memory Also Controls CPU

If your "cold start" or initialization performs CPU-heavy work, Lambda memory configuration matters because CPU allocation scales with configured memory.

A low-memory Lambda can make initialization and CPU-heavy request work slower.

Test several memory sizes rather than assuming the smallest setting is cheapest. A higher-memory function can sometimes complete sufficiently faster to have comparable or lower total compute cost.

This affects **runtime/cold start**, not your local Docker compilation time.

---

# 23. Strong Option for Rust: Skip Docker Images Entirely

If the Lambda does not require OS packages, custom binaries, browser dependencies, ML libraries, or other container-specific filesystem requirements, a ZIP deployment is worth testing.

AWS documents Cargo Lambda for Rust, including ARM64 builds:

```bash
cargo lambda build --release --arm64
```

To build the ZIP package:

```bash
cargo lambda build --release --arm64 --output-format zip
```

Then deploy with Cargo Lambda:

```bash
cargo lambda deploy my-function
```

Or use the resulting `bootstrap.zip` with your normal IaC/AWS CLI flow.

## Why ZIP can be faster for iteration

It removes several container-only steps:

```text
Docker context
-> Docker build graph
-> runtime image assembly
-> image layer compression
-> ECR image push
-> Lambda container image optimization
```

and replaces them roughly with:

```text
Rust compile
-> ZIP binary
-> upload Lambda package
```

This is often the first architecture change to test if "deployment speed" is more important than having a container.

Do not assume ZIP will automatically improve handler execution latency; this recommendation is mainly about the **build/deploy loop**.

---

# 24. Suggested Fast Dockerfile for M1 -> ARM64 Lambda

This version is intentionally straightforward and cache-oriented.

```dockerfile
# syntax=docker/dockerfile:1

# Build in AL2023 to keep libc compatibility predictable with the final image.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS build

RUN dnf install -y gcc gcc-c++ make curl pkgconfig \
    && dnf clean all

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal

ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /app

# Stable dependency metadata first.
COPY Cargo.toml Cargo.lock ./

# For workspaces, COPY each member Cargo.toml here as well.

RUN --mount=type=cache,id=cargo-registry-arm64,target=/root/.cargo/registry \
    --mount=type=cache,id=cargo-git-arm64,target=/root/.cargo/git \
    cargo fetch --locked

# Source changes should happen after dependency metadata.
COPY src ./src

RUN --mount=type=cache,id=cargo-registry-arm64,target=/root/.cargo/registry \
    --mount=type=cache,id=cargo-git-arm64,target=/root/.cargo/git \
    --mount=type=cache,id=cargo-target-arm64,target=/app/target \
    cargo build --release --locked \
    && mkdir -p /out \
    && cp target/release/my-lambda /out/bootstrap

# Minimal Lambda runtime stage.
FROM public.ecr.aws/lambda/provided:al2023
COPY --from=build /out/bootstrap /var/runtime/bootstrap
ENTRYPOINT ["/var/runtime/bootstrap"]
```

Then:

```bash
docker buildx build \
  --platform linux/arm64 \
  --provenance=false \
  --progress=plain \
  --push \
  -t "$ECR_REPO:$TAG" \
  --cache-from "type=registry,ref=$ECR_REPO:buildcache" \
  --cache-to "type=registry,ref=$ECR_REPO:buildcache,mode=max,oci-mediatypes=true,image-manifest=true" \
  .
```

If your local BuildKit cache is persistent and fast, test without the two registry cache arguments too. Remote cache transfer can itself cost time.

---

# 25. Suggested `.dockerignore`

```gitignore
target/
.git/
.github/
.DS_Store
.env
.env.*
node_modules/
coverage/
*.log
*.tmp
*.profraw
*.profdata
```

Add project-specific large directories.

---

# 26. Example Fast Development Release Profile

If production `release` linking is slow because of aggressive optimization:

```toml
[profile.deploy-dev]
inherits = "release"
lto = false
codegen-units = 16
strip = "symbols"
panic = "abort"
```

Then change the Docker build line to:

```dockerfile
cargo build --profile deploy-dev --locked
```

and copy from:

```text
target/deploy-dev/my-lambda
```

Keep normal production optimization for releases where runtime performance/size has been measured to justify the extra compile time.

---

# 27. Diagnostic Checklist

Run through these in order.

## Architecture

- [ ] Lambda architecture is `arm64`.
- [ ] Buildx command uses only `--platform linux/arm64`.
- [ ] No Dockerfile stage forces `linux/amd64`.
- [ ] You are not building a multi-architecture manifest.

## Buildx

- [ ] Use `--progress=plain` and identify the actual slow step.
- [ ] Do not use `--no-cache`.
- [ ] Do not recreate the builder every run.
- [ ] Do not prune BuildKit before normal deployments.
- [ ] Use `--push` instead of `--load` + `docker push`.
- [ ] Use `--provenance=false` for Lambda.

## Docker context

- [ ] `target/` is ignored.
- [ ] `.git/` is ignored.
- [ ] Large test/generated assets are ignored.
- [ ] Build context transfer is small.

## Cargo

- [ ] Cargo registry is cache-mounted.
- [ ] Cargo Git directory is cache-mounted.
- [ ] `target/` is cache-mounted.
- [ ] `Cargo.lock` is committed and `--locked` is used.
- [ ] Rust version is reasonably stable.
- [ ] Dependency features are not unnecessarily huge.
- [ ] Full LTO / `codegen-units=1` is not used for every dev deployment unless needed.

## Runtime image

- [ ] Multi-stage build.
- [ ] Compiler/build tools are not in final image.
- [ ] Only executable + actual runtime assets are copied.
- [ ] Final base is preferably AL2023 if compatible with the application.
- [ ] Native shared libraries used by the Rust binary exist in the runtime image.

## Push/update

- [ ] ECR push time is measured separately from compile time.
- [ ] Lambda `Pending`/`LastUpdateStatus` time is measured separately.
- [ ] Cold-start `Init Duration` is measured separately from deployment.

---

# 28. What I Would Change First in Your Exact Setup

For **M1 Mac -> ARM64 Lambda -> Docker Buildx**, I would do these first, in this order:

1. Confirm the command is exactly single-platform ARM64:

   ```bash
   --platform linux/arm64
   ```

2. Search the Dockerfile for any `linux/amd64` / `x86_64` stage.

3. Add:

   ```bash
   --progress=plain
   ```

   and determine whether the actual problem is Cargo, Docker export, push, or Lambda update.

4. If `cargo build` is slow, cache:

   ```text
   /root/.cargo/registry
   /root/.cargo/git
   /app/target
   ```

5. Ensure `target/` and `.git/` are in `.dockerignore`.

6. Replace `--load` + `docker push` with Buildx `--push` if you currently use both.

7. Keep the final runtime stage to AL2023 + the Rust executable.

8. If build caches disappear between deployments, add the ECR registry cache.

9. If the container exists only because "Lambda can use Docker," test a Cargo Lambda ARM64 ZIP deployment instead. It is usually the largest simplification of the Rust deployment path.

---

# 29. References

- AWS Lambda: Create a function using a container image
  https://docs.aws.amazon.com/lambda/latest/dg/images-create.html

- AWS Lambda: Selecting/configuring arm64 architecture
  https://docs.aws.amazon.com/lambda/latest/dg/foundation-arch.html

- AWS Lambda: Rust ZIP deployment / Cargo Lambda
  https://docs.aws.amazon.com/lambda/latest/dg/rust-package.html

- AWS Lambda Rust Runtime project
  https://github.com/aws/aws-lambda-rust-runtime

- Amazon Linux 2023 on Lambda
  https://docs.aws.amazon.com/linux/al2023/ug/lambda.html

- Docker: Multi-platform builds / QEMU performance
  https://docs.docker.com/build/building/multi-platform/

- Docker: Optimize build cache usage
  https://docs.docker.com/build/cache/optimize/

- Docker: Registry cache backend
  https://docs.docker.com/build/cache/backends/registry/

- Docker: Rust language-specific build guide
  https://docs.docker.com/guides/rust/

---

## Bottom Line

On an M1 Mac targeting ARM64 Lambda, **architecture emulation should not be your main problem** as long as the complete build stays on `linux/arm64`.

For Rust, the most common deployment-time killers are:

1. losing the Cargo `target/` cache,
2. invalidating Docker cache too early,
3. sending a huge build context,
4. loading an image locally before pushing it,
5. pushing a large runtime image,
6. doing full-LTO production builds on every edit,
7. losing the Buildx builder/cache between runs,
8. confusing Lambda's post-push image optimization with local build time.

If containers are not providing a real requirement, **Cargo Lambda ARM64 ZIP deployment is the alternative I would benchmark against your Docker path first**.
