#!/usr/bin/env python3
"""Read a split GGUF's tensor names off Hugging Face without downloading it.

The cloud model's weights are pinned by commit and digest
(docs/spikes/glm-on-gcp.md → The weights pin), but a digest says only that
the bytes are the bytes; it does not say the file carries what the engine
needs to run the model as trained. For GLM-5.3 that matters in one
specific way: llama.cpp marks the DSA indexer tensors `TENSOR_NOT_REQUIRED`
(`src/models/glm-dsa.cpp`), so a GGUF converted without them loads without
complaint and runs DENSE attention, which is not the model Z.ai evaluated.
This script is the check that the pinned files have them, and it is re-run
whenever the pin moves.

Only each shard's header is fetched — a ranged request for its first few
MiB, which holds the key-value metadata and the tensor infos — so checking
the whole 801 GB set moves well under a hundred megabytes. curl does the
fetching because it keeps the Range header across Hugging Face's redirect
to its CDN; urllib does not, and a check that silently starts downloading
48 GB per shard is a check nobody runs twice.

    build-aux/gguf-tensors.py unsloth/GLM-5.3-GGUF <commit> Q8_0 17

prints the architecture's indexer and context metadata, how many tensors
are indexer tensors, and which blocks carry them. It exits non-zero when
no block does.
"""

import struct
import subprocess
import sys

# Header bytes fetched per shard. The first shard also holds the
# tokenizer's vocabulary, which is several MiB on its own.
FIRST_SHARD_BYTES = 16 << 20
OTHER_SHARD_BYTES = 4 << 20

SCALAR_FORMATS = {
    0: "B", 1: "b", 2: "H", 3: "h", 4: "I", 5: "i",
    6: "f", 7: "B", 10: "Q", 11: "q", 12: "d",
}
STRING = 8
ARRAY = 9
GGUF_MAGIC = 0x46554747


def fetch_prefix(url, length):
    return subprocess.run(
        ["curl", "-sSfL", "--max-time", "120", "-r", f"0-{length - 1}", url],
        check=True,
        capture_output=True,
    ).stdout


class Reader:
    def __init__(self, data):
        self.data = data
        self.offset = 0

    def scalar(self, fmt):
        (value,) = struct.unpack_from("<" + fmt, self.data, self.offset)
        self.offset += struct.calcsize("<" + fmt)
        return value

    def string(self):
        length = self.scalar("Q")
        value = self.data[self.offset:self.offset + length].decode()
        self.offset += length
        return value

    def value(self, kind):
        if kind == STRING:
            return self.string()
        if kind == ARRAY:
            element = self.scalar("I")
            count = self.scalar("Q")
            return [self.value(element) for _ in range(count)]
        return self.scalar(SCALAR_FORMATS[kind])


def read_header(url, length):
    reader = Reader(fetch_prefix(url, length))
    if reader.scalar("I") != GGUF_MAGIC:
        raise SystemExit(f"{url}: not a GGUF file")
    reader.scalar("I")  # version
    tensor_count = reader.scalar("Q")
    kv_count = reader.scalar("Q")
    metadata = {}
    for _ in range(kv_count):
        key = reader.string()
        metadata[key] = reader.value(reader.scalar("I"))
    names = []
    for _ in range(tensor_count):
        names.append(reader.string())
        dims = reader.scalar("I")
        for _ in range(dims):
            reader.scalar("Q")
        reader.scalar("I")  # type
        reader.scalar("Q")  # offset
    return metadata, names


def main():
    if len(sys.argv) != 5:
        raise SystemExit(__doc__)
    repo, commit, quant, total = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    model = repo.split("/")[-1].removesuffix("-GGUF")
    names = []
    for shard in range(1, total + 1):
        url = (
            f"https://huggingface.co/{repo}/resolve/{commit}/{quant}/"
            f"{model}-{quant}-{shard:05d}-of-{total:05d}.gguf"
        )
        length = FIRST_SHARD_BYTES if shard == 1 else OTHER_SHARD_BYTES
        metadata, shard_names = read_header(url, length)
        names += shard_names
        if shard == 1:
            for key, value in metadata.items():
                if "indexer" in key or key.endswith((".context_length", ".block_count")):
                    print(key, value[:12] if isinstance(value, list) else value)
    indexer = [name for name in names if "indexer" in name]
    blocks = sorted({int(name.split(".")[1]) for name in indexer})
    print("tensors", len(names), "indexer tensors", len(indexer))
    print("blocks with indexer tensors:", blocks)
    if not blocks:
        raise SystemExit("no indexer tensors: llama.cpp would run this GGUF with dense attention")


if __name__ == "__main__":
    main()
