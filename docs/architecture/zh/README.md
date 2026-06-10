# 架构文档（中文版）

This directory holds the **original Chinese-language architecture
documentation** that was the working language during the design
sprint. The English versions one level up
([`docs/architecture/`](../)) are translations of these files —
content is one-to-one, structure is identical.

本目录是设计冲刺期间的中文工作版本。上一级 [`docs/architecture/`](../)
里的英文版本是这些文件的翻译，内容一一对应，结构相同。

## Why both versions exist

The design discussion happened in Chinese at speed; preserving the
original language alongside the English translation:

- documents the actual reasoning context (idiom, emphasis,
  decision-making cadence) that shaped the design;
- gives Mandarin-speaking reviewers the unedited version;
- removes the translation as a single point of failure if the
  English version drifts from the source over time.

## Authoritative version

For day-to-day use, the **English version under
[`docs/architecture/`](../) is canonical** — it's what code
doc-comments, README, and CHANGELOG cross-reference, so prefer it if
the two ever diverge. The Chinese files here are **maintained in sync**
(kept current, not frozen); fixes that change facts should land in both.

## File mapping

Every `NN-name.md` here has a sibling at
[`../NN-name.md`](..) with the same name. Docs 17–18 and 20–21 were
written in English only, so they don't appear in this directory (the
zh Doc 00 index links to the English originals for those).
