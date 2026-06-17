# archive/ — deprecated / point-in-time docs

Kept for history but **superseded**. Current method = two-stage "Lever B"; see
[../AGENT_RUNBOOK.md](../AGENT_RUNBOOK.md), [../NONCE_SEARCH_FLOW.md](../NONCE_SEARCH_FLOW.md),
[../gpu-nonce/LEVER_B_EXPLAINED.md](../gpu-nonce/LEVER_B_EXPLAINED.md).

| File | Why archived |
|---|---|
| NONCE_SEARCH_PIPELINE.md | Old pipeline design; conflicts with NONCE_SEARCH_FLOW.md (claims gcd.cu over-strict — reality flipped to under-rejecting, ~82% false-clean). |
| gpu_phase_prefilter_plan.md | Plan for the GPU phase prefilter — built as Lever B stage-2. |
| gpu_phase_M1_spec.md | Spec for the "M1" phase-sim kernel — implemented (verify_dual). |
| gpu-nonce_report.md | Point-in-time GPU-opt log @ frontier 1a5e620 (bench 2026-06-12). |
| gpu-nonce_leverB_notes.md | Lever-B dev notes @ 35ceb01; superseded by LEVER_B_EXPLAINED.md. |
| gpu-nonce_realign_notes.md | gcd.cu realignment diagnosis @ 84d5b0e; historical. |
| hunt_launch.md | Old single-stage hunt + CPU fast-screen verify-daemon launch/health-check; superseded by leverB_hunt.sh. |
| hunt/ | Old single-stage LAN-fleet deploy toolkit (deploy.sh/node_hunt.sh run single-stage gpu-nonce + fast-screen). leverB fully replaced it; fleet = run leverB_hunt.sh per host. |
