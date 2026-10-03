#!/usr/bin/env bash
# Set a GCP project up for one taste-ide workspace, or take it back out.
#
# This is the whole of what a project owner does for the IDE's cloud
# machines (ENVIRONMENTS → "A model on a cloud VM"), and it is the only
# place the user's own Google credentials are ever used: the IDE never
# holds them, not even to set itself up. Run it where gcloud is signed in
# as you — Cloud Shell is the easy place — and it is safe to run again,
# which is how a re-enrolled workspace or a role that gained a permission
# is repaired.
#
#   build-aux/gcp-setup.sh PROJECT WORKSPACE CA_PEM_FILE
#   build-aux/gcp-setup.sh --remove PROJECT WORKSPACE
#
# WORKSPACE is the eight hex digits that end the workspace's state
# directory (`…/taste-ide/workspaces/<name>-<WORKSPACE>/`), and the CA is
# the certificate the IDE enrolled for it, whose key never leaves that
# machine's TPM. The IDE also hands this same file out with the three
# values filled in above the first command (`taste_gcp::setup`), so the
# copy you are given and the copy reviewed here cannot differ.
#
# ## What it creates, and what it grants
#
# - The APIs the IDE calls.
# - A custom role, `tasteIde`, of exactly the calls the IDE makes: the
#   machines' networks, firewall rules, DNS policy, disks, and instances,
#   and the operations, quotas, and machine facts it reads. Nothing in it
#   touches billing, IAM, or a service account, so the identity can spend
#   money only by running machines, which the IDE's cap meters, and it
#   cannot widen its own reach. Shared by every workspace in the project.
# - A Workload Identity pool, `taste-ide`, shared the same way.
# - An X.509 provider for this workspace, `taste-<WORKSPACE>`, whose only
#   trust anchor is the workspace's CA and which admits one subject,
#   `taste-<WORKSPACE>`.
# - The role, granted to that one federated principal.
#
# No service account and no key: organizations created since May 2024
# forbid creating or uploading service-account keys by default, and a
# key file is exactly what a copied disk would hand over.
#
# ## --remove
#
# Deletes the workspace's provider and its grant; the shared pool and
# role stay for other workspaces. Then it lists whatever the workspace's
# machines and disks are still in the project, because removing the
# identity stops the IDE managing them, and anything left running keeps
# costing. GCP keeps a deleted provider for 30 days; setting the same
# workspace up again inside them restores it rather than failing.
set -euo pipefail
export CLOUDSDK_CORE_DISABLE_PROMPTS=1

POOL=taste-ide
ROLE=tasteIde

# The APIs the IDE calls. Kept equal to `taste_gcp::setup::SERVICES` by a
# test that reads this file.
SERVICES=(
  cloudbilling.googleapis.com
  cloudresourcemanager.googleapis.com
  compute.googleapis.com
  dns.googleapis.com
  iam.googleapis.com
  iamcredentials.googleapis.com
  sts.googleapis.com
)

# Exactly what the IDE calls. Kept equal to
# `taste_gcp::setup::PERMISSIONS` by a test that reads this file, and the
# IDE's preflight asks `testIamPermissions` for the same list.
PERMISSIONS=(
  compute.disks.create
  compute.disks.delete
  compute.disks.get
  compute.disks.list
  compute.disks.setLabels
  compute.disks.update
  compute.disks.use
  compute.disks.useReadOnly
  compute.firewalls.create
  compute.firewalls.delete
  compute.firewalls.get
  compute.firewalls.list
  compute.firewalls.update
  compute.globalOperations.get
  compute.instances.create
  compute.instances.delete
  compute.instances.get
  compute.instances.getGuestAttributes
  compute.instances.list
  compute.instances.setLabels
  compute.instances.setMetadata
  compute.instances.start
  compute.instances.stop
  compute.machineTypes.get
  compute.networks.create
  compute.networks.delete
  compute.networks.get
  compute.networks.list
  compute.networks.updatePolicy
  compute.networks.use
  compute.projects.get
  compute.regionOperations.get
  compute.regions.get
  compute.subnetworks.create
  compute.subnetworks.delete
  compute.subnetworks.get
  compute.subnetworks.list
  compute.subnetworks.use
  compute.subnetworks.useExternalIp
  compute.zoneOperations.get
  compute.zones.get
  dns.networks.bindPrivateDNSPolicy
  dns.policies.create
  dns.policies.delete
  dns.policies.get
  dns.policies.list
  dns.policies.update
  resourcemanager.projects.get
)

usage() {
  echo "usage: $0 PROJECT WORKSPACE CA_PEM_FILE" >&2
  echo "       $0 --remove PROJECT WORKSPACE" >&2
  exit 2
}

MODE=setup
if [[ -z "${TASTE_PROJECT:-}" ]]; then
  if [[ "${1:-}" == "--remove" ]]; then
    MODE=remove
    shift
    [[ $# -eq 2 ]] || usage
  else
    [[ $# -eq 3 ]] || usage
    TASTE_CA_PEM=$(<"$3")
  fi
  TASTE_PROJECT=$1
  TASTE_WORKSPACE=$2
fi

if [[ ! "$TASTE_PROJECT" =~ ^[a-z][a-z0-9-]{4,28}[a-z0-9]$ ]]; then
  echo "$TASTE_PROJECT is not a GCP project id" >&2
  exit 2
fi
if [[ ! "$TASTE_WORKSPACE" =~ ^[0-9a-f]{8}$ ]]; then
  echo "$TASTE_WORKSPACE is not a workspace id (eight lowercase hex digits)" >&2
  exit 2
fi

PROJECT=$TASTE_PROJECT
PROVIDER=taste-$TASTE_WORKSPACE
SUBJECT=taste-$TASTE_WORKSPACE
PROJECT_NUMBER=$(gcloud projects describe "$PROJECT" --format='value(projectNumber)')
MEMBER="principal://iam.googleapis.com/projects/$PROJECT_NUMBER/locations/global/workloadIdentityPools/$POOL/subject/$SUBJECT"

if [[ "$MODE" == remove ]]; then
  gcloud projects remove-iam-policy-binding "$PROJECT" --condition=None \
    --role="projects/$PROJECT/roles/$ROLE" --member="$MEMBER" >/dev/null 2>&1 || true
  gcloud iam workload-identity-pools providers delete "$PROVIDER" \
    --location=global --workload-identity-pool="$POOL" --project="$PROJECT" 2>/dev/null || true
  echo "Workspace $TASTE_WORKSPACE no longer has access to $PROJECT."
  echo "Its machines and disks still in the project, which keep costing until deleted:"
  gcloud compute instances list --project="$PROJECT" \
    --filter="labels.taste-workspace=$TASTE_WORKSPACE" --format='value(name,zone,status)'
  gcloud compute disks list --project="$PROJECT" \
    --filter="labels.taste-workspace=$TASTE_WORKSPACE" --format='value(name,zone,sizeGb)'
  exit 0
fi

# One PEM certificate and nothing else, since it is written into a YAML
# file below.
PEM_LINES=$(printf '%s\n' "$TASTE_CA_PEM")
if [[ "$(head -n1 <<<"$PEM_LINES")" != "-----BEGIN CERTIFICATE-----" ]] \
  || [[ "$(tail -n1 <<<"$PEM_LINES")" != "-----END CERTIFICATE-----" ]] \
  || [[ "$(grep -c -- '-----BEGIN' <<<"$PEM_LINES")" != 1 ]] \
  || grep -qvE '^(-----(BEGIN|END) CERTIFICATE-----|[A-Za-z0-9+/=]+)$' <<<"$PEM_LINES"; then
  echo "the CA must be exactly one PEM certificate" >&2
  exit 2
fi

gcloud services enable "${SERVICES[@]}" --project="$PROJECT"

PERMISSION_LIST=$(IFS=,; echo "${PERMISSIONS[*]}")
if gcloud iam roles describe "$ROLE" --project="$PROJECT" >/dev/null 2>&1; then
  gcloud iam roles update "$ROLE" --project="$PROJECT" \
    --permissions="$PERMISSION_LIST" >/dev/null
else
  gcloud iam roles create "$ROLE" --project="$PROJECT" --stage=GA \
    --title="taste-ide" --description="What the taste-ide cloud machines call" \
    --permissions="$PERMISSION_LIST" >/dev/null
fi

gcloud iam workload-identity-pools describe "$POOL" --location=global \
  --project="$PROJECT" >/dev/null 2>&1 \
  || gcloud iam workload-identity-pools create "$POOL" --location=global \
    --project="$PROJECT" --display-name="taste-ide"

TRUST=$(mktemp)
trap 'rm -f "$TRUST"' EXIT
{
  echo "trustStore:"
  echo "  trustAnchors:"
  echo "  - pemCertificate: |"
  sed 's/^/      /' <<<"$PEM_LINES"
} >"$TRUST"

STATE=$(gcloud iam workload-identity-pools providers describe "$PROVIDER" \
  --location=global --workload-identity-pool="$POOL" --project="$PROJECT" \
  --format='value(state)' 2>/dev/null || true)
if [[ "$STATE" == DELETED ]]; then
  gcloud iam workload-identity-pools providers undelete "$PROVIDER" \
    --location=global --workload-identity-pool="$POOL" --project="$PROJECT"
fi
if [[ -n "$STATE" ]]; then
  gcloud iam workload-identity-pools providers update-x509 "$PROVIDER" \
    --location=global --workload-identity-pool="$POOL" --project="$PROJECT" \
    --trust-store-config-path="$TRUST"
else
  gcloud iam workload-identity-pools providers create-x509 "$PROVIDER" \
    --location=global --workload-identity-pool="$POOL" --project="$PROJECT" \
    --trust-store-config-path="$TRUST" \
    --attribute-mapping="google.subject=assertion.subject.dn.cn" \
    --attribute-condition="assertion.subject.dn.cn == '$SUBJECT'"
fi

gcloud projects add-iam-policy-binding "$PROJECT" --condition=None \
  --role="projects/$PROJECT/roles/$ROLE" --member="$MEMBER" >/dev/null

echo
echo "Done. Give the IDE this project number: $PROJECT_NUMBER"
