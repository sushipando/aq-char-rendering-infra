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
  readonly framesPerRenderLambda: number;
  readonly sourceBundleFrameCount: number;
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
    // 120-frame jobs must produce fast (<300s) and each source SWF spawns its
    // own FFDec JVM, so prepare runs are capped at 300s.
    prepare: mib(3008, 4096, 300),
    render: mib(3008, 4096, 900),
    finalizer: mib(3008, 4096, 300),
    complete: mib(512, 512, 60),
    cleanup: mib(512, 512, 60),
    shutdown: mib(512, 512, 300),
  },
  render: {
    schemaVersion: 1,
    // v14: render workers encode complete frames and compute exactly the
    // configured number of frames. This removes the previous overlap-frame
    // rasterization that every non-first batch needed for delta cropping.
    // v13: an Idle label immediately followed by stop() begins on that settled
    // root frame, fixing Drudgen's quest-bubble placement (frame 8 vs frame 7).
    // v12: a stopped startup parent begins on its settled frame while nested
    // idle clips keep advancing; unlabeled weapon root timelines also advance
    // instead of being frozen on frame 1.
    // v11: idle AS3 timelines that settle on a later stop() frame start on and
    // hold the complete finished parent state. v10 incorrectly promoted nested
    // artwork and turned the finished state into a new loop.
    // v10: detect later authored stop() frames instead of wrapping the parent
    // back to frame 1.
    // v9: invisible animation states (opacity-0 blink/cape frames) store null
    // bounds instead of their loose header canvas, so the shared viewbox no
    // longer includes their phantom stage and the animation fills the frame.
    // v8: random-pose ground cosmetics ping-pong their authored pose span so
    // the dragon bobs without the mid-timeline direction flip (v7 froze them).
    // v7: mirror-flip (random-pose ground cosmetic) layers are frozen at
    // their initial pose instead of looping the direction swap, so v6 cache
    // entries are invalidated.
    rendererVersion: 'v14',
    // Replace this before uploading/deploying a source corpus.
    assetDatasetVersion: 'dev-v1',
    maxSize: 2048,
    // FFDec SVG export at zoom 2 makes multi-state cosmetics (e.g. capes with
    // 128 unique states) crawl at ~15s/frame -> a 32-minute export that blows
    // the 300s prepare budget. Zoom 1 is fully supported by the compositor
    // (export_zoom/minimum-stroke math) and keeps 120-frame jobs < 300s; the
    // only loss is sub-pixel stroke crispness, which the 2048 canvas
    // upscaling already smooths.
    zoom: 1,
    padding: 0,
    completeLoop: true,
    // 120-frame outputs keep every source FFDec export (and the render map)
    // comfortably inside the 300s prepare budget, even for rare cosmetic
    // assets with many unique states (e.g. DmnkAlterEgoGR: ~15s/frame at
    // zoom 2). 360 frames on such assets could never finish in 300s.
    maxFrames: 120,
    subframeStart: 1,
    // Compute one complete frame per render Lambda. Source SVG bundles remain
    // four frames apiece so prepare does not trade the render speedup for
    // hundreds of additional small, serial S3 uploads.
    framesPerRenderLambda: 1,
    sourceBundleFrameCount: 4,
    // Dev concurrency was raised 10 -> 1000 (case 178797464300402), so small
    // render Lambdas in a single wave are now the fast path: a 120-frame job
    // is 120 independent full-frame tasks. Memory stays capped at 3008 MiB
    // (~1.7 vCPU), while the account's raised concurrency lets all tasks run
    // without serializing four expensive rasterizations in each invocation.
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
  // Each source SWF exports in its own Lambda, with its own CPU allocation.
  // A character normally has about five sources, so keep enough concurrency
  // to export all of them in one wave.
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
