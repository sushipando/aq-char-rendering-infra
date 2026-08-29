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
    // v4: shared canvas now comes from vector bounds computed in Prepare
    // (the compose-stage alpha probe rasterization was removed).
    rendererVersion: 'v4',
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
    mapConcurrency: 90,
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
  jobQueueVisibilitySeconds: 180,
};

// Root/management account (619440099418) has 400 concurrent executions, so
// it is used as a throwaway high-parallelism benchmark environment. A
// 360-frame job at batch size 30 yields 12 batches, so mapConcurrency 12
// runs the entire Map in one wave. Smaller batches create more batches and
// thus more concurrent render Lambdas when desired. NOT for production use:
// this is the organization management account and should only host
// short-lived benchmark stacks that are torn down afterward.
const ROOT_TUNING: InfrastructureTuning = {
  ...DEV_TUNING,
  functions: {
    ...DEV_TUNING.functions,
    // More memory = more vCPU (Lambda scales CPU by memory). Render workers
    // get ~3 vCPU to speed up the per-frame rasterize. Prepare stays at 3008
    // MiB because its FFDec export is dominated by a single source, so extra
    // vCPU gave no measurable speedup (13.4s vs 13.1s) and just costs more.
    prepare: { memoryMiB: 3008, ephemeralStorageMiB: 4096, timeoutSeconds: 900 },
    render: { memoryMiB: 5308, ephemeralStorageMiB: 4096, timeoutSeconds: 900, reservedConcurrency: 100 },
  },
  render: {
    ...DEV_TUNING.render,
    // Small batches + high concurrency is the correct way to exploit the
    // 400-slot ceiling: a 360-frame job becomes 90 batches of 4, each ~16s,
    // all in one wave, so the Map wall approaches the single-batch time
    // instead of a fixed 120s+ chunk. (On the 10-slot dev account this just
    // contends; it only works because root has headroom.)
    frameBatchSize: 4,
    mapConcurrency: 90,
    renderCacheEnabled: false,
  },
};

const ENVIRONMENTS: Readonly<Record<string, EnvironmentConfig>> = {
  dev: {
    account: '538522204887',
    region: 'us-west-2',
    stage: 'dev',
    tuning: DEV_TUNING,
  },
  root: {
    account: '619440099418',
    region: 'us-west-2',
    stage: 'root',
    tuning: ROOT_TUNING,
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
