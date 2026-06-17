# Nonce 搜索(Hunt)——流程、瓶颈、如何提速

> 2026-06-13 在一次 SQWIN_CLEAN=18 的实跑中写成。所有数字都是**本次实测**(zan3:2×RTX 5090 扫描 + 62 核 verify)。目标:讲清每个组件在做什么、时间到底花在哪、并给出方案,让 **hunt 不再是卡点**。

---

## 一句话总结

- **hunt 的瓶颈是 VERIFY,不是 scan。** GPU 产候选的速度比 CPU verify 消费的速度快约 **180 倍**(本轮:攒了 250 万候选,只验了 ~1.4 万)。
- **verify 被白白浪费的根因:** GPU 预筛只建模 **GCD** 这条 hazard 轴,**不管 square-cleanup / apply** 轴。所以大多数"GPU-clean"候选其实仍是 square-dirty,在昂贵的全电路 verify 里被否掉。enrich 很弱(λ_candidate ≈ λ_random)。
- **verify 慢是因为 `fast-screen` 做的是全门级模拟**:把 ~1020 万门的电路在 9024-shot 岛上逐门跑一遍(每个候选 ~9 秒),而没有用便宜的解析 filter。
- **根本解法**(让以后**任何** idea 都能快速 hunt):把 filter 做**完整**(GCD **+** apply **+** square 都建模)且**快**(解析,不是全模拟,最好进 GPU kernel)。这样候选到手时在所有 classical 轴上都接近干净,verify 几乎不用跑慢路径。

---

## 0. 为什么需要 hunt

分数是在一个**固定的 9024-shot Fiat–Shamir 岛**上评的。这个岛(9024 个测试输入 + 模拟器 R/Hmr 的随机性)由对**整条 op stream 做 `SHAKE256`** 来播种:

```
seed = SHAKE256( "quantum_ecc-fiat-shamir-v2" || len(ops) || 每个 op 的 7 个字段 )
```

两个后果:
1. `src/point_add/` 的**任何**改动都会改变 op stream → 重新 roll 岛 → 之前烘焙的 `DIALOG_TAIL_NONCE` 就过期了。
2. **island-exact** 类 lever(截断:丢一个 carry bit、缩 comparator、压窄 GCD 宽度)在*大多数*输入上算对,但在**少数"hard input"上算错**。这些 hard input 会不会落进岛里,取决于 nonce。

一个 **winner nonce** = 它对应的 op stream 的岛**恰好躲开了所有 hard input** → `classical=0, phase=0, ancilla=0`。`DIALOG_TAIL_NONCE` 就是挪动岛的旋钮(它往 op stream 末尾追加一段"tail" → 改变 SHAKE 种子)。**hunt 就是在 nonce 空间里搜一个干净的岛。**

> 心智模型:每个候选优化往输入空间里埋了若干"地雷"(hard input)。hunt 不停掷骰子(选一个 nonce → 一个 9024 输入的岛),直到掷出一把**一个地雷都没踩到**的。地雷越多(截断越狠)⇒ 干净的一把越罕见 ⇒ hunt 越难。这就是"cliff(悬崖)"。

---

## 1. 三个阶段

```
        nonce 空间 (2^48)
              │
   ┌──────────▼───────────┐   阶段 A:GPU SCAN  (gpu-nonce, CUDA)
   │  便宜的 GCD 预筛       │   ~8,700 nonce/s(2 卡)。约 5.8% 输出为
   │  每个 nonce:这个岛    │   "CLEAN_CANDIDATE"(9024 shot 全部 GCD-clean)。
   │  是不是 GCD-clean?    │
   └──────────┬───────────┘
              │  CLEAN_CANDIDATE nonce=...   (候选堆积非常快)
   ┌──────────▼───────────┐   阶段 B:CPU VERIFY  (fast-screen)
   │ 对 1020 万门电路       │   -P32 下 ~3/s。每个候选 ~9 秒。确认整条电路
   │ 做全门级模拟,         │   (GCD+apply+square+phase)都干净。这是瓶颈。
   │ 跑完那个岛            │
   └──────────┬───────────┘
              │  WINNER nonce (classical=0 phase=0 ancilla=0)
   ┌──────────▼───────────┐   阶段 C:官方 EVAL (build_circuit + eval_circuit)
   │  可信打分器           │   bake 时只跑一次。ground truth。写 score.json。
   └──────────────────────┘
```

---

## 2. 每个组件到底在做什么

### `gpu-nonce/src/gcd.cu` —— GPU 预筛 kernel
- 是 CPU filter(`dialog_gcd_classical_filter.rs`)**一部分**的 CUDA 移植。头注释明说:*"Truncated dialog-GCD filter … BodyTrimMismatch + ComparatorMismatch disabled."*
- 每个 nonce、每个 shot(9024 个):用 SHAKE + EC 标量乘(预算 `gtable`)推出测试点,组出 GCD 因子,**把截断 binary-GCD 复算 `ACTIVE_ITERATIONS`(258)步**。判该 shot **hard** 当且仅当 **WidthOverflow**(某值超出 active-width 包络)**或**终值 `u ≠ 1`(不收敛)。
- 一个 nonce 是 `CLEAN_CANDIDATE` 当且仅当**全部 9024 shot** 都过。
- **关键局限:它只建模 GCD 轴。** 它**不**建模 apply 值 hazard,也不建模 **square-cleanup carry escape**。所以它只能 enrich 难度里的 GCD 那部分。
- 每步 active 宽度 `AW[]/CB[]/BW[]` 在 host 上由 `DialogGcdFilterConfig::from_env()` 算出并上传。`gpu-nonce` 会先 `build()`,所以烘焙的旋钮(WIDTH_MARGIN、COMPARE_BITS…)会自动流进 filter。

### `dialog_gcd_classical_filter.rs` —— 解析 filter("source of truth"模型)
- `check_gcd_factor()` —— GCD 复算(`gcd.cu` 镜像的就是它)。
- `check_point_add_apply_hazards()` —— apply 值 hazard。
- `square_row_window_cleanup_summary()` —— **square-cleanup carry escape**(`gcd.cu` 忽略的那条轴)。
- `check_all_shots()` / `check_point_add_inputs()` —— 在一个岛上跑**完整**的 classical 模型,**完全不模拟门**。
- **这是关键资产。** 它已经建模了 GPU kernel 没建模的轴。但 `fast-screen` 当前**没用它**。

### `fast-screen` —— CPU 验证器(当前实现)
- 每个候选:`point_add::build()`(~1.6 秒)→ 对全部 ~1020 万 op 做**全门级模拟**(64 shot 一批、位切片、最多 141 批)→ 报 `classical / phase / ancilla`。
- **~9 秒/候选**(我的)/ ~15.8 秒(旧 binary,它还顺带数 Toffoli)。**模拟那一步是大头(~7.6 秒)**,不是 build。
- 它是**正确的**(全模拟 = ground truth)但**慢**,而且**没用解析 filter**。

### `validate_pipeline` —— 对齐检查
- 把 N 个 nonce 同时过 GPU kernel 和 CPU `check_gcd_factor`,打印 `agree=N/32`。`agree=32/32` ⇒ GPU kernel 精确复现 CPU 的 GCD filter。(电路改动后 kernel 可能漂移,必须重新对齐 —— 见 §6。)

### `eval_circuit` —— 官方打分器(阶段 C)
- 读 `ops.bin`、重新模拟、执行 4 项校验、写 `score.json`。是**唯一**判定真实胜负的东西。bake 时用一次。

---

## 3. 实测数字(本次)

| 量 | 数值 | 备注 |
|---|---|---|
| GPU 扫描速率 | **~8,700 nonce/s**(2 卡,各 ~4,350) | 每 nonce = 9024 shot ×(2 次 EC 乘 + 258 步 GCD) |
| 候选率 | **~5.8%**(≈58,000/M) | 通过 GCD 预筛的比例 |
| 候选产生速率 | **~500 候选/s** | 8,700 × 5.8% |
| Verify 速率(`-P32`) | **~3/s** | 瓶颈 |
| `fast-screen` 单次 | **~9 秒/候选**(我的)、15.8 秒(旧) | build 1.6 秒 + sim ~7.6 秒 |
| t≈75 分钟时积压 | **攒了 250 万候选,验了 ~1.4 万** | verify 落后约 180 倍 |
| λ_cand(GCD-clean 候选的全 classical) | **15.0**(SQWIN=18) | baseline 14.3,随机 17.7 |
| GPU 平均 hard/nonce(仅 GCD) | 2.5(SQWIN=18)、2.78(baseline) | filter *确实* enrich 的那部分(~λ2.5) |
| `validate_pipeline` | **agree=32/32** | GPU kernel 已对齐 CPU GCD filter |
| P(cls=0) 上界 | **< ~1/5000**(1.47 万里 0 个 clean) | 只是上界 —— 要算时间必须先观测到 ≥1 个 cls=0(见 §5) |

---

## 4. 瓶颈在哪(按重要性排序)

1. **VERIFY 吞吐(~3/s)。** 它决定 wall-clock。攒了 250 万候选、只验了 1.4 万 → verify 是闸门,其它都供过于求。
2. **enrich 太弱(#1 被浪费的根因)。** GPU filter 只 enrich **GCD** 轴(压掉 ~λ2.5:17.7→15.0)。剩下的 ~15 个 mismatch 大头是 **square-cleanup/apply** —— GPU 从没筛过。于是几乎每个候选都在 9 秒全模拟里死掉。`λ_candidate(15) ≈ λ_random(17.7)` ⇒ 落入文档说的"filter 没 enrich"区间。
3. **`fast-screen` 用全模拟,不用解析 filter。** ~9 秒/候选,而解析 `check_all_shots` 只需亚秒。
4. **每进程重建。** `fast-screen` 每次都重建整条电路(~1.6 秒);持久 daemon 只需 build 一次 base。

**不是瓶颈:** GPU 扫描(供过于求 180 倍)。加扫描 GPU 对 time-to-winner **毫无帮助**。

---

## 5. 为什么不能用 `e^(−λ)` 估时间

`λ` = 每个岛平均 hard shot 数。若 shot 独立 Poisson,则 `P(cls=0)=e^(−λ)` → λ=15 时 e^(−15)≈3×10⁻⁷("不可行")。**但 shot 是相关的**(由岛结构驱动),所以 zero-bin 厚得多:实测里一个相近 λ≈14 的案例 `P(cls=0)≈1/7000≈e^(−8.8)`,而不是 e^(−14)。所以:
- λ **只能用来比相对难度**(baseline 14.3 = 可 hunt;WIDTH_MARGIN=9 → 23.5 = 过 cliff)。
- 要真正估时间,**必须先观测到 ≥1 个 `cls=0`**,用经验 `P(cls=0)=CLS0/N`,再 `P(winner)=P(cls=0)·P(phase=0|cls=0)`。

---

## 6. 核心答案:为什么 HUNT 永远是卡点

写电路 idea 是**确定性的、快**。hunt 是**随机的**,它的成本由两件事决定:

1. **你的改动加了多少 hard input(λ)。** 截断越狠 = 分数赢越多 = 地雷越多 = P(winner) 越低。过了"cliff"(λ 远超 baseline ~14),可行时间内根本不存在干净的岛。→ *优先选温和的、sub-cliff 的、或 value-exact 的改动。*
2. **filter 对你这个改动碰到的 hazard 建模得有多完整。** filter 就是把"暴力掷骰子"变成"enrich 搜索"的东西。**如果 filter 没建模你改动扰动的那条轴,hunt 就在那条轴上退化成暴力** —— 哪怕 `validate_pipeline` 仍然通过(它只查 GCD 轴)。

> 本次例子:`SQWIN_CLEAN=18` 扰动的是 **square-cleanup** 轴。GPU filter 只建模 **GCD**。所以 GPU-clean 候选在 square 轴上**没被 enrich** → 残留 ~15 个 mismatch → verify 每个花 9 秒只为否掉它。hunt"卡住"不是因为 idea 不好(它是个干净的 −273k 赢),而是因为**工具无法 enrich 这个 idea 所碰的轴。**

**所以:一个 idea 的可 hunt 程度,等于 filter 对它引入的 hazard 的完整程度。** 这才是你老卡在 hunt 的真正原因。

---

## 7. 如何提速(按杠杆大小排序)

### A. 把 filter 做完整 —— 建模 square-cleanup + apply,不只 GCD  ★ 最大的结构性收益
把 `square_row_window_cleanup`(以及 apply hazard)移植进 **GPU kernel**(或至少做成一道 CPU 解析预筛)。这样"候选"= *在所有轴上 classical-clean*,候选到手时已接近 winner,verify 几乎不跑慢路径。这能让**任何**未来的截断 idea 都可 hunt,而不只是 GCD 轴的。

### B. 把 `fast-screen` 的全模拟换成解析 filter  ★ verify ~10×
用 `check_all_shots()` 替代门级模拟。两种形态:
- *预筛*(最稳):GPU(GCD) → CPU `check_all_shots`(补上 square/apply) → 只有稀有的 classical-clean 幸存者才送**全模拟/eval** 查 phase。零正确性风险(eval 是终判)。
- *完全替换*(更快但更险):解析 classical + 一个 phase 模型。需对着 eval 仔细校验,尤其 **phase**(解析 filter 多半不建模 phase;phase=0 **不能**由 classical=0 推出)。
难点:(1) phase 没法解析建模 → 解析只能当 *classical* 预筛;(2) 岛种子仍需 op stream(要么每次保留 `build()`,要么学 GPU 做 hash 追加 —— 易错);(3) 必须对着 eval 验完整性。
预期:保留每次 `build()` 约 ~3–5×;**持久 daemon(base 只 build 一次)约 ~10–40×**。

### C. 持久 verify daemon —— base 电路只 build 一次
`fast-screen` 每次重建 ~1020 万 op(~1.6 秒)。一个常驻 daemon 把与 nonce 无关的 base 只 build 一次,只变 tail(像 GPU 那样)。省掉每次的 build。

### D. 并行 —— 简单、线性
zan3 上 `-P32 → -P56`(62 核、920 GB 内存)≈ 1.75×。加机器(每台是一个独立 worker,跑不重叠的 nonce 段)。**注意共享机器**:每个 worker 的 `/tmp` 文件加前缀隔离(`/tmp/<host>_*`)、只按自己 binary 的唯一路径杀自己的 PID、共享机上绝不 `pkill -x`。

### E. 选可 hunt 的 idea —— 设计成低 λ
- 先 screen λ(改完后一次 FAIL-row `eval` 就能看到 classical+phase displacement)。
- **value-exact** lever(不加 hard input)按 baseline 速率 hunt —— 总是可 hunt。
- 截断类要保持 **sub-cliff**(λ_cand 接近 baseline ~14)。WIDTH_MARGIN=9(λ 23.5)是更大的分数赢但不可 hunt;SQWIN=18(λ 15)是更小的赢但可 hunt。

---

## 8. 目标架构(让 hunt 永远不再是卡点)

```
GPU kernel:  完整解析 filter(GCD + apply + square)跑全部 9024 shot
             → "候选" = 在每条轴上都 classical-clean(强 enrich)
CPU verify:  对稀有候选做轻量 phase/ancilla 确认
             (或只对这几个做全模拟)
官方 eval:   bake 时的最终 0/0/0 终判
```
filter 一旦完整,候选流就在所有 classical 轴上预先 enrich 过,P(winner|候选) 很高,verify 几乎不跑昂贵路径。**这样新 idea 的 hunt 成本 ≈ 几分钟,不管它碰哪条轴**(只要 filter 建模了那条轴 —— 所以当你在新轴上加 lever 时,*先*扩 filter)。

---

## 9. 每个 idea 的清单(每次开长 hunt 前都做)

1. **screen λ:** 改旋钮 → 重编 `build_circuit` → `eval` → 读 stale nonce 上的 classical+phase displacement。>35 ⇒ 多半过 cliff,放弃。
2. **filter 覆盖:** filter 建模了你这个改动碰的 hazard 轴吗?没有的话,**先扩 filter**(否则 hunt 无法 enrich → 暴力)。
3. **重编 + `validate_pipeline`:** `cargo clean -p quantum_ecc` 然后重编 gpu-nonce + fast-screen;`agree=32/32`(若 `gcd.cu` 漂移则重新对齐)。
4. **Step 0 enrich:** 用全 classical(`NS_EARLY_EXIT=0`)验 ~40 个 GPU-clean 候选 vs ~10 个随机。要 `λ_candidate ≪ λ_random`;若 `≈`,说明 filter 没 enrich 你这条轴 → 去修 filter,别 hunt。
5. **Step 1 经验 P:** hunt 到 ≥1 个 `cls=0`,算 `P(winner)`,再估时间。**绝不**用 `e^(−λ)` 报时间。
6. **Hunt**,然后 **bake**(把 nonce 重新 `set_var` 冻结、官方 `eval` 0/0/0 + score < frontier、commit)。

---

## 11. 代码分层:什么每次都变、什么一直不变(upstream 每几小时更新,如何不大改)

> 背景:`github.com/ecdsafail/ecdsafail-challenge` 每隔几小时就有人提交更好的设计,frontier 不停推进。目标:别人一更新,我这边**只 `git pull` + 重编 + 一项自动检查**,而不是大改。

**已确认的事实(决定了分层):**
- `dialog_gcd_classical_filter.rs`(filter 模型)**是 upstream 跟踪的**,随 frontier commit 一起变 → **我不维护它,`git pull` 就拿到最新的**。
- `ecdsafail-agent/`(我的全部工具)**是 untracked 的,在 `src/` 之外** → **upstream 更新永远不碰它**。

### 三层划分

| 层 | 包含什么 | 谁维护 / 更新方式 |
|---|---|---|
| **L1 upstream `src/`**(每几小时变) | 电路 `point_add/**` + filter 模型 `dialog_gcd_classical_filter.rs` + 固定 harness(`bin/build_circuit`、`bin/eval_circuit`、`circuit.rs`、`sim.rs`、`weierstrass_*`、`lib.rs`) | **别人维护**。我 `git pull` / rebase,**从不手改** |
| **我的提交 delta**(很小) | `mod.rs` 里我调的那几个 `set_var`(我的优化旋钮)+ 我 hunt 出来的 `DIALOG_TAIL_NONCE` | 我维护,**就几行**,每次 sync 后 rebase 上去。这才是要提交的东西 |
| **L2 我的工具 `ecdsafail-agent/`**(基本不变) | gpu-nonce + fast-screen + 脚本 + 文档 | 我维护,但**设计成电路无关**,upstream 更新只需重编 |

### L2 内部:纯吞吐(永不改)vs 唯一的镜像(偶尔对齐)

| 文件 | 性质 | upstream 变时怎么办 |
|---|---|---|
| `gpu-nonce/`:`field.cuh`、`points.cu`、`keccak.cu`、`keccak.rs`、`gtable.rs`、`hunt.cu`、`main.rs` | **纯吞吐**(实现固定的 secp256k1 + SHAKE-over-ops 契约,这俩永远不变) | **不动**,顶多重编 |
| `fast-screen/src/main.rs` | **编排壳**(调 `build()` + 固定 sim,或调 upstream filter 函数) | **不动**,重编即自动跟上新电路 |
| **`gpu-nonce/src/gcd.cu`** | **唯一手维护的镜像** —— filter 逐步 replay 的 CUDA 移植 | upstream 改了 filter 的 GCD-replay 逻辑时会**漂移**;`validate_pipeline`(`agree=N/32`)**自动检测**,漂了才重对齐 |

### 每次 upstream 更新的固定流程(不是大改)

```
1. git pull / rebase 我的 delta        # 拿到新电路 + 新 filter
2. 重编 gpu-nonce + fast-screen          # 它们 link quantum_ecc,自动跟上新电路/filter
3. validate_pipeline                     # agree=32/32 → gcd.cu 仍同步,完事
                                         # 否则:只重对齐 gcd.cu(唯一的手工活)
4. Step-0 enrichment 自检                # 确认 filter 对当前电路仍 enrich
```
**唯一可能要手改的就是 `gcd.cu`,而且只在 `validate_pipeline` 报漂移时。** 其余全是 `git pull` + 重编。

### 黄金原则:**调用 upstream,不要复制 upstream**

我的工具要**调用** upstream 的 filter 函数(`check_all_shots`、`square_row_window_cleanup_summary`、`check_gcd_factor`),**绝不把它们的逻辑抄进自己的代码**。这样 upstream 一改,我重编就自动跟上。唯一不得不"复制"的是 `gcd.cu`(Rust→CUDA 没法直接共享),所以把它**保持最小**、用 `validate_pipeline` 当守门。

**安全网(自动发现漂移,不靠人记):** `validate_pipeline`(gcd.cu vs CPU filter)+ Step-0 enrichment(filter 覆盖够不够)+ 官方 `eval`(终极 ground truth)。三道都过,才信任 hunt。

### 这对"B vs A"的直接结论

- **B(CPU 解析 verify,调用 upstream filter 函数)**:零新增漂移面 —— upstream 改 square 逻辑,`square_row_window_cleanup_summary` 跟着变,我的 B 壳重编即自动跟上。**维护友好。**
- **A(把 square 移植进 `gcd.cu`)**:**增加**手维护的 CUDA 镜像面 → 每次 sync 多一处要对齐。而且 GPU scan 本就不是瓶颈。**从可维护性看,A 不划算,优先 B。**
- **全模拟版 fast-screen 是 100% upstream-proof 的兜底**(零模型耦合,只 build+sim)。即使加了 B 的解析快路径,也保留它当 ground truth。

> 一句话:**electrocircuit/filter 的逻辑只此一份(upstream 的 Rust),我的工具一律"调用"它;唯一的副本是最小化的 `gcd.cu`,由 `validate_pipeline` 守门。** 所以别人更新 ≠ 我大改,而是 `git pull` + 重编 +(偶尔)对齐 gcd.cu。

---

## 10. 血泪坑(本次踩的)

- **`set_var` 块(`mod.rs` ~1489–1546)** 用 `set_var` 强行钉死旋钮**和**nonce,无视 shell env。对这些旋钮用 shell-env 来 screen 是静默无效的;必须改字面量 + 重编。烘焙的 nonce 是 `set_var`(冻结)—— **hunt 前要把它改回 `set_default_env`,才能让 hunt 变 nonce。**
- **`gcd.cu` 会漂移。** 电路改动后它可能与 CPU filter 过/欠计数(我们见过 `agree=0/32`、GPU 判 33 vs CPU 5,因为它有个无条件 WidthOverflow 否决,而 CPU 默认不这么做)。信任 hunt 前必须重新对齐到 `agree=32/32`。
- **共享机器:** zan4 上有别人的任务(alan、tommy 的 GPU python),都在同一个 `ubuntu` 账户下。要隔离 `/tmp` 文件、只按唯一路径杀自己的 PID、绝不 blanket `pkill -x`。
</content>
