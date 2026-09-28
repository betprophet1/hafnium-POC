#!/usr/bin/env bash
# Two throwaway boxes: the venue on one, the load generator on the other, so the client's threads and sockets
# stop competing with the process under test. Everything is tagged venue-harness-test and removed by
# teardown2.sh.
set -eu
P="--profile prediction-exchange --region eu-central-1"
T=/home/someuser/.claude-prophetx/jobs/5ec94852/tmp/cloud
NAME=venue-harness-test
SERVER_TYPE=${SERVER_TYPE:-c6i.8xlarge}
CLIENT_TYPE=${CLIENT_TYPE:-c6i.8xlarge}
IP=$(curl -s --max-time 10 https://checkip.amazonaws.com)

echo "egress ip: $IP"
aws $P ec2 create-key-pair --key-name $NAME --query KeyMaterial --output text > $T/$NAME.pem
chmod 600 $T/$NAME.pem

SG=$(aws $P ec2 create-security-group --group-name $NAME \
  --description "throwaway: two-host load test" --query GroupId --output text)
echo "$SG" > $T/SG
# ssh from here only...
aws $P ec2 authorize-security-group-ingress --group-id "$SG" \
  --protocol tcp --port 22 --cidr "$IP/32" >/dev/null
# ...and anything between the two boxes, which is what makes the client remote rather than local.
aws $P ec2 authorize-security-group-ingress --group-id "$SG" \
  --protocol all --source-group "$SG" >/dev/null
echo "security group: $SG (ssh from $IP/32, all traffic within the group)"

AMI=$(aws $P ssm get-parameter \
  --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
  --query Parameter.Value --output text)

# One subnet for both, so they are in the same AZ and talk over private IPs at low latency.
SUBNET=$(aws $P ec2 describe-subnets --query 'Subnets[0].SubnetId' --output text)
echo "ami: $AMI  subnet: $SUBNET"

launch() {
  local role="$1" type="$2"
  aws $P ec2 run-instances --image-id "$AMI" --instance-type "$type" \
    --key-name $NAME --security-group-ids "$SG" --subnet-id "$SUBNET" \
    --instance-market-options '{"MarketType":"spot","SpotOptions":{"SpotInstanceType":"one-time"}}' \
    --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":80,"VolumeType":"gp3"}}]' \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$NAME-$role},{Key=Purpose,Value=throwaway load test}]" \
    --query 'Instances[0].InstanceId' --output text
}

SERVER=$(launch server "$SERVER_TYPE"); echo "$SERVER" > $T/SERVER_ID
CLIENT=$(launch client "$CLIENT_TYPE"); echo "$CLIENT" > $T/CLIENT_ID
echo "server: $SERVER ($SERVER_TYPE)   client: $CLIENT ($CLIENT_TYPE)"

aws $P ec2 wait instance-running --instance-ids "$SERVER" "$CLIENT"

addr() {
  aws $P ec2 describe-instances --instance-ids "$1" \
    --query "Reservations[0].Instances[0].$2" --output text
}
addr "$SERVER" PublicIpAddress  > $T/SERVER_HOST
addr "$SERVER" PrivateIpAddress > $T/SERVER_PRIVATE
addr "$CLIENT" PublicIpAddress  > $T/CLIENT_HOST

echo "server public $(cat $T/SERVER_HOST)  private $(cat $T/SERVER_PRIVATE)"
echo "client public $(cat $T/CLIENT_HOST)"
