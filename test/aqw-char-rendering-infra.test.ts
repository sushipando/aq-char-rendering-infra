import * as cdk from 'aws-cdk-lib/core';
import { Match, Template } from 'aws-cdk-lib/assertions';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { getEnvironmentConfig } from '../lib/config/environment';
import { AqwCharRenderingInfraStack } from '../lib/aqw-char-rendering-infra-stack';

test('dev environment targets the dedicated account and exposes tuning in one config', () => {
  const environment = getEnvironmentConfig('dev');
  expect(environment.account).toBe('538522204887');
  expect(environment.region).toBe('us-west-2');
  expect(environment.stage).toBe('dev');
  expect(environment.tuning.prepareExportConcurrency).toBe(8);
  expect(environment.tuning.boundsInlineConcurrency).toBe(40);
  expect(environment.tuning.render).toMatchObject({
    rendererVersion: 'v20-rust-bounds',
    rasterSize: 2048,
    outputSize: 2048,
    zoom: 1,
    completeLoop: true,
    maxFrames: 120,
    framesPerRenderLambda: 1,
    sourceBundleFrameCount: 4,
    finalizerDownloadConcurrency: 32,
    mapConcurrency: 300,
    webpQuality: 85,
    allowOfficialAssetFallback: true,
    componentRasterInlineConcurrency: 40,
    componentRasterConcurrency: 200,
    componentRasterFrameCap: 120,
    componentComposeFramesPerLambda: 10,
    componentComposeConcurrency: 20,
  });
});

test('unknown environments fail closed', () => {
  expect(() => getEnvironmentConfig('production')).toThrow('Unknown environment');
});

test('inconsistent bounds retry deadlines fail synthesis', () => {
  const environment = getEnvironmentConfig('dev');
  for (const overrides of [{ boundsQueueVisibilitySeconds: 59 }, { boundsCallbackTimeoutSeconds: 1000 }]) {
    expect(() => new AqwCharRenderingInfraStack(new cdk.App(), 'Invalid', {
      stageName: 'dev',
      tuning: { ...environment.tuning, ...overrides },
    })).toThrow(/Bounds/);
  }
});

test('invalid bounds Inline Map concurrency fails synthesis', () => {
  const environment = getEnvironmentConfig('dev');
  for (const boundsInlineConcurrency of [0, 41]) {
    expect(() => new AqwCharRenderingInfraStack(new cdk.App(), `Invalid${boundsInlineConcurrency}`, {
      stageName: 'dev',
      tuning: { ...environment.tuning, boundsInlineConcurrency },
    })).toThrow(/Inline Map/);
  }
});

test('invalid component-raster Inline Map concurrency fails synthesis', () => {
  const environment = getEnvironmentConfig('dev');
  for (const componentRasterInlineConcurrency of [0, 41]) {
    expect(() => new AqwCharRenderingInfraStack(
      new cdk.App(),
      `InvalidComponent${componentRasterInlineConcurrency}`,
      {
        stageName: 'dev',
        tuning: {
          ...environment.tuning,
          render: { ...environment.tuning.render, componentRasterInlineConcurrency },
        },
      },
    )).toThrow(/Component-raster Inline Map/);
  }
});

test('bounds use the request-selected Inline or Distributed Map with an empty fast path', () => {
  const template = synthesize();
  const machine = Object.values(template.findResources('AWS::StepFunctions::StateMachine'))[0];
  const parts = machine.Properties.DefinitionString['Fn::Join'][1] as unknown[];
  const definition = JSON.parse(parts.map((part) => typeof part === 'string' ? part : 'REF').join(''));
  const states = definition.States.ProtectedRenderWorkflow.Branches[0].States;
  expect(states.ExportSourceFrames.Next).toBe('PlanBounds');
  expect(states.PlanBounds.Next).toBe('HasMissingBounds');
  expect(states.HasMissingBounds.Choices[0]).toMatchObject({ NumericEquals: 0, Next: 'PrepareFinish' });
  expect(states.HasMissingBounds.Default).toBe('SelectBoundsProbeMode');
  expect(states.SelectBoundsProbeMode.Choices[0]).toMatchObject({
    Variable: '$.request.bounds_mode',
    StringEquals: 'inline',
    Next: 'ProbeUniqueStatesInline',
  });
  expect(states.SelectBoundsProbeMode.Default).toBe('ProbeUniqueStatesDistributed');

  const inline = states.ProbeUniqueStatesInline;
  expect(inline.ItemsPath).toBe('$.bounds.inline_tasks');
  expect(inline.MaxConcurrency).toBe(40);
  expect(inline.ItemProcessor.ProcessorConfig).toEqual({ Mode: 'INLINE' });
  expect(inline.ResultPath).toBeNull();
  expect(inline.Next).toBe('PrepareFinish');
  expect(inline.ItemProcessor.States.ProbeUniqueStateInline.Parameters).toMatchObject({
    phase: 'probe',
    'task.$': '$',
  });

  const distributed = states.ProbeUniqueStatesDistributed;
  expect(distributed.ItemReader).toMatchObject({ ReaderConfig: { InputType: 'JSON' }, Parameters: { 'Key.$': '$.bounds.tasks_key' } });
  expect(distributed.ItemProcessor.ProcessorConfig).toEqual({ Mode: 'DISTRIBUTED', ExecutionType: 'STANDARD' });
  expect(distributed.ResultPath).toBeNull();
  expect(distributed.Next).toBe('PrepareFinish');
  const task = distributed.ItemProcessor.States.QueueBoundsProbe;
  expect(task.Resource).toContain('sqs:sendMessage.waitForTaskToken');
  expect(task.Parameters.MessageBody).toEqual({ 'task.$': '$', 'task_token.$': '$$.Task.Token' });
  expect(task.TimeoutSeconds).toBe(1200);
  expect(states.PrepareFinish.Parameters['bounds_plan_key.$']).toBe('$.bounds.plan_key');
});

test('bounds delivery is batch-one, bounded, retry-aware, and included in shutdown', () => {
  const template = synthesize();
  template.hasResourceProperties('AWS::Lambda::EventSourceMapping', {
    BatchSize: 1,
    FunctionResponseTypes: ['ReportBatchItemFailures'],
    ScalingConfig: { MaximumConcurrency: 100 },
  });
  template.hasResourceProperties('AWS::SQS::Queue', {
    QueueName: 'aqw-char-render-bounds-dev',
    VisibilityTimeout: 360,
    RedrivePolicy: Match.objectLike({ maxReceiveCount: 3 }),
  });
  const functions = Object.values(template.findResources('AWS::Lambda::Function'));
  for (const resource of functions) {
    expect(resource.Properties.Architectures).toEqual(['arm64']);
    expect(JSON.stringify(resource.Properties.ImageConfig)).not.toContain('aqw_char_renderer');
  }
  const shutdown = functions.find((fn) => fn.Properties.FunctionName === 'aqw-char-dev-shutdown')!;
  expect(JSON.stringify(shutdown.Properties.Environment.Variables.CHAR_RENDER_STOP_FUNCTIONS)).toContain('BoundsProbeFunction');
  expect(JSON.stringify(shutdown.Properties.Environment.Variables.CHAR_RENDER_STOP_FUNCTIONS)).toContain('ExportSourceFunction');
});

test('every Rust image build is ARM64-only and preserves Cargo build caches', () => {
  const dockerfiles = [
    'services/pipeline-rust/Dockerfile',
    'services/component-raster-rust/Dockerfile',
    'services/component-compose-rust/Dockerfile',
  ];
  for (const relativePath of dockerfiles) {
    const body = fs.readFileSync(path.join(__dirname, '..', relativePath), 'utf8');
    expect(body).toContain('ARG TARGETARCH');
    expect(body).toContain('"${TARGETARCH}" = "arm64"');
    expect(body).toContain('--mount=type=cache');
  }
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
  template.resourceCountIs('AWS::SQS::Queue', 6);
  template.resourceCountIs('AWS::DynamoDB::Table', 1);
  template.resourceCountIs('AWS::Lambda::Function', 10);
  template.resourceCountIs('AWS::StepFunctions::StateMachine', 1);
  template.resourceCountIs('AWS::CloudFront::Distribution', 1);
  template.resourceCountIs('AWS::SSM::Parameter', 2);
  template.resourceCountIs('AWS::Budgets::Budget', 1);
});

test('component rasterization uses the request-selected Inline or Distributed Map', () => {
  const template = synthesize();
  const stateMachines = template.findResources('AWS::StepFunctions::StateMachine');
  const stateMachine = Object.values(stateMachines)[0];
  const parts = stateMachine.Properties.DefinitionString['Fn::Join'][1] as unknown[];
  const definition = JSON.parse(parts.map((part) => typeof part === 'string' ? part : 'REF').join(''));
  const states = definition.States.ProtectedRenderWorkflow.Branches[0].States;

  expect(states.DiscardExportResults.Next).toBe('SelectComponentRasterMode');
  expect(states.SelectComponentRasterMode.Choices[0]).toMatchObject({
    Variable: '$.request.component_raster_mode',
    StringEquals: 'inline',
    Next: 'RasterComponentStatesInline',
  });
  expect(states.SelectComponentRasterMode.Default).toBe('RasterComponentStatesDistributed');

  const inline = states.RasterComponentStatesInline;
  expect(inline.ItemsPath).toBe('$.prepare.component_task_indices');
  expect(inline.MaxConcurrency).toBe(40);
  expect(inline.ResultPath).toBe('$.component_results');
  expect(inline.ItemProcessor.ProcessorConfig).toEqual({ Mode: 'INLINE' });
  expect(inline.Next).toBe('ComposeComponentFrameChunks');

  const distributed = states.RasterComponentStatesDistributed;
  expect(distributed.ItemsPath).toBe('$.prepare.component_task_indices');
  expect(distributed.MaxConcurrency).toBe(200);
  expect(distributed.ResultPath).toBe('$.component_results');
  expect(distributed.ItemProcessor.ProcessorConfig).toEqual({
    Mode: 'DISTRIBUTED',
    ExecutionType: 'EXPRESS',
  });
  expect(distributed.Next).toBe('ComposeComponentFrameChunks');

  for (const map of [inline, distributed]) {
    const task = Object.values(map.ItemProcessor.States)[0] as any;
    expect(task.Parameters).toMatchObject({
      'job_id.$': '$.job_id',
      'manifest_key.$': '$.manifest_key',
      'task_index.$': '$.task_index',
    });
  }

  // Function references are objects inside the Fn::Join array.
  const referencedFunctions = parts
    .filter((part): part is { 'Fn::GetAtt': string[] } =>
      typeof part === 'object' && part !== null && 'Fn::GetAtt' in part,
    )
    .map((part) => part['Fn::GetAtt'][0]);
  // Both modes invoke the same Rust resvg-library raster worker (arm64).
  expect(referencedFunctions.some((id) => id.startsWith('ComponentRasterRustFunction'))).toBe(true);

  template.hasResourceProperties('AWS::IAM::Policy', {
    PolicyDocument: {
      Statement: Match.arrayWith([
        Match.objectLike({
          Action: 'states:StartExecution',
          Effect: 'Allow',
        }),
        Match.objectLike({
          Action: Match.arrayWith(['states:DescribeExecution', 'states:StopExecution']),
          Effect: 'Allow',
        }),
      ]),
    },
  });
});

test('the Rust component-raster worker is the live backend with 3008 MiB and no reserve cap', () => {
  const template = synthesize();
  template.resourceCountIs('AWS::Lambda::Function', 10);
  // The active backend shares the account concurrency pool (no reserved cap).
  const functions = template.findResources('AWS::Lambda::Function');
  const rust = Object.values(functions).find((resource: any) =>
    resource.Properties.FunctionName === 'aqw-char-dev-componentraster-rust',
  );
  expect(rust).toBeDefined();
  expect(rust!.Properties).not.toHaveProperty('ReservedConcurrentExecutions');
});

test('component composition uses Rust with 20-way per-job Map concurrency', () => {
  const template = synthesize();
  const stateMachines = template.findResources('AWS::StepFunctions::StateMachine');
  const stateMachine = Object.values(stateMachines)[0];
  const serializedDefinition = JSON.stringify(stateMachine.Properties.DefinitionString);

  expect(serializedDefinition).toContain('ComponentComposeRustFunction');
  expect(serializedDefinition).not.toContain('ComponentComposeFunction');
  expect(serializedDefinition).toContain('ComposeComponentFrameChunks');
  expect(serializedDefinition).toContain('MaxConcurrency\\\":20');
});

test('Lambda request defaults come from the centralized environment tuning', () => {
  const template = synthesize();
  template.hasResourceProperties('AWS::Lambda::Function', {
    Environment: {
      Variables: Match.objectLike({
        CHAR_RENDER_DEFAULT_RASTER_SIZE: '2048',
        CHAR_RENDER_DEFAULT_OUTPUT_SIZE: '2048',
        CHAR_RENDER_DEFAULT_ZOOM: '1',
        CHAR_RENDER_DEFAULT_COMPLETE_LOOP: 'true',
        CHAR_RENDER_DEFAULT_MAX_FRAMES: '120',
        CHAR_RENDER_DEFAULT_WEBP_QUALITY: '85',
        CHAR_RENDER_DEFAULT_WEBP_METHOD: '4',
        CHAR_RENDER_FRAMES_PER_LAMBDA: '1',
        CHAR_RENDER_SOURCE_BUNDLE_FRAME_COUNT: '4',
        CHAR_RENDER_FINALIZER_DOWNLOAD_CONCURRENCY: '32',
        CHAR_RENDER_COMPONENT_COMPOSE_FRAMES_PER_LAMBDA: '10',
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
    // Both Rust backends are live and share the dev account's raised
    // concurrency pool with the rest of the pipeline (no per-function caps).
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
