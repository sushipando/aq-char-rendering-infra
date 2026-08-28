export interface EnvironmentConfig {
  readonly account: string;
  readonly region: string;
  readonly stage: string;
}

const ENVIRONMENTS: Readonly<Record<string, EnvironmentConfig>> = {
  dev: {
    account: '538522204887',
    region: 'us-west-2',
    stage: 'dev',
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
