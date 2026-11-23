# Recall evaluation

`recall.jsonl` contains 40 bilingual query judgments. `semantic-recall.jsonl`
contains 40 cross-lingual semantic paraphrase judgments over 20 target memories,
designed to avoid repeating the target memory's distinctive words. English queries
target Chinese memories and Chinese queries target English memories. IDs use
`<memory-kind>:<database-id>`; the included 20-row corpus is an opt-in synthetic
fixture that makes the evaluation repeatable without changing normal app data.

Run against a dedicated temporary database and project scope:

```sh
cargo run --release -p pi-memory-core --bin pi-memory-eval -- \
  --db /tmp/pi-memory-eval.db --scope /tmp/pi-memory-eval-project --seed-fixture

cargo run --release -p pi-memory-core --bin pi-memory-eval -- \
  --db /tmp/pi-memory-semantic.db --scope /tmp/pi-memory-semantic-project \
  --dataset eval/semantic-recall.jsonl --corpus eval/corpus.jsonl --seed-fixture
```

The repository includes pinned candidate manifests for the multilingual MiniLM
and E5 models. Download the files at the revisions recorded in those manifests
into a flat local directory (the model and tokenizer basenames must be at its
root), then compare separate bundles with the same synthetic fixture:

```sh
eval/compare-models.sh \
  all-MiniLM-L6-v2 /path/to/minilm crates/core/models.lock.json \
  paraphrase-multilingual-MiniLM-L12-v2 /path/to/multilingual-minilm eval/manifests/paraphrase-multilingual-minilm-l12.json \
  multilingual-e5-small /path/to/e5 eval/manifests/multilingual-e5-small.json
```

For example, `hf download <repository> <remote-file> --revision <revision>` can
fetch each listed file. Keep the remote path from `files[].remote`, then place
the downloaded artifact at `files[].name` in the candidate directory. The
script does not download or execute untrusted model files on its own; runtime
manifest verification checks each local artifact before loading it. The
Chinese-only BGE candidate remains supported through a custom manifest, but is
not checked in until its tokenizer artifact hash is pinned alongside its ONNX
external-data files.

Each manifest pins its repository revision, model/tokenizer file names, all
required weight files (including ONNX external data), file sizes and SHA-256
hashes, plus dimension, maximum token length, and pooling (`mean` or `cls`).
Optional `query_prefix` and `document_prefix` fields support models such as E5. The
evaluator seeds a fresh DB per candidate, so each run tests vectors produced by
that model without mixing models or changing normal application data.

`--seed-fixture` upserts the `eval:*` working memories and their embeddings into
the selected database. It requires the configured embedding model to be ready.
Without that flag, the evaluator uses the existing database contents and model;
`--dataset` and `--corpus` can point to alternate JSONL files. Dataset rows have
`query`, `relevantIds`, `language`, and `kinds` fields. Relevant IDs should be
manually labeled for the database and project scope being evaluated, using the
`<kind>:<id>` form. The evaluator rejects empty queries/language/relevance sets,
malformed or duplicate relevant IDs, so invalid labels cannot silently lower
the aggregate metrics. Before scoring, it also verifies every relevant ID
exists in the selected project scope, including legacy unscoped structured
memories.

The report compares FTS-only, vector-only, and hybrid rankings using Recall@5,
Recall@10, MRR, and nDCG@10, both overall and grouped by language. It also prints
the active model ID, local model file size, model cold-start time, and per-query
end-to-end recall latency P50/P95, including per-language latency.
Add `--explain-misses` to print each relevant target's FTS/vector/hybrid rank and
whether it was removed by a score threshold or eligibility filter.
Cold start is measured from starting background model loading until the bounded
wait returns; recall latency includes storage retrieval, embedding inference,
ranking, and filtering. Use the same machine, database, scope, and corpus when
comparing runs. The synthetic fixture is a smoke dataset for ranking plumbing,
not a substitute for judgments collected from real project memories or a model
selection gate.

On 2026-11-23, the user chose to keep `all-MiniLM-L6-v2` as the active model;
candidate replacement is not a release requirement. The regular 40-case
bilingual fixture on a fresh DB reported hybrid Recall@5/10 of 0.775/0.850,
MRR 0.6525, nDCG@10 0.6941, and end-to-end recall latency P95 of 2.25 ms. The
cross-lingual 40-case semantic fixture reported hybrid Recall@5/10 of
0.525/0.650, versus 0.500/0.650 for vector-only (English hybrid Recall@5 0.6667,
Chinese 0.10); end-to-end latency P95 was 2.57 ms. These repeatable synthetic
fixtures are the current-model baseline, not a claim about real project-memory
quality. `--explain-misses` showed
that the previous cutoff removed relevant single-source vector candidates, while
common English query words were producing unrelated FTS candidates. The changes
improve this synthetic set, but it is not a model-selection gate: multilingual
candidate bundles and judgments from actual retained project memories are not
required while retaining the current model. The Chinese cross-lingual gap remains;
the 0.10 Chinese hybrid Recall@5 hit came from FTS, while vector-only stayed at
0.00. This limitation is accepted for the current-model decision and should be
revisited only if the user requests a model comparison or supplies new judgments.

An earlier local release run on 2026-11-21 reported hybrid Recall@5 `0.8667`,
Recall@10 `0.9333`, MRR `0.7812`, and nDCG@10 `0.8181` on the 30 synthetic
judgments. That run predates the final runtime/evaluator pipeline alignment, so
the numbers are historical plumbing evidence and must not be compared with
reports from the current implementation. The synthetic fixture is not a
model-selection or release-quality gate.
