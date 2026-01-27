#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  ./ops/aws/pmmm_aws.sh deploy
  ./ops/aws/pmmm_aws.sh status <instance-id>
  ./ops/aws/pmmm_aws.sh health <instance-id>
  ./ops/aws/pmmm_aws.sh logs <instance-id>
  ./ops/aws/pmmm_aws.sh update <instance-id>
  ./ops/aws/pmmm_aws.sh stop <instance-id>
  ./ops/aws/pmmm_aws.sh start <instance-id>
  ./ops/aws/pmmm_aws.sh terminate <instance-id>

Env overrides:
  AWS_REGION=eu-west-1
  PMMM_INSTANCE_NAME=pmmm-bot-canary
  PMMM_INSTANCE_TYPE=t3.medium
  PMMM_VOLUME_GB=40
  PMMM_INSTANCE_PROFILE=hft-tracker-ec2-profile
  PMMM_SUBNET_ID=... (optional; defaults to default subnet)
  PMMM_SG_NAME=pmmm-bot-ssm
  PMMM_FORCE_NEW=1 (deploy always creates a new instance)
EOF
}

AWS_REGION="${AWS_REGION:-eu-west-1}"
PMMM_INSTANCE_NAME="${PMMM_INSTANCE_NAME:-pmmm-bot-canary}"
PMMM_INSTANCE_TYPE="${PMMM_INSTANCE_TYPE:-t3.medium}"
PMMM_VOLUME_GB="${PMMM_VOLUME_GB:-40}"
PMMM_INSTANCE_PROFILE="${PMMM_INSTANCE_PROFILE:-hft-tracker-ec2-profile}"
PMMM_SUBNET_ID="${PMMM_SUBNET_ID:-}"
PMMM_SG_NAME="${PMMM_SG_NAME:-pmmm-bot-ssm}"
PMMM_FORCE_NEW="${PMMM_FORCE_NEW:-0}"

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 1; }
}

require_cmd aws

aws_acct() {
  aws sts get-caller-identity --query 'Account' --output text
}

get_al2023_ami() {
  aws ssm get-parameter \
    --region "$AWS_REGION" \
    --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-6.1-x86_64 \
    --query 'Parameter.Value' \
    --output text
}

default_vpc_id() {
  aws ec2 describe-vpcs \
    --region "$AWS_REGION" \
    --filters Name=isDefault,Values=true \
    --query 'Vpcs[0].VpcId' \
    --output text
}

default_subnet_id() {
  aws ec2 describe-subnets \
    --region "$AWS_REGION" \
    --filters Name=default-for-az,Values=true \
    --query 'Subnets[0].SubnetId' \
    --output text
}

subnet_vpc_id() {
  local subnet_id="$1"
  aws ec2 describe-subnets \
    --region "$AWS_REGION" \
    --subnet-ids "$subnet_id" \
    --query 'Subnets[0].VpcId' \
    --output text
}

ensure_sg() {
  local vpc_id="$1"

  local sg_id
  sg_id=$(
    aws ec2 describe-security-groups \
      --region "$AWS_REGION" \
      --filters Name=vpc-id,Values="$vpc_id" Name=group-name,Values="$PMMM_SG_NAME" \
      --query 'SecurityGroups[0].GroupId' \
      --output text 2>/dev/null || true
  )

  if [[ -n "$sg_id" && "$sg_id" != "None" && "$sg_id" != "null" ]]; then
    echo "$sg_id"
    return 0
  fi

  sg_id=$(
    aws ec2 create-security-group \
      --region "$AWS_REGION" \
      --vpc-id "$vpc_id" \
      --group-name "$PMMM_SG_NAME" \
      --description "pmmm bot canary (SSM, no inbound)" \
      --query 'GroupId' \
      --output text
  )

  aws ec2 create-tags \
    --region "$AWS_REGION" \
    --resources "$sg_id" \
    --tags Key=Name,Value="$PMMM_SG_NAME" Key=Project,Value=pmmm >/dev/null

  # No inbound rules. Keep default all-egress.
  echo "$sg_id"
}

find_existing_instance() {
  aws ec2 describe-instances \
    --region "$AWS_REGION" \
    --filters Name=tag:Name,Values="$PMMM_INSTANCE_NAME" Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[?State.Name==`running`].InstanceId | [0]' \
    --output text
}

wait_for_ssm() {
  local instance_id="$1"
  for _ in $(seq 1 60); do
    local status
    status=$(
      aws ssm describe-instance-information \
        --region "$AWS_REGION" \
        --filters Key=InstanceIds,Values="$instance_id" \
        --query 'InstanceInformationList[0].PingStatus' \
        --output text 2>/dev/null || true
    )
    if [[ "$status" == "Online" ]]; then
      return 0
    fi
    sleep 5
  done
  echo "SSM did not come online for $instance_id" >&2
  return 1
}

send_cmd() {
  local instance_id="$1"
  local commands="$2"
  local cmd_id
  cmd_id=$(
    aws ssm send-command \
      --region "$AWS_REGION" \
      --instance-ids "$instance_id" \
      --document-name AWS-RunShellScript \
      --parameters "commands=$commands" \
      --query 'Command.CommandId' \
      --output text
  )
  aws ssm wait command-executed --region "$AWS_REGION" --command-id "$cmd_id" --instance-id "$instance_id" >/dev/null
  aws ssm get-command-invocation \
    --region "$AWS_REGION" \
    --command-id "$cmd_id" \
    --instance-id "$instance_id" \
    --query '{Status:Status,Stdout:StandardOutputContent,Stderr:StandardErrorContent}' \
    --output json
}

deploy() {
  echo "AWS account: $(aws_acct)"

  if [[ "$PMMM_FORCE_NEW" != "1" ]]; then
    local existing
    existing="$(find_existing_instance)"
    if [[ -n "$existing" && "$existing" != "None" && "$existing" != "null" ]]; then
      echo "Found existing running instance: $existing (Name=$PMMM_INSTANCE_NAME)"
      echo "Tip: PMMM_FORCE_NEW=1 ./ops/aws/pmmm_aws.sh deploy"
      return 0
    fi
  fi

  local subnet_id="$PMMM_SUBNET_ID"
  if [[ -z "$subnet_id" ]]; then
    subnet_id="$(default_subnet_id)"
  fi

  local vpc_id
  vpc_id="$(subnet_vpc_id "$subnet_id")"

  local sg_id
  sg_id="$(ensure_sg "$vpc_id")"

  local ami_id
  ami_id="$(get_al2023_ami)"

  local user_data
  user_data="$(mktemp)"
  cat >"$user_data" <<'EOS'
#!/bin/bash
set -euxo pipefail

BOT_USER="pmmm"
REPO_URL="https://github.com/stefangoessens/final-bot.git"
REPO_DIR="/opt/pmmm-bot"
WORK_DIR="/var/lib/pmmm-bot"
ENV_FILE="/etc/pmmm/pmmm.env"

dnf -y install git gcc gcc-c++ make pkgconfig openssl-devel ca-certificates curl-minimal

if ! id -u "$BOT_USER" >/dev/null 2>&1; then
  useradd -m -s /bin/bash "$BOT_USER"
fi

su - "$BOT_USER" -c 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'

cat >/etc/profile.d/rust.sh <<'EOP'
export PATH=/home/pmmm/.cargo/bin:$PATH
EOP

rm -rf "$REPO_DIR"
mkdir -p "$REPO_DIR"
chown -R "$BOT_USER":"$BOT_USER" "$REPO_DIR"
su - "$BOT_USER" -c "git clone --depth 1 --branch main $REPO_URL $REPO_DIR"

su - "$BOT_USER" -c "cd $REPO_DIR && /home/$BOT_USER/.cargo/bin/cargo build --release"
install -m 0755 -o root -g root "$REPO_DIR/target/release/polymarket-mm-bot" /usr/local/bin/polymarket-mm-bot

mkdir -p "$WORK_DIR/logs"
chown -R "$BOT_USER":"$BOT_USER" "$WORK_DIR"

mkdir -p /etc/pmmm
cat >"$ENV_FILE" <<'EOC'
PMMB_TRADING__DRY_RUN=true
PMMB_INFRA__LOG_LEVEL=info
PMMB_INFRA__AWS_REGION=eu-west-1
EOC
chmod 0600 "$ENV_FILE"
chown root:root "$ENV_FILE"

cat >/etc/systemd/system/pmmm-bot.service <<'EOF'
[Unit]
Description=Polymarket BTC 15m MM Bot (pmmm)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=pmmm
WorkingDirectory=/var/lib/pmmm-bot
EnvironmentFile=/etc/pmmm/pmmm.env
ExecStart=/usr/local/bin/polymarket-mm-bot
Restart=always
RestartSec=2
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now pmmm-bot.service
systemctl --no-pager status pmmm-bot.service || true
EOS

local instance_id
instance_id=$(
  aws ec2 run-instances \
    --region "$AWS_REGION" \
    --image-id "$ami_id" \
    --instance-type "$PMMM_INSTANCE_TYPE" \
    --iam-instance-profile "Name=$PMMM_INSTANCE_PROFILE" \
    --metadata-options HttpTokens=required,HttpEndpoint=enabled \
    --block-device-mappings "[{\"DeviceName\":\"/dev/xvda\",\"Ebs\":{\"VolumeSize\":$PMMM_VOLUME_GB,\"VolumeType\":\"gp3\",\"DeleteOnTermination\":true}}]" \
    --network-interfaces "[{\"DeviceIndex\":0,\"SubnetId\":\"$subnet_id\",\"Groups\":[\"$sg_id\"],\"AssociatePublicIpAddress\":true,\"DeleteOnTermination\":true}]" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$PMMM_INSTANCE_NAME},{Key=Project,Value=pmmm}]" \
    --user-data "file://$user_data" \
    --count 1 \
    --query 'Instances[0].InstanceId' \
    --output text
  )

  rm -f "$user_data"

  echo "InstanceId: $instance_id"
  aws ec2 wait instance-running --region "$AWS_REGION" --instance-ids "$instance_id"
  wait_for_ssm "$instance_id"
  echo "SSM: Online"

  echo "Initial status:"
  send_cmd "$instance_id" 'sudo systemctl is-active pmmm-bot.service || true; sudo journalctl -u pmmm-bot.service -n 30 --no-pager --no-hostname || true'
}

status() {
  local instance_id="$1"
  aws ec2 describe-instances \
    --region "$AWS_REGION" \
    --instance-ids "$instance_id" \
    --query 'Reservations[0].Instances[0].{State:State.Name,Name:Tags[?Key==`Name`]|[0].Value,PublicIp:PublicIpAddress,PrivateIp:PrivateIpAddress,AZ:Placement.AvailabilityZone}' \
    --output json
  send_cmd "$instance_id" 'sudo systemctl --no-pager --full status pmmm-bot.service || true'
}

health() {
  local instance_id="$1"
  send_cmd "$instance_id" 'curl -fsS localhost:8080/healthz || true; echo ---; curl -fsS localhost:9090/metrics | head -n 60 || true'
}

logs() {
  local instance_id="$1"
  send_cmd "$instance_id" 'sudo journalctl -u pmmm-bot.service -n 200 --no-pager --no-hostname || true'
}

update_instance() {
  local instance_id="$1"
  send_cmd "$instance_id" 'set -euxo pipefail; sudo systemctl stop pmmm-bot.service || true; sudo -u pmmm bash -lc "cd /opt/pmmm-bot && git fetch origin main && git reset --hard origin/main"; sudo -u pmmm bash -lc "cd /opt/pmmm-bot && /home/pmmm/.cargo/bin/cargo build --release"; sudo install -m 0755 -o root -g root /opt/pmmm-bot/target/release/polymarket-mm-bot /usr/local/bin/polymarket-mm-bot; sudo systemctl start pmmm-bot.service; sudo systemctl --no-pager --full status pmmm-bot.service || true'
}

stop_instance() {
  local instance_id="$1"
  send_cmd "$instance_id" 'sudo systemctl stop pmmm-bot.service || true; sudo systemctl is-active pmmm-bot.service || true'
}

start_instance() {
  local instance_id="$1"
  send_cmd "$instance_id" 'sudo systemctl start pmmm-bot.service || true; sudo systemctl is-active pmmm-bot.service || true'
}

terminate_instance() {
  local instance_id="$1"
  aws ec2 terminate-instances --region "$AWS_REGION" --instance-ids "$instance_id" >/dev/null
  aws ec2 wait instance-terminated --region "$AWS_REGION" --instance-ids "$instance_id"
  echo "terminated: $instance_id"
}

cmd="${1:-}"
shift || true

case "$cmd" in
  deploy) deploy ;;
  status) [[ $# -eq 1 ]] || { usage; exit 1; }; status "$1" ;;
  health) [[ $# -eq 1 ]] || { usage; exit 1; }; health "$1" ;;
  logs) [[ $# -eq 1 ]] || { usage; exit 1; }; logs "$1" ;;
  update) [[ $# -eq 1 ]] || { usage; exit 1; }; update_instance "$1" ;;
  stop) [[ $# -eq 1 ]] || { usage; exit 1; }; stop_instance "$1" ;;
  start) [[ $# -eq 1 ]] || { usage; exit 1; }; start_instance "$1" ;;
  terminate) [[ $# -eq 1 ]] || { usage; exit 1; }; terminate_instance "$1" ;;
  -h|--help|"") usage ;;
  *) echo "unknown command: $cmd" >&2; usage; exit 1 ;;
esac

