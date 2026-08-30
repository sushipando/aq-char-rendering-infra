export interface FunctionTuning {
  readonly memoryMiB: number;
  readonly ephemeralStorageMiB: number;
  readonly timeoutSeconds: number;
  readonly reservedConcurrency?: number;
}

export interface RenderTuning {
  readonly schemaVersion: number;
  readonly rendererVersion: string;
  readonly assetDatasetVersion: string;
  readonly maxSize: number;
  readonly zoom: number;
  readonly padding: number;
  readonly completeLoop: boolean;
  readonly maxFrames: number;
  readonly subframeStart: number;
  readonly frameBatchSize: number;
  readonly mapConcurrency: number;
  readonly webpQuality: number;
  readonly webpMethod: number;
  readonly allowOfficialAssetFallback: boolean;
  readonly officialAssetTimeoutSeconds: number;
  readonly maxActivePerUser: number;
  readonly renderCacheEnabled: boolean;
}

export interface RetentionTuning {
  readonly workDays: number;
  readonly resultDays: number;
  readonly jobRecordDays: number;
  readonly logDays: number;
}

export interface BudgetTuning {
  readonly monthlyUsd: number;
  readonly shutdownPercent: number;
}

export interface InfrastructureTuning {
  readonly functions: Readonly<{
    launcher: FunctionTuning;
    prepare: FunctionTuning;
    render: FunctionTuning;
    finalizer: FunctionTuning;
    complete: FunctionTuning;
    cleanup: FunctionTuning;
    shutdown: FunctionTuning;
  }>;
  readonly render: RenderTuning;
  readonly retention: RetentionTuning;
  readonly budget: BudgetTuning;
  readonly workflowTimeoutMinutes: number;
  readonly jobQueueVisibilitySeconds: number;
  readonly prepareExportConcurrency: number;
}

export interface EnvironmentConfig {
  readonly account: string;
  readonly region: string;
  readonly stage: string;
  readonly tuning: InfrastructureTuning;
}

const mib = (
  memoryMiB: number,
  ephemeralStorageMiB: number,
  timeoutSeconds: number,
  reservedConcurrency?: number,
): FunctionTuning => ({ memoryMiB, ephemeralStorageMiB, timeoutSeconds, reservedConcurrency });

const DEV_TUNING: InfrastructureTuning = {
  functions: {
    // This new AWS account currently requires all 10 regional concurrency
    // units to remain unreserved, so dev uses the shared concurrency pool.
    launcher: mib(512, 512, 30),
    // This new AWS account currently enforces a 3008 MiB Lambda memory cap.
    prepare: mib(3008, 4096, 900),
    render: mib(3008, 4096, 900),
    finalizer: mib(3008, 4096, 300),
    complete: mib(512, 512, 60),
    cleanup: mib(512, 512, 60),
    shutdown: mib(512, 512, 300),
  },
  render: {
    schemaVersion: 1,
    // v8: random-pose ground cosmetics ping-pong their authored pose span so
    // the dragon bobs without the mid-timeline direction flip (v7 froze them).
    // v7: mirror-flip (random-pose ground cosmetic) layers are frozen at
    // their initial pose instead of looping the direction swap, so v6 cache
    // entries are invalidated.
    rendererVersion: 'v8',
    // Replace this before uploading/deploying a source corpus.
    assetDatasetVersion: 'dev-v1',
    maxSize: 2048,
    zoom: 2,
    padding: 0,
    completeLoop: true,
    maxFrames: 360,
    subframeStart: 1,
    frameBatchSize: 4,
    // Dev concurrency was raised 10 -> 1000 (case 178797464300402), so small
    // batches in a single wave are now the fast path: a 360-frame job is 90
    // batches of 4, all concurrent. Memory stays capped at 3008 MiB (~1.7
    // vCPU) so per-frame speed is unchanged, but wall time drops ~6x. (This
    // was previously 30/4 because 10 slots made >4 contend.)
    mapConcurrency: 300,
    webpQuality: 85,
    webpMethod: 4,
    allowOfficialAssetFallback: true,
    officialAssetTimeoutSeconds: 15,
    maxActivePerUser: 2,
    // Dev disables the content-addressed render cache so smoke tests and
    // benchmarks always exercise the real pipeline. Enable in prod for
    // cost/latency deduplication of identical requests.
    renderCacheEnabled: false,
  },
  retention: {
    workDays: 2,
    resultDays: 30,
    jobRecordDays: 35,
    logDays: 30,
  },
  budget: {
    monthlyUsd: 25,
    shutdownPercent: 100,
  },
  workflowTimeoutMinutes: 60,
  // Prepare is split so each source SWF exports in its own Lambda; a
  // character has ~5 sources, so a small concurrency is plenty, and Step
  // Functions throttles excess invocations safely.
  prepareExportConcurrency: 8,
  jobQueueVisibilitySeconds: 180,
};

const ENVIRONMENTS: Readonly<Record<string, EnvironmentConfig>> = {
  dev: {
    account: '538522204887',
    region: 'us-west-2',
    stage: 'dev',
    tuning: DEV_TUNING,
  },
};

export function getEnvironmentConfig(name: string | undefined): EnvironmentConfig {
  const requestedName = name ?? 'dev';
  const environment = ENVIRONMENTS[requestedName];

  if (environment === undefined) {
    throw new Error(
      `Unknown environment "${requestedName}". Available environments: ${Object.keys(ENVIRONMENTS).join(', ')}`,
    );
  }

  return environment;
}
