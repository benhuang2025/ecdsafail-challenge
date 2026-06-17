# ecdsafail — Winner Bake & Submit

WINNER 确认后的完整处理流程。每一步都有显式 STOP gate；**任何一步失败必须立即停止，不往后走**。

> **Subagent 调用**：本文档由独立 subagent 单独运行。按顺序执行 §1–§6，
> 在 §5 commit 完成后向主 agent 汇报结果（commit hash、最终 score、frontier 对比），然后关闭。
> §6 Mac 端 submit 由用户手动执行，不在本 subagent 职责内。

---

## §1 确认 WINNER（STOP gate）

```bash
WINNER=$(cat /tmp/WINNER)
echo "Winner nonce: $WINNER"
```

在 verify.log 里找到该 nonce 的验证行，确认是真正的 0/0/0：

```bash
grep "$WINNER" /tmp/verify.log
```

**期望输出**（三个字段全为 0）：
```
<nonce>  classical=0  phase_garbage=0  ancilla_garbage=0  OK
```

> **HARD STOP** — 如果 grep 没找到该 nonce，或任何字段非 0：
> 汇报 `§1 FAIL: nonce $WINNER 在 verify.log 中未通过 0/0/0`，停止。

---

## §2 烘焙 nonce 到代码（STOP gate）

```bash
cd /home/ubuntu/ben/ecdsafail-challenge

# 读取当前 DIALOG_TAIL_NONCE
OLD_NONCE=$(grep -m1 'DIALOG_TAIL_NONCE' src/point_add/mod.rs \
  | grep -oP '(?<=")[0-9]+(?=")')
echo "OLD_NONCE=$OLD_NONCE  WINNER=$WINNER"

# 替换第一个出现的（configure_ecdsafail_submission_route 里最靠前那个）
sed -i "s/set_default_env(\"DIALOG_TAIL_NONCE\", \"${OLD_NONCE}\")/set_default_env(\"DIALOG_TAIL_NONCE\", \"${WINNER}\")/1" \
  src/point_add/mod.rs

# 确认替换结果
grep 'DIALOG_TAIL_NONCE' src/point_add/mod.rs | head -3
```

> **HARD STOP** — 如果第一行仍然显示 `OLD_NONCE` 而不是 `WINNER`：
> sed 替换失败（可能 OLD_NONCE 提取有误），汇报错误，停止。不要手动乱改。

---

## §3 官方 eval（HARD STOP gate）

这是最关键的 gate。**0/0/0 + OK + score < frontier 全部满足才能继续。**

```bash
cd /home/ubuntu/ben/ecdsafail-challenge
numactl --cpunodebind=0 --membind=0 \
  cargo run --release --quiet --bin eval_circuit 2>&1 | tail -20
```

验收清单（逐项检查）：
- [ ] `classical mismatches    : 0`
- [ ] `phase-garbage batches   : 0`
- [ ] `ancilla-garbage batches : 0`
- [ ] `ROW: <avg_tof>  <qubits>  OK`（不是 FAIL）
- [ ] `score = avg_tof × qubits < frontier_score`

> **HARD STOP** — 任何一项不满足：
> - mismatches 非 0 或 phase-garbage 非 0：nonce 无效，还原 mod.rs（`git checkout -- src/point_add/mod.rs`），汇报失败，停止。
> - score ≥ frontier：优化方案被 frontier 追上，汇报 `§3 FAIL: score 不优于 frontier`，停止。

记录最终 eval 数字：`avg_tof=X  qubits=Y  score=X×Y`

---

## §4 检查 frontier 是否在 hunt 期间被超越

```bash
git fetch origin
git log origin/main --oneline -3
```

对比 `origin/main` 最新 commit 时间和本次优化的 base commit：
- 如果 origin/main 有新 commit（别人提交了更好分数），需要重新评估 score 是否仍然最优。
- 看最新 commit message：`git log origin/main --oneline -1`（message 里含 score）。如果 message 里没有 score，用 `git stash && cargo run --release --quiet --bin eval_circuit 2>&1 | grep ROW: && git stash pop` 实测新 frontier 分数。

> **HARD STOP** — 如果新 frontier 的 score ≤ 我们的 score（即我们不再是最优）：
> 汇报 `§4 FAIL: frontier 已被更新，我们的 score 不再是最优`，停止，等待主 agent 重新评估。

---

## §5 本地 commit（最后一步，不 push）

**永远不要 `git push`。**

```bash
cd /home/ubuntu/ben/ecdsafail-challenge
git add src/point_add/mod.rs
git commit -m "Beat frontier: <优化描述> + clean nonce ${WINNER}"
git log --oneline -3
```

commit message 中的 `<优化描述>` 要写：改了哪个 knob / lever、期望效果（如 `KAL_FOLD 18→17 (-638k score)`）。

commit 完成后，向主 agent 汇报：
```
§5 DONE
commit: <hash>
score: avg_tof=X  qubits=Y  total=X×Y
vs frontier: Δ=<负数，更好>
nonce: $WINNER
```

---

## §6 Mac 端提交（用户手动，subagent 不执行）

subagent 在 §5 完成后关闭。以下步骤由**用户在本地 Mac** 执行：

```bash
# 在 Mac 上同步 zan3 的代码
cd ~/Documents/brevis/ecdsafail-challenge
git fetch zan3   # 或 rsync / scp src/point_add/mod.rs

# 确认 score 还是最优后提交
ecdsafail submit --note-file submission-note.md --model "Claude Sonnet 4.6"
```

submission note 要写：
- 改了哪个 knob，引用 [peak_reduction_skill.md](./peak_reduction_skill.md) 或 [toffoli_reduction_skill.md](./toffoli_reduction_skill.md) 中对应 lever
- 本地 eval 分数（avg_tof × qubits）
- 使用了哪些工具（fast-screen、gpu-nonce、validate_pipeline）
