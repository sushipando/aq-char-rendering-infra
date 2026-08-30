import * as path from 'node:path';
import * as cdk from 'aws-cdk-lib';
import * as budgets from 'aws-cdk-lib/aws-budgets';
import * as cloudfront from 'aws-cdk-lib/aws-cloudfront';
import * as origins from 'aws-cdk-lib/aws-cloudfront-origins';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as ecrAssets from 'aws-cdk-lib/aws-ecr-assets';
import * as events from 'aws-cdk-lib/aws-events';
import * as eventTargets from 'aws-cdk-lib/aws-events-targets';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as logs from 'aws-cdk-lib/aws-logs';
import * as s3 from 'aws-cdk-lib/aws-s3';
import * as sns from 'aws-cdk-lib/aws-sns';
import * as snsSubscriptions from 'aws-cdk-lib/aws-sns-subscriptions';
import * as sqs from 'aws-cdk-lib/aws-sqs';
import * as sfn from 'aws-cdk-lib/aws-stepfunctions';
import * as tasks from 'aws-cdk-lib/aws-stepfunctions-tasks';
import * as ssm from 'aws-cdk-lib/aws-ssm';
import { Construct } from 'constructs';
import { FunctionTuning, InfrastructureTuning } from './config/environment';

export interface AqwCharRenderingInfraStackProps extends cdk.StackProps {
  readonly stageName: string;
  readonly tuning: InfrastructureTuning;
}

interface RendererFunctions {
  readonly launcher: lambda.DockerImageFunction;
  readonly prepare: lambda.DockerImageFunction;
  readonly render: lambda.DockerImageFunction;
  readonly finalizer: lambda.DockerImageFunction;
  readonly complete: lambda.DockerImageFunction;
  readonly cleanup: lambda.DockerImageFunction;
  readonly shutdown: lambda.DockerImageFunction;
}

export class AqwCharRenderingInfraStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props: AqwCharRenderingInfraStackProps) {
    super(scope, id, props);

    const { stageName, tuning } = props;
    const removalPolicy = stageName === 'prod' ? cdk.RemovalPolicy.RETAIN : cdk.RemovalPolicy.DESTROY;
    const sourceBucket = this.createSourceBucket(stageName, removalPolicy);
    const workBucket = this.createWorkBucket(stageName, tuning, removalPolicy);
    const queues = this.createQueues(stageName, tuning);
    const jobTable = this.createJobTable(stageName, removalPolicy);
    const renderEnabled = new ssm.StringParameter(this, 'RenderEnabled', {
      parameterName: `/aqw-char/${stageName}/render-enabled`,
      description: 'Global admission and compute kill switch for AQW character rendering',
      stringValue: 'true',
      tier: ssm.ParameterTier.STANDARD,
    });
    const maximumActivePerUser = new ssm.StringParameter(this, 'MaximumActivePerUser', {
      parameterName: `/aqw-char/${stageName}/max-active-per-user`,
      description: 'Atomic admission limit for concurrent AQW character jobs per Discord user',
      stringValue: String(tuning.render.maxActivePerUser),
      tier: ssm.ParameterTier.STANDARD,
    });
    const distribution = this.createDistribution(stageName, workBucket);
    const functions = this.createFunctions({
      stageName,
      tuning,
      sourceBucket,
      workBucket,
      jobTable,
      resultQueue: queues.result,
      publicBaseUrl: `https://${distribution.distributionDomainName}`,
    });
    const stateMachine = this.createWorkflow(stageName, tuning, functions);

    functions.launcher.addEnvironment('CHAR_RENDER_STATE_MACHINE_ARN', stateMachine.stateMachineArn);
    stateMachine.grantStartExecution(functions.launcher);
    stateMachine.grantRead(functions.launcher);
    jobTable.grantReadWriteData(functions.launcher);

    const launcherMapping = new lambda.EventSourceMapping(this, 'LauncherJobQueueMapping', {
      target: functions.launcher,
      eventSourceArn: queues.job.queueArn,
      batchSize: 1,
      enabled: true,
      reportBatchItemFailures: false,
    });
    queues.job.grantConsumeMessages(functions.launcher);

    this.configurePermissions(
      sourceBucket,
      workBucket,
      jobTable,
      queues.result,
      functions,
      tuning,
    );
    this.configureCleanupRules(stateMachine, functions.cleanup);
    this.configureShutdown(
      stageName,
      tuning,
      functions,
      stateMachine,
      renderEnabled,
      launcherMapping,
    );
    this.configureAlarms(queues, stateMachine, functions);
    const botPolicy = this.createBotPolicy(
      stageName,
      jobTable,
      queues.job,
      queues.result,
      renderEnabled,
      maximumActivePerUser,
      sourceBucket,
      tuning.render.assetDatasetVersion,
    );

    new cdk.CfnOutput(this, 'DeploymentEnvironment', { value: stageName });
    new cdk.CfnOutput(this, 'AssetDatasetVersion', {
      value: tuning.render.assetDatasetVersion,
    });
    new cdk.CfnOutput(this, 'SourceAssetBucketName', { value: sourceBucket.bucketName });
    new cdk.CfnOutput(this, 'WorkResultBucketName', { value: workBucket.bucketName });
    new cdk.CfnOutput(this, 'JobQueueUrl', { value: queues.job.queueUrl });
    new cdk.CfnOutput(this, 'ResultQueueUrl', { value: queues.result.queueUrl });
    new cdk.CfnOutput(this, 'JobTableName', { value: jobTable.tableName });
    new cdk.CfnOutput(this, 'StateMachineArn', { value: stateMachine.stateMachineArn });
    new cdk.CfnOutput(this, 'RenderEnabledParameterName', { value: renderEnabled.parameterName });
    new cdk.CfnOutput(this, 'MaximumActivePerUserParameterName', {
      value: maximumActivePerUser.parameterName,
    });
    new cdk.CfnOutput(this, 'CloudFrontBaseUrl', {
      value: `https://${distribution.distributionDomainName}`,
    });
    new cdk.CfnOutput(this, 'HetznerBotManagedPolicyArn', { value: botPolicy.managedPolicyArn });
  }

  private createSourceBucket(stageName: string, removalPolicy: cdk.RemovalPolicy): s3.Bucket {
    return new s3.Bucket(this, 'SourceAssetBucket', {
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      bucketName: undefined,
      encryption: s3.BucketEncryption.S3_MANAGED,
      enforceSSL: true,
      versioned: true,
      removalPolicy,
      autoDeleteObjects: false,
      lifecycleRules: [{ id: `${stageName}-abort-source-multipart`, abortIncompleteMultipartUploadAfter: cdk.Duration.days(1) }],
    });
  }

  private createWorkBucket(
    stageName: string,
    tuning: InfrastructureTuning,
    removalPolicy: cdk.RemovalPolicy,
  ): s3.Bucket {
    return new s3.Bucket(this, 'WorkResultBucket', {
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      encryption: s3.BucketEncryption.S3_MANAGED,
      enforceSSL: true,
      versioned: false,
      removalPolicy,
      autoDeleteObjects: false,
      lifecycleRules: [
        {
          id: `${stageName}-job-work-expiration`,
          prefix: 'jobs/',
          expiration: cdk.Duration.days(tuning.retention.workDays),
          abortIncompleteMultipartUploadAfter: cdk.Duration.days(1),
        },
        {
          id: `${stageName}-render-expiration`,
          prefix: 'renders/',
          expiration: cdk.Duration.days(tuning.retention.resultDays),
          abortIncompleteMultipartUploadAfter: cdk.Duration.days(1),
        },
      ],
    });
  }

  private createQueues(stageName: string, tuning: InfrastructureTuning): {
    job: sqs.Queue;
    jobDlq: sqs.Queue;
    result: sqs.Queue;
    resultDlq: sqs.Queue;
  } {
    const jobDlq = new sqs.Queue(this, 'JobDeadLetterQueue', {
      queueName: `aqw-char-render-jobs-${stageName}-dlq`,
      encryption: sqs.QueueEncryption.SQS_MANAGED,
      retentionPeriod: cdk.Duration.days(14),
    });
    const resultDlq = new sqs.Queue(this, 'ResultDeadLetterQueue', {
      queueName: `aqw-char-render-results-${stageName}-dlq`,
      encryption: sqs.QueueEncryption.SQS_MANAGED,
      retentionPeriod: cdk.Duration.days(14),
    });
    const job = new sqs.Queue(this, 'JobQueue', {
      queueName: `aqw-char-render-jobs-${stageName}`,
      encryption: sqs.QueueEncryption.SQS_MANAGED,
      retentionPeriod: cdk.Duration.days(4),
      visibilityTimeout: cdk.Duration.seconds(tuning.jobQueueVisibilitySeconds),
      deadLetterQueue: { queue: jobDlq, maxReceiveCount: 5 },
    });
    const result = new sqs.Queue(this, 'ResultQueue', {
      queueName: `aqw-char-render-results-${stageName}`,
      encryption: sqs.QueueEncryption.SQS_MANAGED,
      retentionPeriod: cdk.Duration.days(4),
      visibilityTimeout: cdk.Duration.seconds(120),
      deadLetterQueue: { queue: resultDlq, maxReceiveCount: 5 },
    });
    return { job, jobDlq, result, resultDlq };
  }

  private createJobTable(stageName: string, removalPolicy: cdk.RemovalPolicy): dynamodb.Table {
    return new dynamodb.Table(this, 'JobTable', {
      tableName: `aqw-char-render-jobs-${stageName}`,
      partitionKey: { name: 'PK', type: dynamodb.AttributeType.STRING },
      sortKey: { name: 'SK', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      encryption: dynamodb.TableEncryption.AWS_MANAGED,
      pointInTimeRecoverySpecification: { pointInTimeRecoveryEnabled: true },
      timeToLiveAttribute: 'expires_at',
      removalPolicy,
    });
  }

  private createDistribution(stageName: string, workBucket: s3.Bucket): cloudfront.Distribution {
    const renderOnly = new cloudfront.Function(this, 'RenderPrefixGuard', {
      functionName: `aqw-char-render-prefix-${stageName}`,
      code: cloudfront.FunctionCode.fromInline(`function handler(event) {
  var request = event.request;
  if (request.uri.indexOf('/renders/') !== 0) {
    return { statusCode: 404, statusDescription: 'Not Found' };
  }
  return request;
}`),
      runtime: cloudfront.FunctionRuntime.JS_2_0,
    });
    return new cloudfront.Distribution(this, 'ResultDistribution', {
      comment: `AQW character render results (${stageName})`,
      defaultBehavior: {
        origin: origins.S3BucketOrigin.withOriginAccessControl(workBucket),
        allowedMethods: cloudfront.AllowedMethods.ALLOW_GET_HEAD,
        cachedMethods: cloudfront.CachedMethods.CACHE_GET_HEAD,
        cachePolicy: cloudfront.CachePolicy.CACHING_OPTIMIZED,
        compress: false,
        viewerProtocolPolicy: cloudfront.ViewerProtocolPolicy.REDIRECT_TO_HTTPS,
        functionAssociations: [
          { eventType: cloudfront.FunctionEventType.VIEWER_REQUEST, function: renderOnly },
        ],
      },
      enabled: true,
      httpVersion: cloudfront.HttpVersion.HTTP2_AND_3,
      priceClass: cloudfront.PriceClass.PRICE_CLASS_100,
    });
  }

  private createFunctions(input: {
    stageName: string;
    tuning: InfrastructureTuning;
    sourceBucket: s3.Bucket;
    workBucket: s3.Bucket;
    jobTable: dynamodb.Table;
    resultQueue: sqs.Queue;
    publicBaseUrl: string;
  }): RendererFunctions {
    const { stageName, tuning, sourceBucket, workBucket, jobTable, resultQueue, publicBaseUrl } = input;
    const context = path.join(__dirname, '..', 'services', 'renderer');
    const commonEnvironment = {
      CHAR_RENDER_SCHEMA_VERSION: String(tuning.render.schemaVersion),
      CHAR_RENDERER_VERSION: tuning.render.rendererVersion,
      CHAR_RENDER_ASSET_DATASET_VERSION: tuning.render.assetDatasetVersion,
      CHAR_RENDER_ASSET_MANIFEST_KEY: `datasets/${tuning.render.assetDatasetVersion}/manifest.json`,
      CHAR_RENDER_CHARACTER_RENDERER_KEY: `character-renderer/${tuning.render.assetDatasetVersion}/characterB.swf`,
      CHAR_RENDER_SOURCE_BUCKET: sourceBucket.bucketName,
      CHAR_RENDER_WORK_BUCKET: workBucket.bucketName,
      CHAR_RENDER_JOB_TABLE: jobTable.tableName,
      CHAR_RENDER_RESULT_QUEUE_URL: resultQueue.queueUrl,
      CHAR_RENDER_PUBLIC_BASE_URL: publicBaseUrl,
      CHAR_RENDER_BATCH_SIZE: String(tuning.render.frameBatchSize),
      CHAR_RENDER_MAX_ACTIVE_PER_USER: String(tuning.render.maxActivePerUser),
      CHAR_RENDER_DEFAULT_MAX_SIZE: String(tuning.render.maxSize),
      CHAR_RENDER_DEFAULT_ZOOM: String(tuning.render.zoom),
      CHAR_RENDER_DEFAULT_PADDING: String(tuning.render.padding),
      CHAR_RENDER_DEFAULT_COMPLETE_LOOP: String(tuning.render.completeLoop),
      CHAR_RENDER_DEFAULT_MAX_FRAMES: String(tuning.render.maxFrames),
      CHAR_RENDER_DEFAULT_SUBFRAME_START: String(tuning.render.subframeStart),
      CHAR_RENDER_DEFAULT_WEBP_QUALITY: String(tuning.render.webpQuality),
      CHAR_RENDER_DEFAULT_WEBP_METHOD: String(tuning.render.webpMethod),
      CHAR_RENDER_ALLOW_OFFICIAL_ASSET_FALLBACK: String(
        tuning.render.allowOfficialAssetFallback,
      ),
      CHAR_RENDER_OFFICIAL_ASSET_TIMEOUT_SECONDS: String(
        tuning.render.officialAssetTimeoutSeconds,
      ),
      CHAR_RENDER_CACHE_ENABLED: String(tuning.render.renderCacheEnabled),
      CHAR_RENDER_FFDEC_PATH: '/opt/ffdec/ffdec-cli.jar',
      CHAR_RENDER_RSVG_CONVERT: '/opt/resvg/resvg',
      CHAR_RENDER_CWEBP: '/opt/libwebp/bin/cwebp',
      CHAR_RENDER_WEBPMUX: '/opt/libwebp/bin/webpmux',
    };
    const make = (
      logicalId: string,
      handler: string,
      settings: FunctionTuning,
    ): lambda.DockerImageFunction => {
      const functionName = `aqw-char-${stageName}-${logicalId.toLowerCase()}`;
      const logGroup = new logs.LogGroup(this, `${logicalId}LogGroup`, {
        logGroupName: `/aws/lambda/${functionName}`,
        retention: logs.RetentionDays.ONE_MONTH,
        removalPolicy: cdk.RemovalPolicy.DESTROY,
      });
      return new lambda.DockerImageFunction(this, `${logicalId}Function`, {
        functionName,
        architecture: lambda.Architecture.X86_64,
        code: lambda.DockerImageCode.fromImageAsset(context, {
          cmd: [handler],
          platform: ecrAssets.Platform.LINUX_AMD64,
        }),
        description: `AQW character renderer ${logicalId} stage`,
        environment: commonEnvironment,
        ephemeralStorageSize: cdk.Size.mebibytes(settings.ephemeralStorageMiB),
        logGroup,
        memorySize: settings.memoryMiB,
        reservedConcurrentExecutions: settings.reservedConcurrency,
        timeout: cdk.Duration.seconds(settings.timeoutSeconds),
        tracing: lambda.Tracing.ACTIVE,
      });
    };
    return {
      launcher: make('Launcher', 'aqw_char_renderer.handlers.launcher.handler', tuning.functions.launcher),
      prepare: make('Prepare', 'aqw_char_renderer.handlers.prepare.handler', tuning.functions.prepare),
      render: make('Render', 'aqw_char_renderer.handlers.render.handler', tuning.functions.render),
      finalizer: make('Finalizer', 'aqw_char_renderer.handlers.finalize.handler', tuning.functions.finalizer),
      complete: make('Complete', 'aqw_char_renderer.handlers.complete.handler', tuning.functions.complete),
      cleanup: make('Cleanup', 'aqw_char_renderer.handlers.cleanup.handler', tuning.functions.cleanup),
      shutdown: make('Shutdown', 'aqw_char_renderer.handlers.shutdown.handler', tuning.functions.shutdown),
    };
  }

  private createWorkflow(
    stageName: string,
    tuning: InfrastructureTuning,
    functions: RendererFunctions,
  ): sfn.StateMachine {
    const lambdaRetry = {
      errors: [
        'Lambda.ServiceException',
        'Lambda.AWSLambdaException',
        'Lambda.SdkClientException',
        'Lambda.TooManyRequestsException',
      ],
      interval: cdk.Duration.seconds(2),
      backoffRate: 2,
      maxAttempts: 5,
    };
    // Prepare is split across three phases so the per-source FFDec export
    // (the dominant cost, especially at maxFrames up to 2000) runs in
    // parallel, one Lambda per source SWF.
    const prepareResolve = new tasks.LambdaInvoke(this, 'PrepareResolve', {
      lambdaFunction: functions.prepare,
      payload: sfn.TaskInput.fromObject({ request: sfn.JsonPath.objectAt('$.request') }),
      payloadResponseOnly: true,
      resultPath: '$.prepare',
    }).addRetry(lambdaRetry);

    const exportMap = new sfn.Map(this, 'ExportSourceFrames', {
      itemsPath: '$.prepare.sources',
      maxConcurrency: tuning.prepareExportConcurrency,
      resultPath: '$.export_results',
      itemSelector: {
        job_id: sfn.JsonPath.stringAt('$.prepare.job_id'),
        input_key: sfn.JsonPath.stringAt('$.prepare.input_key'),
        source: sfn.JsonPath.objectAt('$$.Map.Item.Value'),
      },
    });
    exportMap.itemProcessor(
      new tasks.LambdaInvoke(this, 'ExportSource', {
        lambdaFunction: functions.prepare,
        payload: sfn.TaskInput.fromObject({
          phase: 'export',
          job_id: sfn.JsonPath.stringAt('$.job_id'),
          input_key: sfn.JsonPath.stringAt('$.input_key'),
          source: sfn.JsonPath.objectAt('$.source'),
        }),
        payloadResponseOnly: true,
      }).addRetry(lambdaRetry),
    );

    const prepareFinish = new tasks.LambdaInvoke(this, 'PrepareFinish', {
      lambdaFunction: functions.prepare,
      payload: sfn.TaskInput.fromObject({
        phase: 'finish',
        request: sfn.JsonPath.objectAt('$.request'),
        input_key: sfn.JsonPath.stringAt('$.prepare.input_key'),
        export_results: sfn.JsonPath.listAt('$.export_results'),
      }),
      payloadResponseOnly: true,
      resultPath: '$.prepare',
    }).addRetry(lambdaRetry);

    // The shared canvas is computed in prepare_finish from each unique
    // state's alpha-probed bounds, so there is no probe Map or Fit Lambda.
    const renderMap = new sfn.Map(this, 'RenderFrameBatches', {
      itemsPath: '$.prepare.batches',
      maxConcurrency: tuning.render.mapConcurrency,
      resultPath: '$.render_results',
      itemSelector: {
        job_id: sfn.JsonPath.stringAt('$.prepare.job_id'),
        manifest_key: sfn.JsonPath.stringAt('$.prepare.manifest_key'),
        batch: sfn.JsonPath.objectAt('$$.Map.Item.Value'),
      },
    });
    renderMap.itemProcessor(
      new tasks.LambdaInvoke(this, 'RenderFrameBatch', {
        lambdaFunction: functions.render,
        payload: sfn.TaskInput.fromObject({
          job_id: sfn.JsonPath.stringAt('$.job_id'),
          manifest_key: sfn.JsonPath.stringAt('$.manifest_key'),
          batch: sfn.JsonPath.objectAt('$.batch'),
        }),
        payloadResponseOnly: true,
      }).addRetry(lambdaRetry),
    );

    // Finalize muxes the animation and completes the job inline, so a
    // rendered job costs no trailing completion state transition.
    const finalize = new tasks.LambdaInvoke(this, 'FinalizeAnimation', {
      lambdaFunction: functions.finalizer,
      payload: sfn.TaskInput.fromObject({
        job_id: sfn.JsonPath.stringAt('$.prepare.job_id'),
        manifest_key: sfn.JsonPath.stringAt('$.prepare.manifest_key'),
        render_results: sfn.JsonPath.listAt('$.render_results'),
        request: sfn.JsonPath.objectAt('$.request'),
      }),
      payloadResponseOnly: true,
    }).addRetry(lambdaRetry);

    const completeCached = new tasks.LambdaInvoke(this, 'CompleteCachedJob', {
      lambdaFunction: functions.complete,
      payload: sfn.TaskInput.fromObject({
        request: sfn.JsonPath.objectAt('$.request'),
        result: sfn.JsonPath.objectAt('$.prepare.result'),
        render_hash: sfn.JsonPath.stringAt('$.prepare.render_hash'),
      }),
      payloadResponseOnly: true,
    }).addRetry(lambdaRetry);

    const renderMiss = exportMap.next(prepareFinish).next(renderMap).next(finalize);
    const cacheChoice = new sfn.Choice(this, 'CachedResultExists')
      .when(sfn.Condition.booleanEquals('$.prepare.cache_hit', true), completeCached)
      .otherwise(renderMiss);
    const branch = prepareResolve.next(cacheChoice);
    const protectedWorkflow = new sfn.Parallel(this, 'ProtectedRenderWorkflow');
    protectedWorkflow.branch(branch);

    const failureHandler = new tasks.LambdaInvoke(this, 'CompleteFailedJob', {
      lambdaFunction: functions.complete,
      payload: sfn.TaskInput.fromObject({
        request: sfn.JsonPath.objectAt('$.request'),
        failure: sfn.JsonPath.objectAt('$.failure'),
      }),
      payloadResponseOnly: true,
    });
    const failed = new sfn.Fail(this, 'RenderFailed', {
      error: 'CharacterRenderFailed',
      cause: 'The terminal failure handler released the user slot and emitted a result.',
    });
    protectedWorkflow.addCatch(failureHandler.next(failed), { resultPath: '$.failure' });

    return new sfn.StateMachine(this, 'RenderStateMachine', {
      stateMachineName: `aqw-char-render-${stageName}`,
      stateMachineType: sfn.StateMachineType.STANDARD,
      definitionBody: sfn.DefinitionBody.fromChainable(protectedWorkflow),
      timeout: cdk.Duration.minutes(tuning.workflowTimeoutMinutes),
      tracingEnabled: true,
      logs: {
        destination: new logs.LogGroup(this, 'StateMachineLogGroup', {
          logGroupName: `/aws/vendedlogs/states/aqw-char-render-${stageName}`,
          retention: logs.RetentionDays.ONE_MONTH,
          removalPolicy: cdk.RemovalPolicy.DESTROY,
        }),
        includeExecutionData: false,
        level: sfn.LogLevel.ERROR,
      },
    });
  }

  private configurePermissions(
    sourceBucket: s3.Bucket,
    workBucket: s3.Bucket,
    jobTable: dynamodb.Table,
    resultQueue: sqs.Queue,
    functions: RendererFunctions,
    tuning: InfrastructureTuning,
  ): void {
    sourceBucket.grantRead(functions.prepare);
    functions.prepare.addToRolePolicy(
      new iam.PolicyStatement({
        actions: ['s3:PutObject'],
        resources: [
          sourceBucket.arnForObjects(
            `dynamic-assets/${tuning.render.assetDatasetVersion}/*`,
          ),
          // Content-addressed vector-state warm cache written by export workers.
          sourceBucket.arnForObjects('vector-states/*'),
        ],
      }),
    );
    workBucket.grantReadWrite(functions.prepare);
    workBucket.grantReadWrite(functions.render);
    workBucket.grantReadWrite(functions.finalizer);
    for (const fn of [
      functions.prepare,
      functions.finalizer,
      functions.complete,
      functions.cleanup,
    ]) {
      jobTable.grantReadWriteData(fn);
    }
    resultQueue.grantSendMessages(functions.finalizer);
    resultQueue.grantSendMessages(functions.complete);
    resultQueue.grantSendMessages(functions.cleanup);
    const concurrency = {
      [functions.launcher.functionName]: tuning.functions.launcher.reservedConcurrency,
      [functions.prepare.functionName]: tuning.functions.prepare.reservedConcurrency,
      [functions.render.functionName]: tuning.functions.render.reservedConcurrency,
      [functions.finalizer.functionName]: tuning.functions.finalizer.reservedConcurrency,
    };
    functions.shutdown.addEnvironment('CHAR_RENDER_WORKER_CONCURRENCY', JSON.stringify(concurrency));
  }

  private configureCleanupRules(
    stateMachine: sfn.StateMachine,
    cleanup: lambda.DockerImageFunction,
  ): void {
    stateMachine.grantRead(cleanup);
    new events.Rule(this, 'TerminalWorkflowRule', {
      eventPattern: {
        source: ['aws.states'],
        detailType: ['Step Functions Execution Status Change'],
        detail: {
          status: ['FAILED', 'TIMED_OUT', 'ABORTED'],
          stateMachineArn: [stateMachine.stateMachineArn],
        },
      },
    }).addTarget(new eventTargets.LambdaFunction(cleanup));
    new events.Rule(this, 'JobReconciliationSchedule', {
      description: 'Repairs stale character jobs and leaked user slots',
      schedule: events.Schedule.rate(cdk.Duration.minutes(15)),
    }).addTarget(new eventTargets.LambdaFunction(cleanup));
  }

  private configureShutdown(
    stageName: string,
    tuning: InfrastructureTuning,
    functions: RendererFunctions,
    stateMachine: sfn.StateMachine,
    renderEnabled: ssm.StringParameter,
    launcherMapping: lambda.EventSourceMapping,
  ): void {
    functions.shutdown.addEnvironment('CHAR_RENDER_ENABLED_PARAMETER', renderEnabled.parameterName);
    functions.shutdown.addEnvironment('CHAR_RENDER_STATE_MACHINE_ARN', stateMachine.stateMachineArn);
    functions.shutdown.addEnvironment(
      'CHAR_RENDER_STOP_FUNCTIONS',
      JSON.stringify([
        functions.launcher.functionName,
        functions.prepare.functionName,
        functions.render.functionName,
        functions.finalizer.functionName,
      ]),
    );
    functions.shutdown.addEnvironment(
      'CHAR_RENDER_LAUNCHER_EVENT_SOURCE_UUIDS',
      JSON.stringify([launcherMapping.eventSourceMappingId]),
    );
    renderEnabled.grantWrite(functions.shutdown);
    functions.shutdown.addToRolePolicy(
      new iam.PolicyStatement({
        actions: ['lambda:PutFunctionConcurrency', 'lambda:UpdateEventSourceMapping'],
        resources: ['*'],
      }),
    );
    functions.shutdown.addToRolePolicy(
      new iam.PolicyStatement({
        actions: ['states:ListExecutions'],
        resources: [stateMachine.stateMachineArn],
      }),
    );
    functions.shutdown.addToRolePolicy(
      new iam.PolicyStatement({
        actions: ['states:StopExecution'],
        resources: [
          cdk.Stack.of(this).formatArn({
            service: 'states',
            resource: 'execution',
            resourceName: `${stateMachine.stateMachineName}:*`,
          }),
        ],
      }),
    );

    const shutdownTopic = new sns.Topic(this, 'BudgetShutdownTopic', {
      topicName: `aqw-char-budget-shutdown-${stageName}`,
      displayName: 'AQW character rendering automatic budget shutdown',
    });
    shutdownTopic.addSubscription(new snsSubscriptions.LambdaSubscription(functions.shutdown));
    shutdownTopic.addToResourcePolicy(
      new iam.PolicyStatement({
        actions: ['sns:Publish'],
        principals: [new iam.ServicePrincipal('budgets.amazonaws.com')],
        resources: [shutdownTopic.topicArn],
        conditions: { StringEquals: { 'aws:SourceAccount': this.account } },
      }),
    );
    const budget = new budgets.CfnBudget(this, 'MonthlyCostBudget', {
      budget: {
        budgetName: `aqw-char-rendering-${stageName}`,
        budgetLimit: { amount: tuning.budget.monthlyUsd, unit: 'USD' },
        budgetType: 'COST',
        timeUnit: 'MONTHLY',
      },
      notificationsWithSubscribers: [
        {
          notification: {
            comparisonOperator: 'GREATER_THAN',
            notificationType: 'ACTUAL',
            threshold: tuning.budget.shutdownPercent,
            thresholdType: 'PERCENTAGE',
          },
          subscribers: [{ address: shutdownTopic.topicArn, subscriptionType: 'SNS' }],
        },
      ],
    });
    budget.node.addDependency(shutdownTopic);
    new cdk.CfnOutput(this, 'BudgetShutdownTopicArn', { value: shutdownTopic.topicArn });
  }

  private configureAlarms(
    queues: { job: sqs.Queue; jobDlq: sqs.Queue; result: sqs.Queue; resultDlq: sqs.Queue },
    stateMachine: sfn.StateMachine,
    functions: RendererFunctions,
  ): void {
    queues.jobDlq.metricApproximateNumberOfMessagesVisible().createAlarm(this, 'JobDlqAlarm', {
      threshold: 1,
      evaluationPeriods: 1,
      alarmDescription: 'A character render request exhausted launcher retries.',
    });
    queues.resultDlq.metricApproximateNumberOfMessagesVisible().createAlarm(this, 'ResultDlqAlarm', {
      threshold: 1,
      evaluationPeriods: 1,
      alarmDescription: 'A character render result exhausted delivery retries.',
    });
    queues.job.metricApproximateAgeOfOldestMessage().createAlarm(this, 'JobQueueAgeAlarm', {
      threshold: 600,
      evaluationPeriods: 2,
      alarmDescription: 'Character render requests have waited at least ten minutes.',
    });
    stateMachine.metricFailed().createAlarm(this, 'WorkflowFailureAlarm', {
      threshold: 1,
      evaluationPeriods: 1,
      alarmDescription: 'At least one character render workflow failed.',
    });
    for (const [name, fn] of Object.entries(functions)) {
      if (name === 'shutdown') continue;
      fn.metricErrors().createAlarm(this, `${name}FunctionErrorAlarm`, {
        threshold: 1,
        evaluationPeriods: 1,
        alarmDescription: `The ${name} character renderer Lambda reported an error.`,
      });
    }
  }

  private createBotPolicy(
    stageName: string,
    jobTable: dynamodb.Table,
    jobQueue: sqs.Queue,
    resultQueue: sqs.Queue,
    renderEnabled: ssm.StringParameter,
    maximumActivePerUser: ssm.StringParameter,
    sourceBucket: s3.Bucket,
    assetDatasetVersion: string,
  ): iam.ManagedPolicy {
    const manifestArn = sourceBucket.arnForObjects(
      `datasets/${assetDatasetVersion}/manifest.json`,
    );
    const dynamicAssetArn = sourceBucket.arnForObjects(
      `dynamic-assets/${assetDatasetVersion}/*`,
    );
    return new iam.ManagedPolicy(this, 'HetznerBotPolicy', {
      managedPolicyName: `aqw-char-hetzner-bot-${stageName}`,
      description: 'Least-privilege admission and result-delivery access for the Hetzner Discord bot',
      statements: [
        new iam.PolicyStatement({
          actions: [
            'dynamodb:GetItem',
            'dynamodb:UpdateItem',
            'dynamodb:PutItem',
            'dynamodb:TransactWriteItems',
          ],
          resources: [jobTable.tableArn],
        }),
        new iam.PolicyStatement({ actions: ['sqs:SendMessage'], resources: [jobQueue.queueArn] }),
        new iam.PolicyStatement({
          actions: [
            'sqs:ReceiveMessage',
            'sqs:DeleteMessage',
            'sqs:ChangeMessageVisibility',
            'sqs:GetQueueAttributes',
          ],
          resources: [resultQueue.queueArn],
        }),
        new iam.PolicyStatement({
          actions: ['ssm:GetParameter'],
          resources: [renderEnabled.parameterArn, maximumActivePerUser.parameterArn],
        }),
        new iam.PolicyStatement({
          actions: ['s3:GetObject'],
          resources: [manifestArn, dynamicAssetArn],
        }),
        new iam.PolicyStatement({
          actions: ['s3:PutObject'],
          resources: [dynamicAssetArn],
        }),
      ],
    });
  }
}
