# Hybrid retrieval and a natural-language task-context compiler

Research date: 2026-10-24.

Status: exploratory research concept proposed by the project owner. Not an
accepted architecture, implementation plan, or implemented Nexus capability.
The related-work review below is selective, not a systematic novelty search.

## Motivation and hypothesis

The proposed direction combines RAG with compiler-inspired processing of agent
context. Instead of treating history as text to retain, summarize, or retrieve
by similarity alone, maintain an explicit representation of task state,
evidence, dependencies, and the conditions under which conclusions remain
applicable. Construct model context as a projection of that state.

### Full proposed vision

Build a compiler-inspired agent semantic-memory and context-compression system
that transforms natural language, conversation, and tool execution records into
a structured **Semantic IR**. Borrow AST-like structure, types, dependency
analysis, and optimization passes to organize task semantics. Combine knowledge
graphs for entities, causal hypotheses, and dependencies with vector indexing
for semantic retrieval. Persist original records using lossless compression so
selected interpretations can always be traced back to retained source data.

At runtime, reconstruct a compact, task-relevant context rather than repeatedly
sending the entire history. Explore a purpose-trained **LoRA** adapter to help
models encode, read, and expand the IR. The long-term goal is high information
density and reliable long-running work, including with lightweight models and
limited context windows, while keeping the Agent Core minimal, deterministic,
and composable. These are research goals, not demonstrated compression ratios,
model capabilities, or implementation commitments.

The motivating SymPy run found the eventual correct fix early in its assistant
narrative but continued with repeated investigation, validation, and cleanup.
The [benchmark record](agent-benchmarks.md#why-convergence-was-slow-evidence-and-limits)
documents the observations and their limits. This does not establish that
memory retrieval or context eviction caused the repetition: environment defects
were present, and the saved text lacks complete tool traces and timing.

The hypothesis is that explicit evidence applicability and task dependencies
can reduce repeated work and stale conclusions without reducing correctness.
The target is a general long-task agent problem, not an optimization for one
benchmark, language, test framework, or hidden test.

## Compiler-inspired pipeline

```text
User requests, repository content, tool outcomes, and prior conversation
    -> candidate discovery through retrieval
    -> structured interpretation / task intermediate representation
    -> provenance, dependency, conflict, and applicability checks
    -> incremental task-state updates
    -> bounded context slice for the next decision
```

This is a conceptual decomposition, not a requirement to run every stage on
every turn. Retrieval can discover candidate evidence through keyword, symbol,
structural, or vector search. It does not itself determine whether a claim is
true, current, or authorized. Neither a vector database nor a graph database is
required to explore this model.

Unlike a programming language, natural language has ambiguous semantics.
Parsing it into structured data cannot establish correctness by itself.
Model-generated interpretations and inferred dependencies must remain
correctable proposals, not automatically authoritative facts.

## Candidate task intermediate representation

Useful record categories include:

- Goals, constraints, and explicit user corrections.
- Hypotheses and their supporting or contradicting evidence.
- Actions and their execution status, including uncertain effects.
- Artifacts and their versions, including the current candidate patch.
- Validation observations and the artifact/environment versions they cover.
- Open questions, failed approaches, and unresolved risks.

Records would need stable identities, provenance references, and applicability
conditions. Model assertions, user requirements, and recorded tool outcomes are
different kinds of information. A successful tool exit is an observation, not
automatic proof that the intended task outcome occurred.

For example:

```text
Claim: candidate repair is validated
Depends on: patch P1, environment E1, test observation T1

Current patch changes from P1 to P2
    -> T1 remains a historical observation
    -> T1 alone no longer establishes validation of P2
    -> revalidate the relevant dependency or mark applicability unknown
```

This separates historical truth from present applicability. Similarly, a later
user correction can supersede an earlier task requirement without erasing the
fact that it was previously requested. Dependencies that cannot be established
reliably must remain unknown; the system must not invent precise invalidation
rules from uncertain natural-language interpretations.

## Possible differentiators to investigate

The potentially useful contribution is not simply combining RAG with a graph or
calling structured extraction a compiler. It is giving the intermediate
representation operational semantics that can be checked:

- Typed distinctions between requirements, assumptions, observations, and claims.
- Explicit evidence provenance and artifact/environment version applicability.
- Incremental invalidation when dependencies change, without rewriting history.
- Decision-oriented context slicing rather than similarity-only selection.
- Reconstructable links from selected conclusions to original evidence.
- Safety-preserving treatment of pending operations and uncertain effects.

Whether this combination is novel or better than existing systems is unresolved.
That requires a broader literature/code review and comparative experiments.

## Storage, graph, and retrieval responsibilities

Separate the proposed system into three representations:

- **Source records:** losslessly compressed original events and content, with
  stable references and integrity checks. References must resolve within the
  retention policy; compression does not justify unbounded retention of private
  data. Deletion or expiry must also affect derived views and indexes.
- **Semantic view:** typed records and dependency edges derived from sources.
  Version the IR schema and extractor so interpretations can be corrected or
  rebuilt without rewriting the underlying execution history.
- **Search indexes:** keyword, symbol, structural, and vector indexes discover
  candidate evidence. A graph supports relationship traversal; neither an
  embedding match nor a graph edge establishes truth or grants authority.

Distinguish temporal order, evidence support, artifact dependency, and inferred
causality. An observation preceding another does not establish a causal edge.
Even a recorded tool result can contain untrusted assertions; source provenance
is not a blanket truth label.

Original-text recovery is different from semantic completeness. A small IR
cannot guarantee preservation of every detail needed by an unknown future
task. Recovery should include fetching referenced source evidence, not asking a
model to invent omitted details. Likewise, "minimum effective context" is an
optimization goal, not a provable universal minimum: aim for bounded context
that covers current constraints, applicable evidence, and unresolved risks,
with an explicit path to request more information.

## Compiler passes and execution boundary

Candidate passes include source-preserving deduplication, explicit supersession
of requirements, applicability updates after artifact/environment changes,
dependency slicing, and context-budget allocation. Each selected claim should
retain source mappings and each transformation should have an inspectable
reason. Do not assume semantic equivalence from textual similarity or apply
"dead-code elimination" to permanently erase apparently irrelevant evidence.

Probabilistic components propose interpretations, links, summaries, and context
slices. The deterministic execution boundary owns authorization, dispatch,
cancellation, operation identity, observed outcomes, and resource limits.
Semantic memory cannot override it. A validated IR schema can establish shape
and certain invariants, but cannot prove arbitrary extracted natural-language
claims. Prefer a small task-oriented IR over an attempted universal language
AST; the compiler analogy is a design aid, not a correctness guarantee.

## LoRA research direction

A specialized adapter could learn to extract candidate records, read typed IR,
follow evidence references, distinguish observations from hypotheses, and ask
for expansion when information is missing. It is not a mechanism for recovering
information absent from both the IR and available sources, nor a substitute for
deterministic checks or stronger reasoning capability.

Explore training only after an IR baseline and usable evaluation data exist.
Training examples should include contradictions, stale validation, corrections,
unknown effects, and cases requiring source expansion, not just successful
compression. Dataset privacy, licensing, and train/evaluation separation need
explicit treatment. Version adapters against the IR they understand; document
model/tokenizer dependence and retain a readable, untrained baseline.

Compare the same model with and without LoRA and separate retrieval quality,
IR extraction quality, representation density, and downstream task correctness.
Fewer characters or records do not necessarily mean fewer tokens. Report actual
token costs, extraction/training overhead, retrieval latency, and expansion
requests; do not attribute a larger model's advantage to the memory design.

### Verifier-driven synthetic semantic training

The project owner's proposed extension is to use LLMs to generate large amounts
of verifiable semantic training material, then train LoRA on the accepted
examples. Prefer generating an explicit task state and event sequence first,
computing reference IR and applicability under defined rules, and only then
asking an LLM to express that sequence as varied conversations or tool records.
This gives checkable targets rather than treating a generated summary as its
own ground truth.

```text
Explicit task state and event sequence
    -> reference IR and answers derived under defined semantics
    -> LLM-generated language variations and distractors
    -> fidelity checks, filtering, and sampled human review
    -> accepted extraction/update/query/expansion training examples
    -> LoRA evaluation against held-out structures and real execution traces
```

For example, a test passes on patch P1, the patch changes to P2, and no new test
runs. "P1 passed that test" is supported, while "P2 passed that test" is not.
If the current decision requires validation of P2, the appropriate response is
to request relevant verification or report insufficient evidence. Generate
minimal contrast pairs that change only a version, event order, or execution
state so the model must follow the semantics rather than memorize phrasing.

Training tasks should cover source-to-IR extraction, IR updates after new
events, evidence-grounded queries, and requests for source expansion when the
available information is insufficient. Include conflicting claims, explicit
user corrections, interrupted operations, unknown effects, long-distance
dependencies, and invalid candidate interpretations. Incorrect examples need
explicit rejection/correction targets, not inclusion as accepted facts.

Verification must distinguish its guarantees:

- Deterministic checks can establish valid references, call/result pairing,
  version identity, and transitions allowed by the defined state machine.
- Applicability checks can establish conclusions only within their declared
  artifact scope, dependency rules, and environmental assumptions.
- Natural-language fidelity, inferred causality, and completeness of extracted
  meaning are not generally proved by those checks. LLM judging can assist
  filtering but is not an independent semantic oracle. Even language generated
  from a correct state can introduce unsupported facts or omit key conditions.

Retain generation provenance, rule/schema versions, and filtering outcomes for
accepted examples. Generator/verifier agreement can reflect shared blind spots;
use independent checks and sampled human audits. Split evaluation by task
structure, event template, and project rather than merely random paraphrases,
and keep real execution traces as a separate generalization test. Assess error
accumulation over long sequences, appropriate uncertainty and expansion
requests, and end-task correctness, not only well-formed IR output. Dataset
generation and training are proposed experiments, not authorized live-model
calls or an implemented pipeline.

## Related research

| Work | Relevant mechanism | Difference or limitation for this concept |
| --- | --- | --- |
| [LLMCompiler: An LLM Compiler for Parallel Function Calling](https://arxiv.org/abs/2312.04511), ICML 2024 | Compiler-inspired function-call planning, dependency-aware dispatch, and parallel execution. [Code](https://github.com/SqueezeAILab/LLMCompiler). | Primarily execution orchestration, not a general evidence/version-aware historical context compiler. Borrow explicit dependencies; parallelism alone does not solve repeated verification. |
| [A-MEM: Agentic Memory for LLM Agents](https://arxiv.org/abs/2502.12110), NeurIPS 2025 | Structured memory notes, dynamic links, and evolution of existing memory representations as new information arrives. [Evaluation code](https://github.com/WujiangXu/A-mem); [memory system](https://github.com/WujiangXu/A-mem-sys). | Relevant to structured memory and updates, but memory organization alone does not establish deterministic evidence applicability or safe action replay. |
| [MAGMA: A Multi-Graph based Agentic Memory Architecture for AI Agents](https://arxiv.org/abs/2601.03236), ACL 2026 | Separate semantic, temporal, causal, and entity graph views; policy-guided traversal for query-adaptive context construction. | Closely related to relationship-aware context selection. Reported LoCoMo and LongMemEval results do not establish faster coding-agent convergence or safe version-based validation invalidation. |
| [MemGPT: Towards LLMs as Operating Systems](https://arxiv.org/abs/2310.08560), 2023 | Virtual context management inspired by hierarchical operating-system memory. [Project](https://research.memgpt.ai/). | A useful memory-lifecycle analogy, distinct from compiler-style dependency analysis and evidence validity. |

The paper abstracts and project links were consulted during the discussion.
The table is a relevance comparison, not an exhaustive implementation audit or
an assertion that these projects lack every proposed mechanism. Structured
memory methods may still use vector retrieval; hybrid retrieval is not by
itself a distinguishing contribution.

## Safety boundaries and evidence needed

Original execution records must remain authoritative for dispatched calls,
pending work, uncertain effects, and permissions. A semantic view must not
authorize a tool, fabricate a successful outcome, silently discard uncertainty,
or trigger a blind retry. User/repository/tool text must not be promoted to
trusted host instructions. Selected state, source references, and any generated
summaries need explicit bounds and privacy controls.

A useful initial experiment would maintain this semantic view alongside the
existing transcript without letting it control execution or eviction. Compare
its conclusions against raw traces and version changes before using it to
select context. Typed records and dependency edges are sufficient for such an
experiment; a new storage service is not a prerequisite.

A sensible exploration order is an untrained, side-by-side semantic view first,
then hybrid retrieval and dependency-aware context selection, and finally LoRA
if errors and measurements justify it. Graph/vector storage services should be
introduced for demonstrated needs, not to make the prototype resemble the
complete vision. No stage is an accepted implementation schedule.

Evaluation should compare existing whole-exchange eviction, retrieval-assisted
context, and the proposed structured-state approach on the same tasks and
models. Measure stale-evidence use, constraint loss, repeated failed actions,
unnecessary revalidation, time from a valid candidate to completion, final task
correctness, and total model/tool overhead. Include adversarial or contradictory
inputs, version changes, and uncertain execution outcomes. Improvement is only
supported if reduced repetition does not hide unresolved risks or reduce final
correctness. Any live-model experiment still requires explicit authorization;
this research note does not authorize one.
