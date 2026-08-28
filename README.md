# AQW Character Rendering Infrastructure

This repository owns the distributed `/char` rendering system: AWS CDK
infrastructure, orchestration handlers, Python rendering services, container
definitions, tests, and operational documentation.

The initial scaffold deliberately creates no chargeable AWS resources. The
system will be implemented in reviewed phases from
[`docs/aws-char-rendering-system.md`](docs/aws-char-rendering-system.md).

## Development environment

| Setting | Value |
| --- | --- |
| AWS account | `538522204887` (`aqw-char-dev`) |
| AWS Region | `us-west-2` |
| AWS CLI profile | `aqw-char-dev` |
| CDK language | TypeScript |
| Renderer language | Python 3.13, managed with `uv` |

Authenticate with temporary IAM Identity Center credentials:

```bash
aws sso login --profile aqw-char-dev
aws sts get-caller-identity --profile aqw-char-dev
```

Install and validate the TypeScript CDK application:

```bash
npm ci
npm run build
npm test
npm run synth -- --profile aqw-char-dev
```

Install and validate all Python workspace packages:

```bash
uv sync --all-packages
uv run --package aqw-char-renderer pytest services/renderer/tests
uv run --package aqw-char-renderer ruff check services/renderer
```

Do not run `cdk bootstrap` or `cdk deploy` casually. Both mutate the AWS
account, and deployed resources can incur charges. The intended bootstrap
environment is:

```text
aws://538522204887/us-west-2
```

## Repository layout

```text
bin/                  CDK application entry point
lib/                  stacks, constructs, and environment configuration
services/renderer/    Python rendering and Lambda/container code
test/                 CDK unit tests
docs/                 architecture and operating documentation
```

No AWS credentials, SSO cache files, downloaded SWFs, render frames, or final
animations belong in Git.
