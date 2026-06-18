# ecdsafail — Agent Optimization Runbook

完整的一次优化迭代流程。每次开新session从第0步开始；如果已经同步且有候选，从第2步开始。

## ⭐ 核心理念（决定整个 ideation 方向）

**只找「sub-cliff 且 nonce search 快」的优化。提升再小都没关系 —— 只要 beat 当前 frontier 就提交。**

为什么是这个方向（见 [NONCE_SEARCH_FLOW.md](./NONCE_SEARCH_FLOW.md)）：
- **瓶颈是 verify 吞吐，不是 idea 大小。** hunt 时间 = (赢家 nonce × candidate率) ÷ verify吞吐。一个 −10M 的大优化如果 over-cliff（λ_cand 远高于 baseline），赢家 nonce 大到几天~几年都猎不到 → 等于 0 分。一个 −200k 的小优化如果 sub-cliff（λ_cand ≈ baseline），几十分钟就出小 nonce → 真能提交。
- **「小而可猎」永远打败「大而不可猎」。** frontier 上那些小 nonce 不是猎得巧，是幸存者偏差：维护者只提交 sub-cliff 的；over-cliff 的当场放弃，从不 commit。
- **两类候选才值得做：**
  1. **value-exact**（Lever B/C/F/G/H：measured-uncompute、exact-adder、measured-fast 非peak adder、NAF、phase-conditioned replay）—— 零新 hard input → λ_cand = baseline → 最快猎。提升通常小，但稳。
  2. **filter-enriched 的 GCD-轴 sub-cliff 截断**（COMPARE_BITS / WIDTH_SLOPE / BINDER_NOTCH / BODY_TRIM）—— filter 能 enrich 这条轴 → 实测 λ_cand 若 ≈ baseline 就可猎。
- **明确避开：** 任何让 λ_cand 明显高于 baseline 的截断（单次砍一整个 GCD iteration、square cleanup 砍太狠、WIDTH_MARGIN 砍太多）。这些的 phase 轴 filter 不 enrich → SQWIN-class 慢猎或 over-cliff 不可行。**screen 出 score-negative 不够，必须 Step-0 实测 λ_cand 确认 sub-cliff 才能 hunt。**

**基本信息**
- 控制节点是 vast.ai RTX5090 GPU server，连接方法：
  ```bash
  ssh-add ~/.ssh/id_rsa
  ssh -p 63190 root@74.48.78.46 -L 8080:localhost:8080 -A
  ```
  连上后 `cd /home/ubuntu/ben/temp/ecdsafail-challenge && export PATH=$HOME/.cargo/bin:$PATH`
- 编辑权限：只能改 `src/point_add/`，禁止改 main.rs / circuit.rs / sim.rs / weierstrass / bin/* / Cargo* / rust-toolchain
- **永远不要 `git push`**
- agent 工具都在 `ecdsafail-agent/`（untracked）。**hunt 部署用两阶段 `ecdsafail-agent/gpu-nonce/leverB_hunt.sh`**（见 Phase 6；旧单阶段 `hunt/` 已归档到 `archive/`）。
- **单机 vast.ai RTX5090 box（hunt 全在这台上跑）：**
  - 多卡 RTX5090，跑两阶段 `leverB_hunt.sh`（`NGPU`/`GPULIST` 选卡，`numactl` 绑 NUMA）。
  - 现在的 hunt 是 GPU-bound 的 CUDA kernel（stage-1 ~8.6k nonce/s/GPU）+ `setsid &` 逐卡后台，**不依赖 `xargs -P`** → 旧 runbook 里「vast.ai 容器 xargs -P 串行 / 消费级 GPU scan 极慢」的坑只针对已归档的旧单阶段 pipeline（host-bound ~4800 nonce/s + 容器 xargs 串行），现在不适用。
  - 容器注意：pytorch 模板可能缺 `numactl` / CUDA toolchain，hunt 前先确认。

---

## Phase 0 — 同步到最新frontier

```bash
git fetch origin
git log origin/main --oneline -5          # 看最新几个commits
git diff HEAD origin/main -- src/point_add/mod.rs | grep set_default_env  # 看有无新knob
```

如果frontier有更新就同步（`ecdsafail-agent/` 是 untracked，pull/checkout 不会动它，安全）：
```bash
git checkout -- src/point_add/mod.rs   # 先丢掉上轮的候选改动
git pull origin main                   # 或 git fetch + git reset --hard origin/main
```
> ⚠️ frontier 一旦推进（尤其重写 fold/apply/compressed 这类结构改动），上轮在旧 base 上的候选**作废**，且 `gcd.cu` filter 会 STALE（见 Phase 5 enrichment 自检）。**sync 后必须重新 Phase 1→2→5 走一遍**，不能拿旧候选直接 hunt。

确认baseline eval通过（0/0/0）：
```bash
numactl --cpunodebind=0 --membind=0 cargo run --release --quiet --bin eval_circuit 2>&1 | tail -15
```

如果FAIL：frontier的nonce可能因本地有修改而过期 — 先 `git checkout -- src/point_add/mod.rs` 还原，再重跑。

记录当前frontier分数：`score = avg_toffoli × qubits`，这是下一步要打败的目标。

---

## Phase 1 — 了解当前peak binder

```bash
TRACE_PEAK=1 numactl --cpunodebind=0 --membind=0 \
  cargo run --release --quiet --bin build_circuit 2>&1 \
  | grep -E 'peak_qubits=|near_peak' | head -20
```

关键问题：
- `peak_qubits=<N>` — 当前peak
- 有几个 `active=<N>`（同为peak值）的phase？（1个 = 可直接tighten；多个co-binder = 必须全部co-descend才能降peak）
- 瓶颈判断：peak每降1q ≈ −avg_toffoli分（~1,400,000）；Toffoli每降1T ≈ −peak分（~1,215）。break-even ≈ 1,150 T/q

---

## Phase 2 — 从skill库找候选优化

**优先级（按可猎性，不是按提升大小）：**
1. **先找 value-exact**（[toffoli_reduction_skill.md](./toffoli_reduction_skill.md) Lever B/C/F/G/H）—— 零新 hard input，λ_cand=baseline，最快猎。哪怕只 −100k 也优先做。
2. **再找 filter-enriched 的 GCD-轴 gentle 截断**（COMPARE_BITS、WIDTH_SLOPE、BINDER_NOTCH、BODY_TRIM）—— 这些 filter 能 enrich，gentle 一步通常 sub-cliff。
3. **不碰** square-cleanup / 整 iteration / WIDTH_MARGIN 这类砍狠了的 —— phase 轴 un-enriched，大概率 over-cliff。
> 成熟 frontier 上单旋钮 sub-cliff win 越来越少（旧的都被 bake 进去了）。frontier 每次推进（尤其重写某个 phase）会重开口子 → **sync 后第一件事就是 re-screen 这些旋钮**。

具体旋钮的当前值/可降空间要从 `git diff cf310ec..origin/main -- src/point_add/mod.rs` 和最新 frontier 的 set_var 块里重新读（frontier 每动一次数字就变，别照搬下面的旧例子）。

根据Phase 1的结论选方向：

**如果peak有空间**（详见 [peak_reduction_skill.md](./peak_reduction_skill.md)）：
- Lever C：`SQUARE_ROW_MAX_SEG` 还能不能再降（当前186，试184）
- Lever C：`DIALOG_GCD_APPLY_CHUNKED_F_BLOCKS` 当前16，试18/20（co-descend apply binder）
- Lever A：找新的idle ancilla可以free+recompute
- Lever G：找还有没有comparator/cadd/csub路径可以做borrowed-carry

**如果Toffoli有空间**（详见 [toffoli_reduction_skill.md](./toffoli_reduction_skill.md)）：
- Lever A：`DIALOG_GCD_COMPARE_BITS` 当前46，试45；`DIALOG_GCD_APPLY_CLEAN_COMPARE_BITS` 当前19，试18
- Lever D：`DIALOG_GCD_FOLD_CARRY_TRUNC_W` 当前18，试17
- Lever H：找还有没有coherent comparator可以做phase-conditioned replay
- GCD knobs：`DIALOG_GCD_BINDER_NOTCH_MAP` / `BODY_CARRY_BAND_TRIMS` 能否多加一步

**每个候选**生成一个假设：
```
候选N: 改 <ENV_VAR> <old>→<new>
预期: Toffoli ±X（从frontier commit diff估算），peak ±Y
类型: value-exact(不加hard inputs) | island-exact(加λ，需re-hunt)
```

---

## Phase 3 — 快速screen候选（FAIL行也有价值）

为 Phase 2 生成的每个候选各启动一个 subagent，**并发运行**，全部跑完后汇总结果。

详细步骤见 **[candidate_screen.md](./candidate_screen.md)**。

启动方式：每个 subagent 的 prompt 中注明：
```
候选编号：N
env var 改动：VAR1=val1, VAR2=val2（可多个）
候选类型：value-exact 或 island-exact
frontier_score：<Phase 0 记录的值>
```

每个 subagent 独立用 env override 运行预编译 binary（不改 mod.rs），安全并发。
subagent 完成后汇报标准结果行，关闭。

汇总所有结果后：
- 只保留 `Δ < 0`（score 改善）且 `N_cls + N_phase ≤ 35` 的候选进 Phase 4
- 其余 DROP

---

## Phase 4 — 选最优候选 + bundle决策

1. **单knob还是bundle**：如果多个候选各自beat frontier，且 `N_cls + N_phase`（各候选单独测时）都≤25，考虑bundle（叠加所有改动，一次hunt）。Bundle 后合计 displacement ≈ 各候选之和，只要≤35仍可hunt（参考 Phase 5.5 表格）。

2. **island-exact vs value-exact**：value-exact候选（Lever B/C/F/H）不加λ，可以和island-exact候选一起bundle而不需额外代价。

3. **最终选定**：在 `src/point_add/mod.rs` 里写好所有选定的 `set_default_env` 改动（`DIALOG_TAIL_NONCE` 暂时保持旧值，等hunt出winner再改）。

确认改动：
```bash
git diff -- src/point_add/mod.rs | grep '^[+-].*set_default_env'
```

---

## Phase 5 — 重建工具 + 验证pipeline

**每次改了 `src/point_add/` 之后必须做，否则fast-screen/gpu-nonce跑的是旧路线**：

```bash
REPO=/home/ubuntu/ben/temp/ecdsafail-challenge

# 重建 fast-screen
(cd $REPO/ecdsafail-agent/fast-screen && cargo clean -p quantum_ecc && cargo build --release)

# 重建 gpu-nonce
(cd $REPO/ecdsafail-agent/gpu-nonce && cargo clean -p quantum_ecc && cargo build --release)

# 验证GPU filter对齐（必须 gpu <= cpu，即GPU mean ≤ CPU mean）
(cd $REPO/ecdsafail-agent/gpu-nonce && \
  CUDA_VISIBLE_DEVICES=1 numactl --cpunodebind=0 --membind=0 \
  cargo run --release --bin validate_pipeline 2>&1 | tail -5)
```

验收标准：
- `GPU mean hard/nonce ≤ CPU mean` → filter是conservative方向，安全（不会漏winner）
- 如果`GPU mean > CPU mean`：filter偏离，必须先realign `dialog_gcd_classical_filter.rs` 再hunt

**⚠️ 但 gpu≤cpu 只证明 filter「安全(不漏 winner)」,不证明它「还在 enrich(够选择性)」—— 两者不同。** 每次 sync 到新 frontier 后,structural 截断改动(frontier 常含:重写 fold/apply、新约简/截断路径)会让 filter STALE:validate 仍过,但筛出的候选 ≈ 随机 → hunt 退化成暴力搜索,无症状、白跑几小时~几天。所以 Phase 5 必须再加一道 **enrichment 自检**(方法见 [nonce_time_estimation.md](./nonce_time_estimation.md) Step 0):量 ~40 个 GPU-clean 候选的真实 classical 均值(`fast-screen NS_EARLY_EXIT=0`)vs ~10 个随机 nonce。若候选均值 ≈ 随机 → filter 没 enrich → **先 realign `gcd.cu` + `dialog_gcd_classical_filter.rs` 建模新截断,再 hunt**(用当前 frontier 自己 baked 的 `DIALOG_TAIL_NONCE` 当回归测试 —— `ecdsafail run` 确认它 0/0/0,正确的 filter 必须对它判 hard=0)。GPU kernel(EC/field/keccak 吞吐)解耦,电路变化时不用动。

---

## Phase 5.5 — Hunt 可行性评估（开跑前必做）

详细方法见 [nonce_time_estimation.md](./nonce_time_estimation.md)。

快速核查：

```bash
# 用 stale nonce 估算 λ（改完 mod.rs 后，不改 DIALOG_TAIL_NONCE 直接跑）
numactl --cpunodebind=0 --membind=0 cargo run --release --bin eval_circuit 2>&1 \
  | grep -E "classical|phase-garbage"
# classical mismatches: N_cls
# phase-garbage batches: N_phase
```

| N_cls + N_phase | 建议 |
|---|---|
| < 15 | 直接跑，< 1 小时 |
| 15–25 | 可行，1–8 小时 |
| 25–35 | 边界，考虑单 knob 代替 bundle |
| > 35 | 基本不可行，换方案 |

> N_phase 每增加 1，P(winner) 下降约 63%（主要可控变量）。

### ⚠️ stale-nonce displacement 只是粗筛 —— 真正的 gate 是 Step-0 实测 λ_cand

displacement 是「一个随机岛」的单点采样，而且**对 GCD-轴截断会高估难度**：filter 会 enrich GCD/classical 轴、把那部分 hard input 在 hunt 里筛掉，但 **phase 轴 filter 不 enrich**。所以 **screen 出 score-negative + displacement 不爆 ≠ 可猎**。开跑前**必须**跑 Step-0（[nonce_time_estimation.md](./nonce_time_estimation.md) Step 0）：

```bash
# rebuild gpu-nonce + fast-screen（改了 src/point_add 必做）→ validate_pipeline → scan 300k →
# 取 ~40 个 GPU-clean 候选，NS_EARLY_EXIT=0 跑全 count，量 mean_cls / mean_phase
```

判据（和当前 frontier 自己的 baseline λ_cand 比）：
- `mean_cls ≈ baseline 且 mean_phase` 不明显高于 baseline → **sub-cliff，可猎，部署**。
- `mean_cls` 或 `mean_phase` 明显高出 baseline（例：baseline~14，候选~20）→ **over-cliff，丢弃**，回 Phase 2 换更 gentle 的或换 value-exact。

> **⭐ 决定性闸门：mean_cls/mean_phase 仍只是均值，预测不了 P(winner) 的尾巴。开跑前必须再看 near-miss 分布 —— 扫 ~10M nonce，量 classical-clean 候选的 `max failbatch / 141`。差 141 还有几十（如 98/141）= phase 太硬、别投；有候选到 ≥138 = 接近、可投。这是唯一能在事前区分「30min」和「几十小时」的指标。** verify_dual 已带 `PHASE NEAR-MISS: max failbatch=.../141` 输出。

> 没有「几分钟」级的 hunt —— 即便 sub-cliff，下限也是 ~几十分钟到几小时（取决于 fleet verify 吞吐）。要更快只能加 verify 核数（fleet 越大越快），不是找「更便宜的 idea」。

## Phase 6 — 部署 hunt（两阶段 Lever B，按机手动跑）

**当前方法 = 两阶段 `ecdsafail-agent/gpu-nonce/leverB_hunt.sh`**（单机 8 卡；stage-1 gcd.cu 预过滤 → stage-2 verify_dual op-loop；详见 [gpu-nonce/LEVER_B_EXPLAINED.md](./gpu-nonce/LEVER_B_EXPLAINED.md)）。旧的单阶段 fleet 工具链 `hunt/`（`deploy.sh`/`node_hunt.sh`，单阶段 gpu-nonce + CPU fast-screen verify）**已归档到 `archive/`，不再用**。

```bash
# 在 vast.ai 机器上。先 Step-0 确认 sub-cliff（Phase 5.5）！只部署确认过的候选。
# 1) 改旋钮进 mod.rs（src/point_add/）+ unfreeze baked nonce → 重建 gpu-nonce（见 Phase 5）。
# 2) 跑（GCD-轴 knob 必须前缀赋值，env 才透传进 circuit_prep+stage-1+verify_dual）：
cd ecdsafail-agent/gpu-nonce
DIALOG_GCD_COMPARE_BITS=45 NGPU=8 START=<n0> CHUNK=20000000 CHUNKS=<n> \
  setsid nohup bash leverB_hunt.sh > /tmp/hunt.log 2>&1 < /dev/null &
```

`leverB_hunt.sh` 每 chunk 自动：circuit_prep 刷新 `/tmp/phase_circuit` dump → stage-1 扫 → stage-2 verify_dual 在幸存者上跑 → 出 `DUAL_CLEAN_CANDIDATE`。

**单机多卡即整个 fleet**：`leverB_hunt.sh` 用 `NGPU`/`GPULIST` 把所有 RTX5090 一次性吃满，`START` 内部按 `STRIDE` 给每卡分 disjoint range，不用手动分区间。要进一步加吞吐只能加卡 / 加 verify 核，没有跨机编排（已不再用 LAN fleet）。

巡检 / 停：
- **dump 刷新后核对** `od -An -tu8 -N24 /tmp/phase_circuit/meta.bin` 的 n_ops == 你 knob 下 `build_circuit`（`DIALOG_TAIL_NONCE=none`）的 base ops —— 防静默跑 default 电路。
- 候选数前 ~200s 是 0 = 正常（GPU scan 首批 host-bound）。
- 停：`pgrep -f "bash leverB_hunt.sh"` 拿脚本 PID 杀，**再** `pkill -9 -x gpu-nonce; pkill -9 -x verify_dual`（-x 精确名避免 pkill 自匹配）。
- winner = `/tmp/lb_s2_g*.log` 里的 `DUAL_CLEAN_CANDIDATE`；CPU `eval_circuit` 复核 0/0/0 后才 bake（Phase 7）。

---

## Phase 7 — On WINNER：烘焙、eval、commit、提交

详细流程、每步的 HARD STOP gate、commit 格式、Mac 端 submit 见：

**[winner_bake.md](./winner_bake.md)**

流程摘要（每步失败必须立即停止，不往后走）：
1. §1 确认 `/tmp/WINNER` nonce 在 verify.log 中是真正的 0/0/0
2. §2 sed 烘焙 nonce 进 `src/point_add/mod.rs`，确认替换成功
3. §3 官方 eval：classical=0 / phase-garbage=0 / ancilla-garbage=0 / OK / score < frontier — **全部满足才能继续**
4. §4 fetch origin，确认 frontier 在 hunt 期间没被超越
5. §5 本地 `git commit`（**永远不 push**），汇报 commit hash 和最终 score
6. §6 由用户在 Mac 端手动 `ecdsafail submit`

---

## 快速参考：分数换算

```
score = avg_executed_toffoli × peak_qubits

peak降1q 节省的分数 ≈ avg_toffoli
Toffoli降1T 节省的分数 ≈ peak_qubits
break-even ≈ avg_toffoli / peak_qubits（T/q）
→ 降peak比降Toffoli通常ROI更高，但peak越来越难降
```
> 具体数字以 Phase 0 实测 baseline 为准（frontier 每推进一次数字就变）。

---

## 常见失败模式

| 症状 | 原因 | 修复 |
|---|---|---|
| eval FAIL但fast-screen说0/0/0 | fast-screen binary是stale的 | `cargo clean -p quantum_ecc && cargo build --release` in agent/fast-screen |
| validate_pipeline 0/32 | GPU filter偏离新代码 | 检查 `dialog_gcd_classical_filter.rs` 的strict_body_trim gate |
| hunt有candidate但verify全FAIL | GPU filter方向错（overcount） | validate_pipeline确认gpu mean ≤ cpu mean |
| 进程在SSH断开后消失 | SIGHUP | 用`setsid ... &`启动 |
| eval FAIL 25 classical + 16 phase | nonce stale（代码改了但nonce没换） | 重新hunt |
| git diff有预期之外的改动 | 前次session留下working tree残留 | `git diff HEAD`检查，`git checkout -- <file>`还原 |
| 官方eval通过但submit被reject | 在hunt期间frontier又被别人更新了 | `git fetch`检查新frontier，用新分数重新评估是否仍然beat |
| 候选率 5% 而非 ~0.09%（B/gcd.cu） | 改 gcd.cu 后只重编了部分二进制，跑的是 stale binary | 重编**全部**用 gcd.cu 的 bin；查 `mtime > gcd.cu` |
| hunt「停了」却还在烧 GPU | `pgrep -x leverB_hunt.sh` 匹配不到（进程名是 bash），脚本还活着 respawn | `pgrep -f "bash leverB_hunt.sh"` 拿 PID 杀，再 `pkill -9 -x gpu-nonce/verify_dual` |
| B 出 DUAL_CLEAN 但 CPU eval FAIL | stale dump（/tmp/phase_circuit 是旧电路）→ phase XOF 错位 | leverB_hunt 自动 circuit_prep 刷新；verify_dual 断言 n_ops==build() |

---

## 机器约束速查（vast.ai 单机）

部署 = 在这台上跑两阶段 `gpu-nonce/leverB_hunt.sh`（Phase 6），`NGPU`/`GPULIST` 选卡、`numactl` 绑 NUMA。

| 机器 | 连接 | GPU | 备注 |
|---|---|---|---|
| vast.ai RTX5090 box | `ssh -p 63190 root@74.48.78.46 -L 8080:localhost:8080 -A`（先 `ssh-add ~/.ssh/id_rsa`） | 多卡 RTX5090（按 `nvidia-smi` 实际可见卡数设 `NGPU`/`GPULIST`） | 唯一节点；pytorch 模板，hunt 前确认 `numactl`/CUDA toolchain 已装 |

- Mac 端用上面的命令连这台，连上后 `cd /home/ubuntu/ben/temp/ecdsafail-challenge && export PATH=$HOME/.cargo/bin:$PATH` 再驱动一切。
- 单机操作，**无跨机 ssh**；后台进程一律 `setsid nohup ... &` 防 SIGHUP 断连即死。
