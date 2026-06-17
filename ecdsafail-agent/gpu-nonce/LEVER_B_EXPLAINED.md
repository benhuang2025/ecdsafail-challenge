# Lever B —— 两段式 nonce search,为什么能快几十倍

> 一句话:**不要在每个 nonce 上都跑完整电路。先用一个便宜的"解析检查"把 99.9% 注定失败的 nonce 在不跑电路的情况下毙掉,只对幸存者跑昂贵的完整模拟。**

---

## 1. 背景:nonce search 在搜什么,为什么慢

每次我们改电路(截断某个加法器/比较器来省 Toffoli),就会引入一批 **hard input**——在这些输入上截断算错。
电路要通过测试,必须 0 mismatch。但测试用哪些输入,是由一个哈希 **island** 决定的:

```
SHAKE256(整个 op-stream)  ->  选定 9024 个测试 shot
```

`DIALOG_TAIL_NONCE` 这个 48-bit 数会在 op-stream 末尾追加 96 个恒等 op(X;X),
**扰动哈希 → 换一组 island**。所以 nonce search 就是:

> 不停换 nonce,直到碰到某个 island,它的 9024 个 shot **恰好全部避开**了所有 hard input
> (= classical 0 mismatch **且** phase 0 garbage)。这样的 nonce 叫 **clean nonce / winner**。

clean nonce 极其稀有(约 1e-8 ~ 1e-9 量级),所以要扫海量 nonce。**评估每个 nonce 的成本,直接决定整个搜索要多久。**

---

## 2. 老办法的瓶颈:每个 nonce 都跑完整 op-loop

最初的评估器(CPU)和后来的 fused GPU kernel(`hunt_dual` / `hunt_phase`),做法是:

```
对每个 nonce:
   for 141 个 batch (每 batch 64 shot):
       跑完整 10,288,316 条 op 的可逆电路模拟   <-- 昂贵
       检查 classical 是否匹配 + phase 是否为 0
   第一个失败的 batch 出现就 early-exit
```

问题:**判断一个 nonce 是不是 classical-fail,也得真的把电路 op-loop 跑起来。**
而 99.9%+ 的 nonce 都是 classical-fail——我们为了得出"这个 nonce 不行",
白白跑了一大段昂贵的 op-loop。

实测:
- CPU fleet(~470 核):**~40 nonce/s**
- fused GPU kernel(单卡):**~200 nonce/s**

按 ~1e9 nonce 的搜索规模,单靠 fused kernel 8 卡也要 **~9 天**。

---

## 3. 关键洞察:classical 失败可以"解析地"判定,根本不用跑电路

电路的核心是一个 **二进制 GCD / Kaliski 模逆**。一个 nonce 的某个 shot 会不会 classical-fail,
**完全由这条 GCD 的截断算术决定**,和 phase 随机数无关。

而这条 GCD 的逻辑,我们可以用**几十条整数位运算**在 GPU 上**直接重放(replay)**,
得出"这个 shot 的截断 GCD 会不会算错"——**完全不需要执行那 1000 多万条 op**。

这就是 `gcd.cu`:一个**纯解析的 classical 预筛**。它的速度不再受 op-loop 限制,
而是受椭圆曲线输入推导限制:

- `gcd.cu` 预筛:**~8,300 nonce/s/卡**(对比 op-loop 的 ~200/s,**快 ~40 倍**)

---

## 4. 两段式架构

```
                        海量 nonce (1e9 量级)
                               │
        ┌──────────────────────▼───────────────────────┐
        │  STAGE 1 :  gcd.cu 解析预筛                     │
        │  ~8,300 nonce/s/卡 ,不跑 op-loop                │
        │  把 classical 注定失败的 nonce 全部毙掉          │
        └──────────────────────┬───────────────────────┘
                  ~0.01–0.1% 幸存 (classical-clean 候选)
                               │
        ┌──────────────────────▼───────────────────────┐
        │  STAGE 2 :  verify_dual (完整 op-loop)          │
        │  只对幸存者跑昂贵模拟,验 classical + phase       │
        │  输出 0/0 的就是 DUAL-CLEAN 候选                 │
        └──────────────────────┬───────────────────────┘
                               │ 命中
        ┌──────────────────────▼───────────────────────┐
        │  CPU eval_circuit 金标准复核 (0/0/0)            │
        │  = 真正的 winner,可提交                         │
        └────────────────────────────────────────────────┘
```

**核心思想**:把"便宜但不完整"的检查放前面挡掉绝大多数,
"昂贵但精确"的检查只在极少数幸存者上做。

---

## 5. 为什么能快这么多 —— 一笔账

设扫 N 个 nonce,候选率 p(stage-1 放过的比例):

```
总时间 ≈ N / (stage1速率)   +   (N·p) / (stage2速率)
            └─ 便宜,全量 ─┘      └─ 昂贵,但只占 p 比例 ─┘
```

只要 **p 足够小**,第二项可忽略,系统速率 ≈ stage-1 速率。

| 方案 | 速率(8 卡) | 1e9 nonce 耗时 |
|---|---|---|
| CPU fleet | ~40/s（总） | ~290 天 |
| fused op-loop | ~1,600/s | ~7 天 |
| **Lever B** | **~27k–66k/s** | **~4–10 小时** |

相对 fused **~17–40×**,相对 CPU fleet **~700–1600×**。

> 速率是区间,因为它取决于候选率 p:p 越低,瓶颈越靠近 stage-1(越快)。
> p 由 `gcd.cu` 的富集能力决定 —— 见下一节。

---

## 6. 唯一的硬性正确要求:`gcd.cu` 必须"保守"(0 false-hard)

stage-1 一旦把某个 nonce 毙掉,它就**永远不进 stage-2**。
所以如果 stage-1 误杀了真正的 winner,**整个搜索就漏了解,白搜**。

形式化要求:

- **false-hard(致命)= 0**:绝不能把"真 classical-clean 的 shot"判成 hard。
  → 保证任何真 winner 都能通过 stage-1。
- **false-clean(无害)**:把"其实 hard 的 shot"放过去没关系,stage-2 的精确 op-loop 会再抓一次。

所以 `gcd.cu` 宁可**多放、绝不错杀**。最保守的核就是只查 `终态 u≠1`
(模逆的 GCD 必须终止于 u=1,这是任何 frontier 都成立的结构性事实),
天然 0 false-hard。在此之上可以加更激进的 codec 富集(提高拒绝率、降低 p、跑更快),
**但每次换 frontier 都必须用 `leverB_diag` 重新验证 0-false-hard**——
codec 表是按特定 frontier 推导的,过期就会产生 false-hard,这时退回保守核即可。

> 富集 vs 安全的权衡:codec 开 = p 小 = 快,但需 per-frontier 维护;
> 保守核 = p 大些 = 略慢,但任何 frontier 天然安全。两者都正确,只差速度。

---

## 7. 两个真实踩过的坑

1. **陈旧转储 false-clean**:stage-2 用的电路转储(`/tmp/phase_circuit`)如果来自**旧 frontier**,
   它和 verify_dual 算 phase 用的哈希就**对不上**,phase 判定全错,会报假的 winner。
   → 修复:`verify_dual` 启动时断言 `dump.n_ops == build() 的 op 数`,不符直接 panic;
   `leverB_hunt.sh` 每次开跑前强制重新 `circuit_prep` 刷新转储。
   → 即便漏了,**CPU 金标准复核**是最后一道网,假 winner 过不了它。

2. **gcd.cu 必须随 frontier 自适配**:步宽(active width / compare bits / carry trunc)
   通过 `DialogGcdFilterConfig::from_env()` **自动适配**当前 knob,无需手改;
   只有硬编码的 codec 表需要 per-frontier 验证/重生成。

---

## 8. 怎么用(一条命令)

```bash
# 每次改完电路:
NGPU=8 START=<起始nonce> CHUNK=<每卡每轮nonce数> CHUNKS=<轮数> \
  bash ecdsafail-agent/gpu-nonce/leverB_hunt.sh
```

脚本自动:刷新转储 → stage-1 多卡预筛 → 收候选 → 拆分 → stage-2 并行验 →
命中打印 `DUAL_CLEAN_CANDIDATE` → 最后用 CPU `eval_circuit` 复核即可提交。

每换一个 frontier,先跑一次 `leverB_diag` 确认 `PER-SHOT FALSE-HARD = 0`,再开搜。
```

---

### TL;DR
**老办法对每个 nonce 都跑完整电路才能判它失败;Lever B 发现"会不会 classical 失败"可以用几十条位运算解析地算出来,于是先用这个便宜检查毙掉 99.9% 的 nonce,只对幸存者跑昂贵的完整模拟。** 便宜检查快 ~40×,且只要它"绝不错杀真 winner"(0 false-hard,CPU 复核兜底),整体就能在保证找到解的前提下提速 ~17–40×。
