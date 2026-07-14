# Retrace — Performance

Retrace is a local-first coding agent, **tuned for faster speeds**: the model spends
its turns doing work instead of repeating itself. A round of profiling and tuning cut
the redundant model calls and dropped end-to-end latency by roughly a third — with no
loss in answer quality.

> A rendered version of this page (graph + table) lives at
> [`index.html`](./index.html) — open it locally or via GitHub Pages.

## Headline

| | after tuning |
|---|---|
| Inference latency | **−32%** |
| Model calls / task | **−28%** |
| Prefix-cache hit rate | **95.6%** |

## What the tuning did

Each agent task runs the model several times. Profiling showed a share of those calls
were redundant — the model would deliver its answer, then get asked to deliver it
again. Tuning removed the repeat. Measured across 5 fresh tasks.

| Metric | Before | After | Change |
|---|--:|--:|--:|
| Redundant completion round-trips (per 5 tasks) | 5 | 1 | −80% |
| Model calls / task | 5.0 | 3.6 | −28% |
| Inference latency (5 tasks) | 50.5 s | 34.4 s | −32% |
| Prefix-cache hit rate | 90.5% | 95.6% | +5.1 pts |
| Answer correctness | 5 / 5 | 5 / 5 | maintained |

## Tuned build — measured on cold cache

| Metric | Per task | Detail |
|---|--:|---|
| Wall-clock time to answer | 10.7 s | end-to-end |
| Cost | $0.0026 | gateway-metered |
| Prefix-cache hit rate | 87.2% | first-touch cold |
| Model calls | 3.3 | avg / task |

## Methodology

- **Setup.** Headless `retrace exec`, 5 fresh single-purpose repos, reasoning off, run
  cache-cold to remove ordering bias.
- **Model.** Qwen-3.6-27B class, served through the Retrace gateway with prefix caching enabled.
- **Before / after.** The delta is a single change — the agent loop no longer re-requests
  an answer the model already produced. Nothing else varied.
- **Honesty.** These are internal benchmark numbers on one model and workload; your
  throughput depends on model, hardware, and task.
