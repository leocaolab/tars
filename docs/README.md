# tars documentation

Everything here is written for someone **using** tars. Design rationale,
architecture rewrites, roadmaps and internal audits are not published — they live
in the private working repo, and they change faster than anything you should
build against.

| Path | What's inside |
|------|---------------|
| [`USER-GUIDE.md`](./USER-GUIDE.md) | Start here. Five-minute getting started, the three call shapes, and when *not* to reach for tars. |
| [`providers/`](./providers/) | One page per provider — [anthropic](./providers/anthropic.md), [bedrock](./providers/bedrock.md), [claude-cli](./providers/claude-cli.md), [antigravity](./providers/antigravity.md), [opencode](./providers/opencode.md). Auth, models, and the quirks each one actually has. |
| [`recipes/`](./recipes/) | Task-shaped: [batch](./recipes/batch.md), [cost and reliability](./recipes/cost-and-reliability.md). |
| [`observability.md`](./observability.md) | How to see what a run did, at three grains. |

## Reading order

**Using tars** — `USER-GUIDE.md`, then the page for your provider. Most people
never need to leave those two.

**Something cost more than you expected, or failed in a way you cannot see** —
[`observability.md`](./observability.md).

**Batching, retries, budgets** — [`recipes/`](./recipes/).

## What is authoritative

[CHANGELOG.md](../CHANGELOG.md) is the record of what actually shipped. If a doc
here disagrees with the code, the code is right and the doc is a bug — please
open an issue.
