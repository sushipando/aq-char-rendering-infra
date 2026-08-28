import * as cdk from 'aws-cdk-lib/core';
import { Construct } from 'constructs';

export interface AqwCharRenderingInfraStackProps extends cdk.StackProps {
  readonly stageName: string;
}

export class AqwCharRenderingInfraStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props: AqwCharRenderingInfraStackProps) {
    super(scope, id, props);

    // Resources are intentionally added in reviewed implementation phases.
    // Keeping the initial stack empty lets us validate account targeting and
    // synthesis without creating chargeable AWS resources.
    new cdk.CfnOutput(this, 'DeploymentEnvironment', {
      description: 'Environment represented by this stack',
      value: props.stageName,
    });

    cdk.Validations.of(this).acknowledge({
      id: 'CloudFormation-Validate::F0001',
      reason: 'The initial scaffold intentionally has no deployable resources yet.',
    });
  }
}
