# Spike: GLM-5.3 on GCP

The research behind ENVIRONMENTS.md → "A model on a cloud VM: GLM-5.3 on
GCP". Done 2026-10-01 and recorded the way `vm-substrate.md` records its
measurements: every fact below has a command or a source beside it. No
product code changed. **Nothing here was measured on a running VM** —
throughput, load time, and cost per task are estimates until Phase 2's
bake-off replaces them, and they are labelled as estimates wherever they
appear.

**Conclusion up front: GLM-5.3, unsloth's Q8_0 GGUF, on llama.cpp at or
after the GLM indexer support (#25407), with its sparse attention running,
on an on-demand machine in the user's own GCP project, chosen by measured
cost per task among `c4-highmem-192`, `c4d-highmem-192`, and
`g4-standard-192`.** The faithful-FP8 GPU stack (8× H200) was rejected on
capacity, Q4 and GLM-5.3-Flash on capability, GLM-5.2 on Vertex on a
measured capability gap, and nothing was rejected on speed, because speed
was never a requirement.

## What was asked for

David, 2026-10-01, in the order he said it:

- "I want another agent option where the IDE provisions necessary
  resources on GCP to run GLM-5.3 with a goal of a 2-10 token/second rate
  (or whatever you suggest that keeps cost strongly under control)."
- "I don't entirely trust GLM-5.3, so I want the VM sandboxed for egress.
  I only want my IDE client to be able to access it."
- "I don't want any compromise on the model's capabilities. 'Flash' is
  not good enough. Capability is more important than speed."
- "I really don't care about token speed as long as cost is managed.
  Capability comes first, and I'd rather no[t] play capacity games."
- Asked whether a GLM-5.3 chat's own commands should also run with the
  environment's egress cut: "Yes, cut it."

So the requirements, ranked: capability first; cost managed; no capacity
games; speed irrelevant; the model's machine reachable by the IDE alone,
and reaching nothing itself.

## The model

| Fact | Value | Source |
| --- | --- | --- |
| Parameters | 753B total, ~40B active per token | model card |
| Layers | 78, plus one MTP block (the GGUF's `block_count` is 79) | `config.json`; GGUF header |
| Experts | 256 routed, 8 per token, plus one shared; first 3 layers dense | `config.json` |
| Attention | MLA with DeepSeek Sparse Attention (DSA): a lightning indexer picks the top 2048 keys per query | `config.json` (`index_topk: 2048`) |
| Indexer sharing | "full" indexers on layers 0–2 and every fourth layer from 6; the rest reuse the previous full layer's selection | `config.json` (`indexer_types`, `index_topk_freq: 4`, `index_skip_topk_offset: 3`) |
| Context | 1,048,576 tokens | `config.json` (`max_position_embeddings`) |
| Released precision | BF16 and FP8 (E4M3) | model card |
| Relation to GLM-5.2 | "the same base model as GLM-5.2 — every gain comes from post-training" | model card |

The attention configuration of 5.3 is field-for-field that of 5.2, which
is what lets llama.cpp's GLM-5.2 work apply unchanged:

```sh
for r in zai-org/GLM-5.3 zai-org/GLM-5.2; do
  curl -sSL https://huggingface.co/$r/resolve/main/config.json |
    python3 -c 'import json,sys; c=json.load(sys.stdin); print({k: v for k, v in c.items() if "index" in k})'
done
```

## Why not GLM-5.2, and not Flash

GLM-5.3-Flash (320B, 18B active, a different hybrid attention) was ruled
out by David before it was costed. GLM-5.2 came back in as the one
faithful option with no capacity question at all — Vertex AI serves it as
a managed, per-token API inside the user's own project
([GLM models on Vertex](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/maas/zaiorg),
which lists 4.7, 5, and 5.2; not 5.3) — and was ruled out on the gap.
Z.ai's own numbers, from the GLM-5.3 model card (vendor-reported, on
Z.ai's harness):

| What it measures | Benchmark | 5.2 | 5.3 |
| --- | --- | --- | --- |
| Finding vulnerabilities | CyberGym | 77.2 | 84.5 |
| Exploiting them | ExploitBench | 24.4 | 54.4 |
| | ExploitGym (2h/6h) | 29/39 | 105/130 |
| Long-horizon agent work | Terminal-Bench 3.0 | 4.6 | 28.3 |
| | SWE-Marathon (v1.1) | 19.4 | 42.5 |
| | DeepSWE (v1.1) | 46.2 | 66.9 |
| Tool use | Toolathlon Verified | 59.9 | 73.0 |
| Shorter agent tasks | Terminal-Bench 2.1 | 81.0 | 88.2 |

For defensive security work driven through Claude Code the gap is
largest exactly where the work is: single-pass review is ~7 points apart,
while long autonomous loops (where 5.2 collapses on Terminal-Bench 3.0)
and the confirming half of the work (reproducing a finding, rating it,
checking a patch closes it) more than double. A measured, large gap lost
to an unmeasured, plausibly small one, which is what the llama.cpp path
turned out to be.

## Engines: who runs GLM-5.3's sparse attention, and where

| Engine | Sparse attention for `glm_moe_dsa` | Hardware | Notes |
| --- | --- | --- | --- |
| vLLM | Yes | Hopper and Blackwell GPUs; ROCm; Gaudi (`vllm-gaudi` #1760) | Not Ampere: "the sparse-MLA attention backend and the lightning-indexer are Hopper/Blackwell-only" ([gist](https://gist.github.com/timinar/c8d2eca4e2ea7d11db57a1e6e62d06a2)) |
| vLLM, CPU backend | **No** | — | "Sparse Attention is not supported on CPU" ([#57345](https://github.com/vllm-project/vllm/issues/57345)); the CPU sparse-MLA and indexer kernels in v0.30.0 are DeepSeek-V4 only ([#55355](https://github.com/vllm-project/vllm/pull/55355)); a GLM CPU draft exists for 5.3-Flash only ([#57687](https://github.com/vllm-project/vllm/pull/57687)) |
| SGLang | Yes | GPUs (H200, B200, B300, GB300, MI300X-class) | Serves `/v1/messages` itself ([docs](https://docs.sglang.io/docs/basic_usage/anthropic_api)); its Intel AMX CPU backend has no DSA |
| KTransformers | Yes, through SGLang | GPU for attention, CPU for experts | Needs a GPU, so it inherits the GPU's capacity question |
| **llama.cpp** | **Yes, since 2026-07-24** | **CPU and CUDA** | DSA in generic GGML ([#23346](https://github.com/ggml-org/llama.cpp/pull/23346), 2026-05-29), the fused `GGML_OP_LIGHTNING_INDEXER` with a CPU kernel ([#24231](https://github.com/ggml-org/llama.cpp/pull/24231), 2026-07-11), GLM-5.2's per-layer full/shared indexers ([#25407](https://github.com/ggml-org/llama.cpp/pull/25407), merged 2026-07-24 as `88bfee1`), CUDA kernels ([#25545](https://github.com/ggml-org/llama.cpp/pull/25545)). Serves `/v1/messages` itself, which the private route already relies on |

A correction worth keeping, because the wrong answer is the one a search
finds first: llama.cpp's DSA feature request,
[#20363](https://github.com/ggml-org/llama.cpp/issues/20363), is "closed
as not planned", and this research first read that as "llama.cpp runs
GLM-5.x densely". It was closed because the work landed through the PRs
above instead. The original GLM architecture PR
([#19460](https://github.com/ggml-org/llama.cpp/pull/19460), "indexer is
not yet supported") is the one that ran densely.

## Checking that the sparse path is the one that runs

Three things have to hold, and each was checked rather than assumed:

1. **The layer pattern.** llama.cpp has no `indexer_types` key in older
   GGUFs, so it infers the pattern from the context length: below 1M it
   assumes GLM-5/5.1 (every layer full); at 1M it uses
   `GLM_5_2_DEFAULT_INDEXER_TYPES` (`src/models/glm-dsa.cpp`, read at
   `master` on 2026-10-01). That table is `1, 1` followed by nineteen
   `1, 0, 0, 0` groups — 78 entries, full at 0, 1, 2, 6, 10, … — which is
   GLM-5.3's `indexer_types` exactly.
2. **The tensors.** The indexer tensors are created `TENSOR_NOT_REQUIRED`,
   so a GGUF without them loads and **silently runs dense**. The pinned
   files have them:

   ```sh
   build-aux/gguf-tensors.py unsloth/GLM-5.3-GGUF \
     346b3591c7f28d1a23716f97a065ecf12ec14771 Q8_0 17
   # glm-dsa.block_count 79
   # glm-dsa.context_length 1048576
   # glm-dsa.attention.indexer.head_count 32
   # glm-dsa.attention.indexer.key_length 128
   # glm-dsa.attention.indexer.top_k 2048
   # tensors 1809 indexer tensors 395
   # blocks with indexer tensors: [0, 1, …, 78]
   ```

   That is five indexer tensors in every one of the 79 blocks. The script
   reads only shard headers (ranged requests, well under 100 MB for the
   set) and is re-run whenever the pin moves.
3. **The context length selects 5.2's pattern.** `context_length` is
   1048576, so the 5.2 table above is the one in force.

What remains unchecked until a VM runs it: that the pinned llama.cpp build
logs the indexer as loaded. Phase 2 reads that from the server's load log
and fails the bake-off if it does not.

## What still differs from Z.ai's own stack

- **Tool calls.** Unsloth edited GLM's chat template because many engines
  do not support its original notation
  ([unsloth docs](https://unsloth.ai/docs/models/glm-5.3)), and llama.cpp
  parses GLM tool calls with its own code rather than Z.ai's vLLM/SGLang
  parser. **This is now the main unmeasured deviation**, and the one that
  matters most for an agent: Claude Code works entirely through tool
  calls, and security work is escaping-heavy (regexes, YARA and Sigma
  rules, shell, payload strings), which is where a parser is likeliest
  to differ. Most parser faults are loud; the dangerous one is an Edit
  that lands with one backslash changed.
- **Weights.** Q8_0 quantized from BF16, against the official FP8. Both
  are near-lossless; for DeepSeek-V3.2 under the same code, Q8_0's
  perplexity with the indexer was 2.9126 against 2.9115 without it
  (#23346), which bounds nothing about quantization but shows the scale
  of differences at stake.
- **Indexer arithmetic.** llama.cpp computes the indexer in F32 on CPU;
  the reference kernels use FP8. If anything that is closer to full
  precision; the top-k selections can still differ at the margin.

The public evidence on dense versus sparse attention is DeepSeek-V3.2's,
from the person who later implemented DSA in llama.cpp: dense first
looked equal or better on lineage-bench, and on revisiting harder
problems, "using dense attention makes the model a bit dumber"
([HF discussion](https://huggingface.co/deepseek-ai/DeepSeek-V3.2/discussions/35)).
The same work found its first sparse implementation at ~70% against the
API's 90% on lineage-128, because of a Hadamard-transform bug, and 95%
after the fix ([discussion #21183](https://github.com/ggml-org/llama.cpp/discussions/21183)).
That second fact is the reason for the acceptance test in Phase 5: an
implementation bug in an indexer is as silent as having no indexer.

## The weights pin

`unsloth/GLM-5.3-GGUF` at commit
`346b3591c7f28d1a23716f97a065ecf12ec14771` (also `main` on 2026-10-01,
last modified 2026-08-29), quant `Q8_0`, 17 shards, 801,357,677,216 bytes
(746.3 GiB). Sizes and digests from the Hub's tree API:

```sh
curl -sS https://huggingface.co/api/models/unsloth/GLM-5.3-GGUF/tree/346b3591c7f28d1a23716f97a065ecf12ec14771/Q8_0
```

| Shard | Bytes | SHA-256 |
| --- | --- | --- |
| 00001 | 48305489632 | `7fe0aacb07c33f113aed536300da6fb7fb1cf09e6202d67b6c178a2b878f4644` |
| 00002 | 49105526048 | `ccc253748f43c3167a3801ab2178cead3042f2248c4624297b95c5aa62180873` |
| 00003 | 49299773536 | `4829543a4bd76e7fe82c87a188a83bcdf012ccd5f6ba22e328f0c06ed46165d3` |
| 00004 | 49065113632 | `7a16851636b4e0fbbd59399b5231f5c84793d588091431b3801f11cd76589c41` |
| 00005 | 49065113600 | `f901d41f1c57d921d90dfb9860e3a2ab236558379a07fd3ca2e06e779b3e7003` |
| 00006 | 48843319392 | `5f8b0c68a93415868cd3b61e3d23c96370ce1269a0c6658750fe5833347acf55` |
| 00007 | 49065113600 | `f9a43526c53ab912da0ac54372dfd5c71e0e0064ad2a7177031bc1ee697d1cbf` |
| 00008 | 49065113632 | `90ef2b86ae70ee0f4ff8f04e4be041d8707f427c7329eda80b36bb61679bf1da` |
| 00009 | 48843319392 | `8be38eb8deecb6f01c858b06d13bf2be3515ed4eb036cfc6fce589e1b1c45d9d` |
| 00010 | 49065113600 | `3aafa3a4e42506356bafb16ce54ad13a344a42445f1f11421787747e5ebc6b5d` |
| 00011 | 49065113632 | `a57b126586a31baee6463df66b006c54359892954021272c2191c25743d41d0d` |
| 00012 | 48883731776 | `bbd9238c813c90d9a1645e22e38be5231ca1c056498569486eeb9566f7906ffa` |
| 00013 | 49065113632 | `54089f96058c3f058227121a93cdd978d03601205c4893ec2f96d4c1f38924a7` |
| 00014 | 49065113600 | `28157088d398e3564442985c2183f617b1bc4979420ce8767ea883ba803f6d68` |
| 00015 | 48843319392 | `f8601930e67ca579556041f59356ae1ba66d8c9cae11252c3a60048038d7e2e6` |
| 00016 | 49155939904 | `6c9b48a9d49082fa8fc329edf59a1db4eb90ef58f7e9cfcc6d06ac204cf023a7` |
| 00017 | 17556349216 | `79cd14a05a4c1b867e7d56bf1faaea2f7a88b1063fd992e4f9983a6168f42cdd` |

The BF16 set is 1404.4 GiB in 33 files and buys nothing measurable over
Q8_0; UD-Q4_K_XL is 435.2 GiB and is the capability compromise this plan
exists to avoid.

The llama.cpp build is pinned in Phase 2, when the bake-off first runs it:
a release at or after `88bfee1` (the newest on 2026-10-01 was `b11321`),
by image digest, never by tag.

## Machines

Prices are third-party trackers' as of 2026-10-01
([gcloud-compute.com](https://gcloud-compute.com/c4-highmem-192.html),
[cloudprice.net](https://cloudprice.net/gcp/compute/instances/g4-standard-192),
[devzero](https://www.devzero.io/instances/gcp/a3-ultragpu-8g)), us-central1
unless noted. Phase 1 replaces them with the Cloud Billing Catalog API's
own SKU prices, which is also what the ledger meters against.

| Machine | Memory | On-demand | Spot | Verdict |
| --- | --- | --- | --- | --- |
| `c4-highmem-192` | 1488 GB; Intel Granite or Emerald Rapids (AMX) | $12.51/h | $7.49/h | Candidate |
| `c4d-highmem-192` | 1512 GB; AMD Turin | from $11.95/h | $5.08/h | Candidate |
| `g4-standard-192` | 720 GiB RAM + 4× RTX PRO 6000 (384 GiB) | $18.00/h | not quoted | Candidate: experts split across GPU and CPU, GPU-speed prompt reading; G4 is in 40+ zones |
| `c3-highmem-176` | 1408 GB; Intel Sapphire Rapids | $11.64/h | — | Fallback if C4 quota is refused |
| `c4d-highmem-96`, `g4-standard-96` | 756 GB; 360 GiB + 192 GB | — | — | Too small for Q8_0 plus its cache |
| `a3-ultragpu-8g` | 8× H200, 1128 GB HBM | none: Spot, Flex-start, or a reservation only | ~$30–55/h (trackers disagree by 7×) | Rejected: capacity |

Why the H200 was rejected, which is the "capacity games" David declined:
GPU quota starts at zero and must be granted (preemptible H200 quota of
8); A3 Ultra exists in eight zones worldwide, one per region; it cannot be
created on-demand; Spot can be refused at start or reclaimed on 30
seconds' notice; Flex-start queues a request up to two hours and then runs
it up to seven days ([About Flex-start VMs](https://docs.cloud.google.com/compute/docs/instances/about-flex-start-vms));
and the weights disk is zonal, so a workspace is pinned to one of those
eight zones. A reservation removes all of that and bills around the
clock.

**Cost per task, not cost per hour, is the metric the bake-off ranks by.**
Billing is hourly, so a slow machine is not a cheap one. An estimate,
for a mid-size Claude Code task of ~30 model calls, the context growing to
~85k tokens (~85k tokens read in total, with the server's prefix cache),
and ~30k tokens written, since GLM reasons at length:

| | Reading | Writing | Task | Cost per task |
| --- | --- | --- | --- | --- |
| CPU, Q8_0 (estimate) | ~50–150 tok/s → 10–28 min | ~2–5 tok/s, slowing with context → 1.7–4.2 h | ~2–4.5 h | ~$25–55 on-demand |
| 8× H200, FP8 (estimate) | seconds | ~40–80 tok/s → 6–12 min | ~10–30 min with a cold start | ~$10–25 |

The CPU estimates assume decode bound by memory bandwidth (~42 GB read
per token at Q8_0's ~40B active parameters) and llama.cpp's DSA computing
the full KQ and masking it (#23346), which saves no work at long context.
Phase 2 replaces every cell with a measurement, `g4-standard-192`
included.

## Storage

801 GB of weights want an ~850 GiB disk. On Hyperdisk Balanced the first
3,000 IOPS and 140 MiB/s are free and anything above is billed per month
([Hyperdisk Balanced](https://docs.cloud.google.com/compute/docs/disks/hd-types/hyperdisk-balanced)).
At the free 140 MiB/s a cold load takes ~95 minutes; at 2,400 MiB/s,
~5.5. Capacity alone is on the order of $50–100/month standing, plus
whatever throughput is held while stopped.

Two levers, each for Phase 2 to measure rather than assume: raising the
disk's provisioned throughput before a start and lowering it after a stop
(Hyperdisk limits how often performance can be changed), and parking —
deleting the disk after a stretch unused and re-staging from Hugging Face
on the next use (an ~800 GB download on a small VM, tens of minutes and
well under a dollar of compute).

## Network lockdown: what GCP lets a firewall close, and what it does not

- **The metadata server is always reachable.** A VM can always talk to
  its metadata server at 169.254.169.254, whatever firewall rules say
  ([VPC firewall rules](https://docs.cloud.google.com/firewall/docs/firewalls)).
- **DNS goes through it**, so a deny-all egress rule leaves DNS open, and
  DNS is an exfiltration channel. Cloud DNS **response policies** can
  only supply local data or bypass the policy — there is no NXDOMAIN
  behaviour ([response policies](https://docs.cloud.google.com/dns/docs/zones/manage-response-policies))
  — so they cannot answer "nothing" for every name. A DNS **server
  policy** with an alternative name server on an unassigned private
  address forwards every query to a black hole
  ([server policies](https://docs.cloud.google.com/dns/docs/server-policies-overview)).
  Phase 2 verifies from inside the VM that a public name does not
  resolve.
- **No service account means no tokens.** A VM created with none gets no
  credentials from the metadata server, so Google APIs are unusable to it
  even if they were reachable; Private Google Access stays off on the
  subnet regardless.
- **Guest attributes** are how the VM reports to the IDE without a
  network path out: written to the metadata server by IP (DNS being dead),
  read back by the IDE through the authenticated Compute API.

## What the cap buys

An estimate, from the prices above and before any measurement. Standing
costs (the weights disk) take ~$50–100 of $300, leaving ~$200–250 of
running time in a 730-hour month:

| Machine | Rate | Hours | Share of the month | Per working day (22) |
| --- | --- | --- | --- | --- |
| `c4-highmem-192` | $12.51/h | 16–20 | 2.2–2.7% | ~45–55 min |
| `c4d-highmem-192` | $11.95/h | 17–21 | 2.3–2.9% | ~45–55 min |
| `g4-standard-192` | $18.00/h | 11–14 | 1.5–1.9% | ~30–40 min |

At the CPU estimate of $25–55 per mid-size task that is roughly four to
ten tasks a month, and each session also pays ~$4.50 of overhead (a
~6-minute cold load and the 15-minute idle tail). $300 is a low cap for
regular use; it stays the default until Phase 2's measurements say what a
task actually costs.

## Credentials and the tunnel (2026-10-02)

Two requests from David reshaped the access design: "My IP changes
frequently as I move my laptop around", and "Would there also be a way to
use the TPM or a security key instead of a service account key".

**Service-account keys are out twice over.** Organizations created on or
after 2024-05-03 enforce secure-by-default policies that disable both
service-account key **creation** and key **upload**
([secure by default organizations](https://cloud.google.com/resource-manager/docs/secure-by-default-organizations),
[baseline constraints](https://docs.cloud.google.com/resource-manager/docs/manage-baseline-constraints)),
so "generate the key in hardware and upload its public half" hits the
same wall as a downloaded key.

**Workload Identity Federation with X.509 certificates is the keyless
path**, and it is GA
([docs](https://docs.cloud.google.com/iam/docs/workload-identity-federation-with-x509-certificates)):

| Fact | Value |
| --- | --- |
| Keys | RSA 2048–4096, or ECDSA P-256 or P-384 |
| Chain | depth at most 5; up to 3 trust anchors and 10 intermediates |
| Leaf | `keyUsage=critical, digitalSignature, keyEncipherment`; `basicConstraints=critical, CA:FALSE`; valid at most 390 days |
| Subject mapping | `google.subject` from the subject CN by default, or a SAN, serial, or fingerprint |
| Exchange | `POST https://sts.mtls.googleapis.com/v1/token` over mTLS, `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`, `subject_token_type=urn:ietf:params:oauth:token-type:mtls`, `subject_token` a JSON array of base64 DER certificates, leaf first |
| Access | the federated principal directly (`principal://iam.googleapis.com/projects/N/locations/global/workloadIdentityPools/P/subject/S`), or by impersonating a service account |

A token bound to the certificate, so that one lifted from memory is
useless elsewhere, is described as possible
([mtls-tokensource](https://github.com/salrashid123/mtls-tokensource))
and is for Phase 1 to confirm.

**This host's TPM** (read 2026-10-03):

| Fact | Value |
| --- | --- |
| Device | TPM 2.0 (`/sys/class/tpm/tpm0/tpm_version_major` is 2) |
| Access | `/dev/tpmrm0` is `root:tss 0660`; the user is not in `tss` |
| The group | defined only in `/usr/lib/group` (`tss:x:59:clevis`), so on rpm-ostree `usermod -aG tss` needs the line copied into `/etc/group` first |
| Software on the host | `tpm2-tss` 4.1.3 and `tpm2-tools` 5.7; no `tpm2-pkcs11` |
| OpenSSH | 10.2p1 with `libfido2` 1.16.0, so `ed25519-sk` keys work; a TPM-held key would need `tpm2-pkcs11` |
| The IDE's ssh | run on the host (`flatpak-spawn --host` when sandboxed, `taste_core::podman::host_argv`) |

**The tunnel, weighed.** Without an address allowlist, the port is open
to everyone and authentication carries all of it:

| Option | Roaming | Before authentication, the internet sees | Verdict |
| --- | --- | --- | --- |
| **Mutual TLS, client key in the TPM** | every request is its own connection | the terminator's TLS handshake | **Chosen**: one hardware identity serves Google and the VM; nothing secret on disk; no tunnel process |
| SSH port-forward | reconnect on the next request | sshd | Set aside: the key is on disk unless `tpm2-pkcs11` is layered onto the host |
| IAP TCP forwarding | not applicable; no public IP | only Google | Set aside: the tunnel protocol is implemented by `gcloud` and IAP Desktop and documented by neither ([IAP Desktop](https://github.com/GoogleCloudPlatform/iap-desktop)) |
| WireGuard | native | nothing; it does not answer unauthenticated packets | Set aside for now: needs a WireGuard and TCP stack in-process; the hardening if the TLS residual has to go |

