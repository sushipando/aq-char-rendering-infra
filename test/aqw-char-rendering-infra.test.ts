import * as cdk from 'aws-cdk-lib/core';
import { Template } from 'aws-cdk-lib/assertions';
import { getEnvironmentConfig } from '../lib/config/environment';
import { AqwCharRenderingInfraStack } from '../lib/aqw-char-rendering-infra-stack';

test('dev environment targets the dedicated account in us-west-2', () => {
  expect(getEnvironmentConfig('dev')).toEqual({
    account: '538522204887',
    region: 'us-west-2',
    stage: 'dev',
  });
});

test('unknown environments fail closed', () => {
  expect(() => getEnvironmentConfig('production')).toThrow('Unknown environment');
});

test('initial stack synthesizes without chargeable resources', () => {
  const app = new cdk.App();
  const stack = new AqwCharRenderingInfraStack(app, 'TestStack', {
    env: { account: '538522204887', region: 'us-west-2' },
    stageName: 'dev',
  });
  const template = Template.fromStack(stack);

  expect(template.toJSON().Resources ?? {}).toEqual({});
  template.hasOutput('DeploymentEnvironment', { Value: 'dev' });
});
