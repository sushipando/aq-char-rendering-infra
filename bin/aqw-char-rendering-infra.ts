#!/usr/bin/env node
import * as cdk from 'aws-cdk-lib/core';
import { getEnvironmentConfig } from '../lib/config/environment';
import { AqwCharRenderingInfraStack } from '../lib/aqw-char-rendering-infra-stack';

const app = new cdk.App();
const environmentName = app.node.tryGetContext('environment') ?? process.env.AQW_CHAR_ENVIRONMENT;
const environment = getEnvironmentConfig(environmentName);

new AqwCharRenderingInfraStack(app, `AqwCharRendering-${environment.stage}`, {
  env: {
    account: environment.account,
    region: environment.region,
  },
  description: 'Distributed AQW character rendering infrastructure',
  stackName: `aqw-char-rendering-${environment.stage}`,
  stageName: environment.stage,
  tuning: environment.tuning,
  terminationProtection: environment.stage === 'prod',
  tags: {
    Environment: environment.stage,
    ManagedBy: 'aws-cdk',
    Project: 'aqw-char-rendering',
  },
});
