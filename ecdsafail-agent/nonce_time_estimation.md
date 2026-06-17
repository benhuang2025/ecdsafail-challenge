---
name: ecdsafail-nonce-time
description: >-
  How to correctly estimate the expected time to find a clean nonce for the
  ecdsafail circuit. Covers: empirical winner probability, GPU hunt rate,
  verify throughput, bottleneck analysis, and common mistakes. Use this
  whenever asked "how long will the hunt take?" or "how many candidates needed?"
---

# ecdsafail — Nonce Hunt Time Estimation

> **Subagent 调用**：本文档由独立 subagent 单独运行。完成评估后将结果（预期候选数、预期时间）汇报给主 agent，然后关闭。

---

## ⚠️ 读前必读（否则白等几小时）

**1. displacement / λ ≠ P_winner。** 不要用 stale-nonce 的 `N_cls+N_phase`（下面「理论预测」那张表）或 λ 去**判定可行性或估时间**。那只是粗 proxy，而且**只在 GPU filter 真的在 enrich 时才有意义**。唯一可信的 `E[time]` 来自**当前 op-stream 上实测的经验 `P(cls=0)`**（Step 1），不是 λ。

**2. 「跑了测量」≠「有了估算」。** 如果 verify log 里 `cls=0` 个数 = 0，你只有**下界**，没有点估计。这时**禁止**下「可行，几小时」之类结论——要么继续累积到出现 `cls=0`，要么按下界老实说「可能不可行」。

**3. `λ_residual ≈ 8.5` 不是常数。** 它是历史上某些电路、filter 对齐良好时的值。任何电路结构变化（尤其 `compressed.rs` 或任何重写）都可能让 filter 失准。**绝不可套用 8.5**，必须用 Step 0 在当前电路上重新实测。

> 2026-06-13 实例：用 displacement(25.6, phase 8.6) 断言「at-floor、几小时可行」，但 Step 0 实测 GPU-clean 候选 full-count classical ≈ **19.57**（和随机 nonce 一样）→ filter 根本没 enrich → `e^(−8.5)` 失效 → `E[time]` 实际不可知/可能不可行。displacement 是错的 proxy，白等了时间。

---

## 基本公式

```
E[找到winner需要的时间] = E[需要验证的候选数] / min(候选生成速度, 验证速度)
                        = (1 / P_winner) / min(R_cand, R_verify)
```

三个独立变量，分别从数据里测量。**但先做 Step 0。**

---

## ⚠️ Step 0（必做，最先做）：验证 GPU filter 真的在 enrich

`P(winner) ≈ e^(−λ_residual) × …` 这套理论**只在 GPU filter 把候选的 classical 显著压到低于随机时才成立**。validate_pipeline 通过（gpu mean ≤ cpu mean）只保证 filter **conservative（不漏 winner）**，**不保证它 selective（会 enrich）**——两者完全不同。filter 可以「安全但没用」（让几乎所有 nonce 都过）。

开跑、verify 出真实 RESULT 后，立刻做这个对比（用 `NS_EARLY_EXIT=0` 数**真实** classical 计数，不是早退的 0/1 flag）：

```bash
# (a) ~40 个 GPU-clean 候选的 full classical
grep -h CLEAN_CANDIDATE /tmp/hunt_z3_g*.log | sed 's/.*nonce=//' | sort -un | head -40 \
  | while read n; do DIALOG_TAIL_NONCE=$n NS_EARLY_EXIT=0 NS_SHOTS=9024 fast-screen | grep RESULT; done
#   → mean classical = λ_candidate

# (b) ~10 个随机 nonce 的 full classical（同 NS_EARLY_EXIT=0）
#   → mean classical = λ_random
```

| 对比 | 结论 |
|---|---|
| `λ_candidate << λ_random`（例 8.5 vs 19） | filter 在 enrich ✓，理论模型可用，继续 Step 1 |
| `λ_candidate ≈ λ_random` | **filter 没 enrich** ✗ → P(cls=0)≈随机 → 多半不可行 → **先 realign GPU filter，别开长 hunt** |

只有 λ_candidate 明显低于 λ_random，下面的 `e^(−λ_residual)` 理论才有意义；此时 `λ_residual = λ_candidate`（实测值，不是 8.5）。

---

## Step 1：测 P_winner（每个候选是 winner 的概率）

**正确方法：用实际验证日志的经验数据。**

```bash
# 从 verify log 收集所有结果
grep "RESULT qubits" /tmp/verify.log > /tmp/all.txt

N=$(wc -l < /tmp/all.txt)
CLS0=$(grep -c "classical=0 " /tmp/all.txt)
PH0=$(grep -c " phase=0 " /tmp/all.txt)
BOTH=$(grep -c "classical=0 phase=0 " /tmp/all.txt)

echo "N=$N  cls=0:$CLS0  ph=0:$PH0  both=0(winner):$BOTH"
echo "P(winner) = $BOTH / $N"
```

> 注意：默认 hunt 用 `NS_EARLY_EXIT=1`，所以 verify log 里 `classical` 是 0/1 的 **flag**（0=clean，1=至少一处 mismatch 提前退出），不是计数。要数真实 λ 必须 `NS_EARLY_EXIT=0`（见 Step 0）。

**如果 CLS0=0（还没观测到任何 classical-clean）：你没有 P(cls=0)，只有上界 `< 1/N`。** 不要假装有点估计。按下面「快速可行性判断」表给结论（0/几百~几千 通常= 不可行），并继续累积 verify 直到出现 ≥1 个 cls=0 再算。**不能用 `e^(−λ)` 代替**（mismatch 相关、非独立，见下）。

**如果 CLS0≥1**，用条件概率模型：

```
P(winner) = P(cls=0) × P(ph=0 | cls=0)
```

- P(cls=0) = CLS0 / N （从数据直接读，经验值）
- P(ph=0 | cls=0)：从 cls≤3 的条目里看 phase 分布，外推到 cls=0 时 phase 的期望 μ，P(ph=0|cls=0) ≈ e^(-μ)

示例（2026-06-11）：
```
N=6932  cls=0:1  ph=0:3  both=0:0
P(cls=0)=1/6932; cls=0 entry phase=2 → P(ph=0|cls=0)≈e^(-2)≈0.135
P(winner) ≈ 1/51,000 → E[candidates] ≈ 51,000
```

### ⚠️ 常见错误：独立相乘
`P(winner)=P(cls=0)×P(ph=0)` 是**错的**——cls/ph 正相关，必须用条件 `P(ph=0|cls=0)`。

### ⚠️ 常见错误：用 e^(−λ) 当 P(cls=0)
classical mismatch 跨 9024 shots 是**相关的**（由 island 结构驱动），不是独立 Poisson。所以 `P(cls=0)` **不等于** `e^(−λ_cls)`。`e^(−λ)` 只在「理论预测」里当**极粗的 sanity 量级**用，且前提是 Step 0 已确认 filter enrich。真正的 P(cls=0) 只能经验测（CLS0/N）。

### ⚠️ 常见错误：用旧电路的估计值
每次改 `src/point_add/`（knob/bundle/上游 rebase 到新 frontier）都改变 λ 和 filter 对齐，旧 P_winner 完全失效。**必须用当前电路重测（含 Step 0）。**

---

## Step 2：测候选生成速度 R_cand

```bash
tail -1 /tmp/hunt_z3_g1.log
# 格式: progress ~X/Y  (Z nonce/s total)  candidates=C
# 候选率 = candidates / nonces_scanned （per million）
# R_cand = sum(各GPU nonce/s) × 候选率
```
候选率（candidates/M）反映 filter 松紧，改电路要重测。

---

## Step 3：测验证速度 R_verify

```bash
# 单次 fast-screen ≈ 5-15s（电路重建为主，与 NS_SHOTS 无关）
# -P32 并行 → 看 verify.log 每分钟增量
wc -l /tmp/verify.log; sleep 60; wc -l /tmp/verify.log
```
> 先确认 verify **真的在跑**：`grep -c RESULT /tmp/verify.log` 应 ≈ 行数，且无 `nonce | ` 空行（空行=verify 空跑，见 archive/hunt_launch §3e；两阶段下 verify 日志为 `/tmp/lb_s2_g*.log`）。

---

## Step 4：计算预期时间

```
E[candidates_needed] = 1 / P_winner   （需 Step 1 拿到经验 P_winner；CLS0=0 时无法算，只能给下界）
R_bottleneck = min(R_cand, R_verify)
E[time_to_win] = E[candidates_needed] / R_bottleneck
```
backlog 检查：若 R_cand>R_verify，加上 `backlog_at_win / R_verify`。

---

## 理论预测（仅作开跑前粗略 sanity，不能当结论）

> ⚠️ 这一节只在 **Step 0 已确认 filter enrich** 后才有量级意义。否则 λ_residual ≠ 8.5，整节作废。

stale nonce ≈ 随机 nonce，其 `N_cls + N_phase` 是 λ_raw 的粗估：

| N_cls + N_phase | 粗略难度 | 建议（仍需 Step 0/1 确认） |
|---|---|---|
| < 15 | 容易 | 但仍要实测 |
| 15–25 | 也许可行 | 必须 Step 0 确认 enrich |
| 25–35 | 边界 | 多半要单 knob |
| > 35 | 基本不可行 | 换方案 |

理论模型（**条件成立**：Step 0 确认 λ_candidate≈λ_residual 且 << λ_random）：
`P(winner per candidate) ≈ e^(−λ_residual_cls) × e^(−N_phase)`。历史上 filter 对齐良好时 λ_residual≈8-9，**但这不是常数**——新电路实测（Step 0）为准。

---

## 快速可行性判断（有少量验证数据时）

| 观测 | 结论 |
|------|------|
| cls=0 ≥1 次 且 ph=0 ≥1 次 | 可行，用条件模型估时间 |
| cls=0 出现 0 次（数百~数千样本） | P(cls=0) 太低 → 多半不可行 → 查 Step 0 是否 filter 没 enrich |
| Step 0 显示 λ_candidate ≈ λ_random | filter 没 enrich → realign filter 或换方案，别硬跑 |
| cls=0 AND ph=0 直接出现 | 很幸运，P(winner) 高 |

---

## ⚠️ 不要用来测分布的错误方式

**错误**：取已有 GPU-filtered 候选，在**另一个**电路配置下跑 fast-screen 比较分布。GPU filter 是针对特定电路的，跨电路无意义。**正确**：每个配置各自 GPU hunt、收集该配置的候选再统计。

---

# 进阶决策质量（借鉴友队 `ecdsafail_skills/ecdsafail-island-hunting`，2026-06-16 并入）

上面的 Step 0–4 回答"要等多久"。下面三件回答更前置的"**这条路到底该不该 hunt**"——在烧 GPU 之前就能砍掉死路。

## A. 三通道 per-channel-zero 硬判据（最便宜的 go/no-go，先做）

一个 winner 要 **cls=0 且 phase=0 且 anc=0**（三通道全清）。在跑任何时间估算前，先问：在这批已验证候选里，`cls`、`pha`、`anc` **每一个通道是否各自在某个候选上独立到过 0**？

- 某通道在几百个候选里**从未**到 0 → 它有**结构性 floor**，`0/0/0` 无论换什么 nonce 都不可达 → **直接弃路**，不用再算时间。
- 三通道各自都到过 0（哪怕从不同时到）→ 通过这道筛，再进下面的 overlap test / 时间估算。

> 我现有 Step 1 只盯 cls 和 ph；**anc 通道也要单独确认到 0**。anc≠0 通常是漏了 uncompute（结构 bug），不是 island 能救的。

## B. Failing-shot overlap test（区分"结构 floor"还是"统计长尾"——最关键的单一判断）

**症状**：某通道（通常 cls）在很多候选上**卡在一个小 floor K**，从不更低（例如几百个候选里 best 始终是 `2/*/0`，没见过 cls≤1）。

这时唯一要回答的：K 是**结构 floor**（一组固定 shot 对**每个** nonce 都失败 → `0/0/0` 不可达 → 死路）还是**深统计长尾**（cls=0 以 ~e^−λ 可达，只是慢）？直方图形状（pile-up vs 平滑下降）只是弱信号，几百候选时不可信。**决定性测试**：

```bash
# 1) 给 eval_circuit 打个本地补丁，dump 全部失败 shot（不是只第一个）；env-gated，ecdsafail sync 会重置
#    在 classical-mismatch 分支： if std::env::var("DUMP_FAIL_SHOTS").is_ok() { eprintln!("FAILSHOT {i}"); }
# 2) 对 3–5 个最低-cls 候选，各自 build + DUMP_FAIL_SHOTS=1 eval，收集失败 shot 索引集合
# 3) 比较各集合的重叠
```

| 结果 | 判定 | 行动 |
|---|---|---|
| **各候选失败 shot 高度重叠**（同一批 shot 反复） | **结构 floor**，存在 nonce 无关的固定坏集 | **弃路**，再多 nonce 也没用 |
| **各候选失败 shot 几乎不重叠** | **统计长尾**，存在全清 nonce | 继续 hunt（只是深） |

> 友队实测：死的 1210q 路 `SEG182+FOLD17+DIALOGFOLD17` 卡 cls=2、明显 pile-up（392 候选里 11 个在 cls=2、无更低）→ 结构 floor。而 `slope1016+compare43` 同样 best cls=2，但 4 个候选的失败 shot 是 `{2879,6741}/{2689,3336}/{589,7961}/{1062,8584}`——**零重叠**→统计长尾，0/0/0 可达只是深。

⚠️ **bash 不要用 zsh 跑**：多变量 `CFG="A=1 B=2"` 只在 bash 下能 word-split 成多个 `env` 赋值；zsh 下 `env $CFG` 会当成一个非法变量名、**静默 build base 配置**，于是你所有 island nonce 全 dirty——一个假的 all-dirty 警报。

## C. "统计长尾"也可能是**实际**死路（深度杀人）

overlap test 判成统计长尾 ≠ 在你时间预算内可落地。估 `e^λ` 需要的候选数，和 fleet 实际产能比：
- 友队实测 `slope1015+compare43`(1203q)：346 候选、cls floor 卡 3、零重叠（确属统计长尾），但 `e^9≈8000` 候选才出一个 cls=0，960M nonce 实跑出 **0** 个 island。overlap 说"非结构"，但**深度**把它判死。
- 规则：`e^λ` 超过候选预算就弃路，别白跑。

## D. apply-aware filter 校准决定 cls floor（Step 0 的延伸）

GPU 预过滤是 **apply-aware** 的，它建模了 apply 侧比较器（如 `compare_bits=46`）。**改了比较器的 lever（如 compare 46→43）会静默让 filter 失准**——GPU-`CLEAN` 变成"在错误比较器下 clean"，候选验证出来卡高 cls floor（前面那个 compare43 mean≈8–10 就是这个）。

诊断价值：当 GPU-CLEAN 候选验证后离 0/0/0 很远，**先怀疑 filter/电路比较器不匹配**，再怪 nonce 运气。lever 对 filter 的代价排序：**pre-GCD 改动 0 代价 > value-exact apply 微调 一点 > 改比较器 代价最大**。选 lever 时优先保持 filter 诚实。

## E. 量化 runtime 模型的三处校正（精化 Step 4）

我的 Step 4 用 `1/P_winner / R_bottleneck`。三处让绝对值更可信：

1. **条件漏斗，不是独立连乘**（我已有 cls×ph，补 anc 与密度分层）：
   ```
   island_rate = d · P(c=0) · P(p=0|c=0) · P(a=0|c=0,p=0)
   d = e^(−λ_GCD)        # GPU 预过滤通过率，从【全 nonce】GCD-hard 均值估（百万样本，紧）
   P(c=0)               # GCD-clean 候选里残余 cls 清零率（条件在 d 之上）
   P(p=0|c=0)           # 直接测：c-clean 候选里同时 p-clean 的比例（比建模 pha 分布稳）
   P(a=0|…) ≈ 1         # anc 通常确定性为 0
   ```
   关键：**每个率都从【均值】估，不是数稀有的 0**。`d` 用全 nonce 的 GCD-hard 均值 `λ_GCD`（样本极多），不要靠等 island 出现来估 1e-6 量级的概率。
2. **计数用 Poisson / Negative-Binomial，不要 Gaussian**。cls/pha/anc 是 9024 里几个失败 shot 的非负计数，均值 μ≈2–6。离散度 `σ²/μ≈1` 用 Poisson `P(0)=e^−μ`；`σ²/μ>~1.2`（GCD-clean 是被筛过的异质子群，**通常过离散**）用 NB `P(0)=(1+μ/r)^−r, r=μ²/(σ²−μ)`。候选 <50–100 个时方差不可信，退回 Poisson 用均值。
3. **校准常数 k + 报百分位**。没有闭式能准到一个数量级以内（shot 间相关破坏独立假设）。把闭式当**相对排序器 + ETA 先验**，外面套经验校正：
   ```
   E[N] ≈ k · (1 / island_rate),  k = 历次落地 hunt 的 median(actual_N / predicted_N)
   ```
   每落地一个 island 就记 (predicted, actual) 更新 k（实测 k 跨配置散布 0.3×–73×）。等待是指数分布、**重尾**：百分位 P 需 `ln(1/(1−P))/p` 个 nonce，所以 **p99 ≈ 4.6× 均值**。**永远报 p50/p90/p99，不要只报均值**——"预期 4h"经常意味着"1/100 概率 >18h"。

> 友队有现成工具 `scripts/island_runtime_model.py`（density × 条件漏斗 q × 吞吐 R，自动判 screen-bound vs eval-bound）。screen-bound（q≈1，低密度）多加 scan GPU 线性变快；**eval-bound（独立 pha/anc 模式，q 小）加 scan GPU 没用**，得加速验证器或教会 filter 识别 pha/anc 触发。我的 fused GPU validator 是单阶段，但 Lever-B 两阶段管线（gcd 预过滤→verify_dual）正适用这套分层。
