import * as cdk from 'aws-cdk-lib/core';
import { Match, Template } from 'aws-cdk-lib/assertions';
import { getEnvironmentConfig } from '../lib/config/environment';
import { AqwCharRenderingInfraStack } from '../lib/aqw-char-rendering-infra-stack';

test('dev environment targets the dedicated account and exposes tuning in one config', () => {
  const environment = getEnvironmentConfig('dev');
  expect(environment.account).toBe('538522204887');
  expect(environment.region).toBe('us-west-2');
  expect(environment.stage).toBe('dev');
  expect(environment.tuning.prepareExportConcurrency).toBe(8);
  expect(environment.tuning.render).toMatchObject({
    rendererVersion: 'v14',
    maxSize: 2048,
    zoom: 1,
    completeLoop: true,
    maxFrames: 120,
    framesPerRenderLambda: 1,
    sourceBundleFrameCount: 4,
    mapConcurrency: 300,
    webpQuality: 85,
    allowOfficialAssetFallback: true,
  });
});

test('unknown environments fail closed', () => {
  expect(() => getEnvironmentConfig('production')).toThrow('Unknown environment');
});

function synthesize(): Template {
  const environment = getEnvironmentConfig('dev');
  const app = new cdk.App();
  const stack = new AqwCharRenderingInfraStack(app, 'TestStack', {
    env: { account: environment.account, region: environment.region },
    stageName: environment.stage,
    tuning: environment.tuning,
  });
  return Template.fromStack(stack);
}

test('stack contains the complete private rendering pipeline', () => {
  const template = synthesize();
  template.resourceCountIs('AWS::S3::Bucket', 2);
  template.resourceCountIs('AWS::SQS::Queue', 4);
  template.resourceCountIs('AWS::DynamoDB::Table', 1);
  template.resourceCountIs('AWS::Lambda::Function', 7);
  template.resourceCountIs('AWS::StepFunctions::StateMachine', 1);
  template.resourceCountIs('AWS::CloudFront::Distribution', 1);
  template.resourceCountIs('AWS::SSM::Parameter', 2);
  template.resourceCountIs('AWS::Budgets::Budget', 1);
});

test('Lambda request defaults come from the centralized environment tuning', () => {
  const template = synthesize();
  template.hasResourceProperties('AWS::Lambda::Function', {
    Environment: {
      Variables: Match.objectLike({
        CHAR_RENDER_DEFAULT_MAX_SIZE: '2048',
        CHAR_RENDER_DEFAULT_ZOOM: '1',
        CHAR_RENDER_DEFAULT_COMPLETE_LOOP: 'true',
        CHAR_RENDER_DEFAULT_MAX_FRAMES: '120',
        CHAR_RENDER_DEFAULT_WEBP_QUALITY: '85',
        CHAR_RENDER_DEFAULT_WEBP_METHOD: '4',
        CHAR_RENDER_FRAMES_PER_LAMBDA: '1',
        CHAR_RENDER_SOURCE_BUNDLE_FRAME_COUNT: '4',
        CHAR_RENDER_ALLOW_OFFICIAL_ASSET_FALLBACK: 'true',
      }),
    },
  });
});

test('dev Lambda sizing stays within the new-account limits', () => {
  const template = synthesize();
  const functions = template.findResources('AWS::Lambda::Function');
  for (const resource of Object.values(functions)) {
    expect(resource.Properties.MemorySize).toBeLessThanOrEqual(3008);
    expect(resource.Properties).not.toHaveProperty('ReservedConcurrentExecutions');
  }
});

test('source and result buckets remain private and use distinct lifecycle policies', () => {
  const template = synthesize();
  template.allResourcesProperties('AWS::S3::Bucket', {
    PublicAccessBlockConfiguration: {
      BlockPublicAcls: true,
      BlockPublicPolicy: true,
      IgnorePublicAcls: true,
      RestrictPublicBuckets: true,
    },
  });
  template.hasResourceProperties('AWS::S3::Bucket', {
    VersioningConfiguration: { Status: 'Enabled' },
  });
  template.hasResourceProperties('AWS::S3::Bucket', {
    LifecycleConfiguration: {
      Rules: Match.arrayWith([
        Match.objectLike({ Prefix: 'jobs/', ExpirationInDays: 2 }),
        Match.objectLike({ Prefix: 'renders/', ExpirationInDays: 30 }),
      ]),
    },
  });
});

test('budget shutdown is automatic at the configured threshold', () => {
  const template = synthesize();
  template.hasResourceProperties('AWS::Budgets::Budget', {
    Budget: {
      BudgetLimit: { Amount: 25, Unit: 'USD' },
      BudgetType: 'COST',
      TimeUnit: 'MONTHLY',
    },
    NotificationsWithSubscribers: Match.arrayWith([
      Match.objectLike({
        Notification: {
          ComparisonOperator: 'GREATER_THAN',
          NotificationType: 'ACTUAL',
          Threshold: 100,
          ThresholdType: 'PERCENTAGE',
        },
      }),
    ]),
  });
});
