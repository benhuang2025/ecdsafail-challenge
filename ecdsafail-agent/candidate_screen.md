# ecdsafail — Candidate Screen

单个候选的 eval screen。由独立 subagent 运行，完成后向主 agent 汇报一行结果并关闭。

> **Subagent 调用**：主 agent 在 Phase 3 为每个候选各启动一个 subagent，并发运行。
> 启动时主 agent 在 prompt 中注明：候选编号、要测试的 env var 改动（可多个）、候选类型（value-exact / island-exact）。
> 完成后汇报标准结果行，然后关闭。

---

## 步骤

### §1 确认 binary 是最新的

```bash
ls -lt /home/ubuntu/ben/temp/ecdsafail-challenge/target/release/eval_circuit | head -1
# 检查 mtime 是否比 src/point_add/mod.rs 更新
ls -lt /home/ubuntu/ben/temp/ecdsafail-challenge/src/point_add/mod.rs | head -1
```

如果 binary 比 mod.rs 旧（Phase 5 没做或跳过），**停止并向主 agent 报告 STALE BINARY，不要继续**。

### §2 运行 eval（env var override，不改文件）

`set_default_env` 是"不存在才设"，所以 shell 环境变量优先级高于代码里的默认值。
直接用 env override 运行预编译 binary，不需要改 mod.rs，多个 subagent 可安全并发。
输出（avg_tof / qubits）是纯确定性的电路数，不受 NUMA / CPU affinity 影响，无需 numactl。

```bash
cd /home/ubuntu/ben/temp/ecdsafail-challenge

env VAR1=val1 VAR2=val2 \
  ./target/release/eval_circuit 2>&1 \
  | grep -E 'ROW:|classical mismatches|FAIL|OK'
```

把 `VAR1=val1 VAR2=val2` 替换为主 agent 指定的实际改动。

**注意**：不需要改 `DIALOG_TAIL_NONCE`（用旧 nonce 跑 FAIL 行也能读到 avg_tof 和 qubits）。

### §3 解析输出

从输出中读取：

```
ROW: <avg_tof>  <qubits>  FAIL    ← 正常（nonce stale，改了 mod.rs）
ROW: <avg_tof>  <qubits>  OK      ← 如果 nonce 碰巧还有效
classical mismatches    : <N_cls>
phase-garbage batches   : <N_phase>
```

计算：
- `score = avg_tof × qubits`
- `Δscore = score - frontier_score`（负数 = 更好）
- `mismatch_count = N_cls + N_phase`（**仅作 DROP 闸，不预测 hunt 时间**）

### §4 汇报结果

向主 agent 返回标准结果行：

```
候选<N>: avg_tof=<X>  qubits=<Y>  score=<X×Y>  Δ=<±W>  mismatches=<N_cls>+<N_phase>=<sum>  类型=<value-exact|island-exact>
```

附加判断（本步只判**分数** + 粗筛极端不可行；真正的可行性判定在 Phase 5.5）：
- `Δ ≥ 0` → **DROP（分数没改善）**
- `Δ < 0` 且 `mismatch_count ≤ 35` → **KEEP**（进 Phase 4；可行性留待 Phase 5.5 实测）
- `mismatch_count > 35` → **DROP（mismatch 太多，基本不可猎）**
- ⚠️ `mismatch_count` **只能用来 DROP，不能预测 hunt 时间**——它是单岛的均值采样，而 hunt 要的是分布尾巴。score-negative + ≤35 **≠ 可猎**；是否真能 hunt 由 Phase 5.5 判定：Step-0 实测 λ_cand ≈ baseline、near-miss `max failbatch/141`（差 141 还几十=别投，≥138=接近）、逐通道-零（cls/pha/anc 各自到 0）、失败-shot 重叠测试（结构地板 vs 深尾）。

如果 binary STALE：汇报 `候选<N>: STALE BINARY，需先重跑 Phase 5`。

---

## 快速参考

`N_cls + N_phase` 仅作 **DROP 闸**，不预测时间：

| N_cls + N_phase | 本步动作 |
|---|---|
| ≤ 35 | 不据此估时间；score-negative 则 KEEP，可行性交 Phase 5.5 |
| > 35 | DROP（mismatch 太多，基本不可猎） |

真正的「要不要 / 要多久 hunt」由 Phase 5.5 判定（Step-0 λ_cand + near-miss `max failbatch/141` + 逐通道-零 + 失败-shot 重叠测试），见 [AGENT_RUNBOOK.md](./AGENT_RUNBOOK.md) Phase 5.5 与 [nonce_time_estimation.md](./nonce_time_estimation.md)。

Frontier 分数从主 agent 在 Phase 0 记录的值获取。
