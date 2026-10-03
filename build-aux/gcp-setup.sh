#!/usr/bin/env bash
# Set a GCP project up for taste-ide's cloud machines, or take it back out.
#
# This is the whole of what a project owner does for the IDE's cloud
# machines (ENVIRONMENTS → "A model on a cloud VM"). It runs as you, with
# gcloud signed in: the IDE runs this same file with its own pinned gcloud,
# signed in to the project's own configuration (`taste_gcp::gcloud`), never
# one installed on the base system, and it runs just the same by hand or
# in Cloud Shell. It is safe to run again, which is how a role that gained
# a permission is repaired.
#
#   build-aux/gcp-setup.sh PROJECT
#   build-aux/gcp-setup.sh --remove PROJECT
#
# ## What it creates, and what it grants
#
# - The APIs the IDE calls.
# - A custom role, `tasteIde`, of exactly the calls the IDE makes: the
#   machines' networks, firewall rules, DNS policy, and instances, the
#   Private Google Access the model's VM loads its weights through, the
#   weights bucket and its objects, the operations, quota limits, and
#   machine facts it reads, and tunnelling to the model's VM through IAP. Nothing
#   in it touches billing, IAM, or a service account, so whatever holds it
#   can spend money only by running machines and storing weights, which
#   the IDE's cap meters, and cannot widen its own reach.
# - A service account, `taste-ide@PROJECT.iam.gserviceaccount.com`, with
#   no keys, holding that role. The IDE's own calls run as it.
# - Permission for that account to sign as itself (Service Account Token
#   Creator, on itself alone): the signed URLs a VM with no credential
#   reads and writes the bucket with. It can sign as no other account.
# - Permission for you — whoever runs this — to act as that account
#   (Service Account Token Creator, on that one account), which is how
#   the IDE's calls, made with your sign-in, run with its privileges and
#   no more of yours.
#
# No key is ever created: organizations created since May 2024 forbid
# service-account keys by default, and a key file is exactly what a copied
# disk would hand over.
#
# ## --remove
#
# Deletes the service account and its grant, so the IDE can no longer act
# in the project from any workspace; the role stays, inert without a
# holder. Then it lists whatever of the IDE's machines and disks are still
# in the project, because anything left running keeps costing.
set -euo pipefail
export CLOUDSDK_CORE_DISABLE_PROMPTS=1

ROLE=tasteIde
ACCOUNT=taste-ide

# The APIs the IDE calls. Kept equal to `taste_gcp::setup::SERVICES` by a
# test that reads this file.
SERVICES=(
  cloudbilling.googleapis.com
  cloudquotas.googleapis.com
  cloudresourcemanager.googleapis.com
  compute.googleapis.com
  dns.googleapis.com
  iam.googleapis.com
  iamcredentials.googleapis.com
  iap.googleapis.com
  storage.googleapis.com
)

# Exactly what the IDE calls. Kept equal to
# `taste_gcp::setup::PERMISSIONS` by a test that reads this file, and the
# IDE's preflight asks `testIamPermissions` for the same list.
PERMISSIONS=(
  cloudquotas.quotas.get
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
  compute.subnetworks.setPrivateIpGoogleAccess
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
  iap.tunnelInstances.accessViaIAP
  resourcemanager.projects.get
  storage.buckets.create
  storage.buckets.get
  storage.objects.create
  storage.objects.delete
  storage.objects.get
  storage.objects.list
)

usage() {
  echo "usage: $0 [--remove] PROJECT" >&2
  exit 2
}

MODE=setup
if [[ "${1:-}" == "--remove" ]]; then
  MODE=remove
  shift
fi
[[ $# -eq 1 ]] || usage
PROJECT=$1
if [[ ! "$PROJECT" =~ ^[a-z][a-z0-9-]{4,28}[a-z0-9]$ ]]; then
  echo "$PROJECT is not a GCP project id" >&2
  exit 2
fi

EMAIL="$ACCOUNT@$PROJECT.iam.gserviceaccount.com"
ME=$(gcloud config get-value account 2>/dev/null || true)
if [[ -z "$ME" ]]; then
  echo "gcloud is not signed in; run 'gcloud auth login' first" >&2
  exit 1
fi
if [[ "$ME" == *.gserviceaccount.com ]]; then
  ME_MEMBER="serviceAccount:$ME"
else
  ME_MEMBER="user:$ME"
fi

# A service account just created takes a little while to be visible to
# IAM, so grants to it are retried rather than failed.
retry() {
  local tries=0
  until "$@"; do
    tries=$((tries + 1))
    if [[ $tries -ge 6 ]]; then
      return 1
    fi
    sleep 10
  done
}

if [[ "$MODE" == remove ]]; then
  gcloud projects remove-iam-policy-binding "$PROJECT" --condition=None \
    --role="projects/$PROJECT/roles/$ROLE" \
    --member="serviceAccount:$EMAIL" >/dev/null 2>&1 || true
  gcloud iam service-accounts delete "$EMAIL" --project="$PROJECT" 2>/dev/null || true
  echo "The IDE can no longer act in $PROJECT."
  echo "Its machines and disks still in the project, which keep costing until deleted:"
  gcloud compute instances list --project="$PROJECT" \
    --filter="labels.taste-workspace:*" --format='value(name,zone,status)'
  gcloud compute disks list --project="$PROJECT" \
    --filter="labels.taste-workspace:*" --format='value(name,zone,sizeGb)'
  gcloud storage buckets list --project="$PROJECT" \
    --filter="labels.taste-role:weights" --format='value(name)'
  exit 0
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

gcloud iam service-accounts describe "$EMAIL" --project="$PROJECT" >/dev/null 2>&1 \
  || gcloud iam service-accounts create "$ACCOUNT" --project="$PROJECT" \
    --display-name="taste-ide" \
    --description="What the taste-ide cloud machines run as; it has no keys"

retry gcloud projects add-iam-policy-binding "$PROJECT" --condition=None \
  --role="projects/$PROJECT/roles/$ROLE" \
  --member="serviceAccount:$EMAIL" >/dev/null

retry gcloud iam service-accounts add-iam-policy-binding "$EMAIL" \
  --project="$PROJECT" --condition=None \
  --role=roles/iam.serviceAccountTokenCreator \
  --member="$ME_MEMBER" >/dev/null

retry gcloud iam service-accounts add-iam-policy-binding "$EMAIL" \
  --project="$PROJECT" --condition=None \
  --role=roles/iam.serviceAccountTokenCreator \
  --member="serviceAccount:$EMAIL" >/dev/null

echo
echo "Done. The IDE acts in $PROJECT as $EMAIL, which $ME may act as."
