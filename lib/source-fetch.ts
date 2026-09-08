import * as cdk from 'aws-cdk-lib/core';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as s3 from 'aws-cdk-lib/aws-s3';
import * as ssm from 'aws-cdk-lib/aws-ssm';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as ecrAssets from 'aws-cdk-lib/aws-ecr-assets';
import * as logs from 'aws-cdk-lib/aws-logs';
import { Construct } from 'constructs';
import * as path from 'node:path';

export function sourceFetch(scope: Construct, id: string, props: {
  name: string; sourceBucket: s3.IBucket; workBucket: s3.IBucket;
  dataset: string; parameterName: string;
}): lambda.DockerImageFunction {
  if (!/^\/[A-Za-z0-9_./-]+$/.test(props.parameterName)) {
    throw new Error('brightDataConfigParameter must be an absolute SSM parameter name');
  }
  const fn = new lambda.DockerImageFunction(scope, id, {
    functionName: props.name,
    architecture: lambda.Architecture.ARM_64,
    code: lambda.DockerImageCode.fromImageAsset(path.join(__dirname, '..', 'services', 'renderer'), {
      file: 'Dockerfile.fetch', platform: ecrAssets.Platform.LINUX_ARM64,
      cmd: ['aqw_char_renderer.fetch_sources.handler'],
      exclude: ['tests', '**/__pycache__', '**/.pytest_cache', '**/.venv'],
    }),
    memorySize: 1024, timeout: cdk.Duration.minutes(5),
    logGroup: new logs.LogGroup(scope, `${id}Logs`, {
      logGroupName: `/aws/lambda/${props.name}`, retention: logs.RetentionDays.ONE_MONTH,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    }),
    environment: {
      CHAR_RENDER_SOURCE_BUCKET: props.sourceBucket.bucketName,
      CHAR_RENDER_WORK_BUCKET: props.workBucket.bucketName,
      CHAR_RENDER_ASSET_DATASET_VERSION: props.dataset,
      AQW_BRIGHTDATA_CONFIG_PARAMETER: props.parameterName,
      CHAR_RENDER_OFFICIAL_ASSET_TIMEOUT_SECONDS: '30',
    },
  });
  props.sourceBucket.grantRead(fn);
  props.workBucket.grantRead(fn, 'jobs/*');
  fn.addToRolePolicy(new iam.PolicyStatement({ actions: ['s3:PutObject'], resources: [
    props.sourceBucket.arnForObjects(`dynamic-assets/${props.dataset}/*`),
    props.workBucket.arnForObjects('jobs/*/fetch/*'),
  ] }));
  ssm.StringParameter.fromSecureStringParameterAttributes(scope, `${id}Proxy`, {
    parameterName: props.parameterName,
  }).grantRead(fn);
  return fn;
}

export class SourceFetchTestStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props: cdk.StackProps & {
    stageName: string; dataset: string;
  }) {
    super(scope, id, props);
    const sourceName = this.node.tryGetContext('sourceBucketName');
    const workName = this.node.tryGetContext('workBucketName');
    if (!sourceName || !workName) throw new Error('Fetch test requires sourceBucketName and workBucketName context');
    const fn = sourceFetch(this, 'SourceFetch', {
      name: `aqw-char-${props.stageName}-source-fetch-test`,
      sourceBucket: s3.Bucket.fromBucketName(this, 'SourceBucket', sourceName),
      workBucket: s3.Bucket.fromBucketName(this, 'WorkBucket', workName),
      dataset: props.dataset,
      parameterName: this.node.tryGetContext('brightDataConfigParameter') ?? `/aqw-char/${props.stageName}/brightdata`,
    });
    new cdk.CfnOutput(this, 'SourceFetchFunctionName', { value: fn.functionName });
  }
}
