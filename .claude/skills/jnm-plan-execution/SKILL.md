---
name: jnm-plan-execution
description: Execute JnmBrowser plan documents (.zcode/plans or root plan books) — read order, checkbox tracking, mainline discipline, and the exact completion rule. Use whenever the user hands over a plan or asks to continue one.
---

# Plan-driven execution

This project runs on written plans that the user may update mid-flight and that an AI monitor program supervises. Discipline matters more than speed.

## Read order (every session, before doing work)

1. **Master plan book** (总主线计划书) if one exists/referenced — root-level Chinese-titled `.md` files (e.g. `MCP_*.md`, `Wayfern*.md`, `Chromium*Camoufox*.md`) or the file the user names as the master plan.
2. **Current stage plan** in `.zcode/plans/` (check `.zcode/plans/.active` / `.zcode/specs/.active` / `.zcode/artifacts/.active` pointers when present).
3. Re-read the plan file **whenever resuming** — the user edits plans manually; detect changes, refresh memory, and adjust to the newest requirements instead of executing a stale copy.

## Executing

- Follow the plan **step by step, strictly**; do not skip ahead or gold-plate ("在一个功能上不要过度优化" — don't over-optimize a single feature).
- If the plan has a task checklist, **tick items off in the plan file as you complete them** and keep the check state current.
- Hit a problem? Fix it, then **return to the mainline immediately** (修复后及时回到本计划主线，不要偏离). If a plan change is unavoidable, propose the modification, get alignment, then continue on the adjusted mainline.
- When the monitor replies `好的，在不偏离'XXX计划书'的前提开始继续执行`, continue strictly per that plan.

## Completion rule (exact)

Output the victory phrase **if and only if** the ENTIRE plan — every item — is executed and the stage is truly closed:

```
plan is ok, 我们这阶段胜利了，请金木查看
```

**Never** output it for: a single step, partial edits, mid-stage states, or ordinary turn replies. The phrase signals the supervising program to stop — sending it early abandons the plan.

## Supporting docs

| Area | Doc |
|------|-----|
| Active ZCode plans | `.zcode/plans/` |
| Auto-registration | `docs/auto-registration.md` |
| Self-host sync | `docs/self-hosting-donut-sync.md` |
| Session state/handoffs | `.zcode/state/`, `.zcode/memory/handoffs/` |

Before finishing a stage, run the `jnm-quality-gate` so the plan closes on green checks.
