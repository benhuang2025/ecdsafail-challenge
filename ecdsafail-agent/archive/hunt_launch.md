# Hunt Launch & Health Check

GPU hunt + verify daemon 的完整启动流程。**每次启动后必须通过 §3 的 health check 才算真正开跑**。

> **Subagent 调用**：本文档由独立 subagent 单独运行。完成 §3 health check（全部 6 项通过）后向主 agent 汇报 PASS/FAIL，然后关闭。

---

## §1 启动前预检（pre-flight）

```bash
REPO=/home/ubuntu/ben/ecdsafail-challenge

# 1. 确认 binary 是最新编译的（时间戳比 src/ 新）
ls -lt $REPO/agent/gpu-nonce/target/release/gpu-nonce \
       $REPO/agent/fast-screen/target/release/fast-screen
# 如果 binary 比 src/point_add/ 旧，必须重新 cargo build

# 2. 确认 DIALOG_TAIL_NONCE 已烘焙到代码里（Phase 5 rebuild 后有效）
grep "DIALOG_TAIL_NONCE" $REPO/src/point_add/mod.rs | head -3

# 3. 确认没有残留的旧进程
pgrep -a gpu-nonce || echo "clean"
pgrep -a fast-screen || echo "clean"
# 如果有残留：kill $(pgrep -f gpu-nonce); kill $(pgrep -f verify_daemon)

# 4. GPU 可用
# hunt 前 GPU1-7 应全为 0%（validate_pipeline 已结束，GPU2 可用于 hunt）
nvidia-smi --query-gpu=index,utilization.gpu --format=csv,noheader | grep -E "^[1-7],"

# 5. 清掉旧 log（避免旧 CLEAN_CANDIDATE 污染新 verify）
rm -f /tmp/hunt_z3_g*.log /tmp/verify.log /tmp/verify_main.log \
      /tmp/seen.txt /tmp/allcand.txt /tmp/new.txt /tmp/WINNER
echo "logs cleared"
```

---

## §2 启动

```bash
# hunt（zan3 GPU1-7，NUMA0，range [1, 350M)）
cat > /tmp/hunt_z3.sh << 'EOF'
#!/bin/bash
cd /home/ubuntu/ben/ecdsafail-challenge
export PATH=/usr/local/cuda/bin:$HOME/.cargo/bin:$PATH
export LD_LIBRARY_PATH=/usr/local/cuda-12.8/targets/x86_64-linux/lib:$LD_LIBRARY_PATH
BIN=agent/gpu-nonce/target/release/gpu-nonce
PER=50000000; i=0
for d in 1 2 3 4 5 6 7; do
  s=$((1 + i*PER))
  CUDA_VISIBLE_DEVICES=$d HUNT_START=$s HUNT_COUNT=$PER \
    HUNT_BATCH=1048576 HUNT_BS=64 HUNT_GPUS=1 \
    numactl --cpunodebind=0 --membind=0 $BIN > /tmp/hunt_z3_g$d.log 2>&1 &
  i=$((i+1))
done
wait
EOF
chmod +x /tmp/hunt_z3.sh

# verify daemon（读 zan3 的 hunt log）
cat > /tmp/verify_daemon.sh << 'EOF'
#!/bin/bash
FS_BIN=/home/ubuntu/ben/ecdsafail-challenge/agent/fast-screen/target/release/fast-screen
FS_DIR=/home/ubuntu/ben/ecdsafail-challenge
rm -f /tmp/seen.txt /tmp/WINNER

vone() {
  n="$1"
  r=$(cd "$FS_DIR" && DIALOG_TAIL_NONCE="$n" NS_EARLY_EXIT=1 NS_SHOTS=9024 \
      numactl --cpunodebind=0 --membind=0 "$FS_BIN" 2>/dev/null | grep "^RESULT")
  echo "$n | $r" >> /tmp/verify.log
  echo "$r" | grep -q "classical=0 phase=0 ancilla=0" && echo "$n" >> /tmp/WINNER
}
export -f vone

while [ ! -s /tmp/WINNER ]; do
  grep -h CLEAN_CANDIDATE /tmp/hunt_z3_g*.log 2>/dev/null \
    | sed "s/.*nonce=//" | sort -un > /tmp/allcand.txt
  comm -23 /tmp/allcand.txt <(sort -un /tmp/seen.txt 2>/dev/null || true) > /tmp/new.txt
  cat /tmp/new.txt >> /tmp/seen.txt
  # -P 32: ~21 parallel verifies saturate the 8-GPU candidate output (R_hunt~2.6/s x ~8s/verify);
  # node0 (62 cores, 560MB/verify) fits it. Was -P 8 => verify-bound (R_verify~1/s) => ~2.6x too slow.
  [ -s /tmp/new.txt ] && xargs -P 32 -I{} bash -c 'vone "$@"' _ {} < /tmp/new.txt
  sleep 3
done
echo "WINNER: $(cat /tmp/WINNER)"
EOF
chmod +x /tmp/verify_daemon.sh

# 启动（setsid 防 SSH 断线）
setsid bash /tmp/hunt_z3.sh       > /tmp/hunt_z3_main.log    2>&1 &
sleep 5
setsid bash /tmp/verify_daemon.sh > /tmp/verify_main.log     2>&1 &
echo "started. wait 60s then run health check."
```

---

## §3 启动后 Health Check（**必须全部通过才能离开**）

启动后等 **60 秒**，然后逐项确认：

### 3a. Hunt 进程存活
```bash
pgrep -c gpu-nonce
# 期望: 7（zan3 全卡）。如果 < 7，说明有进程已死
```
失败 → `cat /tmp/hunt_z3_g<N>.log` 找 CUDA_ERROR 或 binary 路径错误。

### 3b. Hunt 进度正常（无 CUDA error）
```bash
for g in 1 2 3 4 5 6 7; do echo "=== g$g ==="; tail -2 /tmp/hunt_z3_g$g.log; done
# 期望每卡：有 "progress ~X/Y  (Z nonce/s total)" 行
# 绝对不能有：CUDA_ERROR / binary not found / permission denied
```
失败 → 看完整 log：`cat /tmp/hunt_z3_g1.log`

### 3c. 候选正在生成
```bash
grep -ch CLEAN_CANDIDATE /tmp/hunt_z3_g*.log 2>/dev/null | paste -sd+ | bc
# 期望：启动 60s 后应该有 >0 个候选（约 1-2个/秒/全卡）
```
失败（60s 后仍 0）→ GPU filter 可能过严，或 binary 用了旧的 quantum_ecc。
先做：`(cd $REPO/agent/gpu-nonce && DIALOG_TAIL_NONCE=... CUDA_VISIBLE_DEVICES=1 ./target/release/validate_pipeline 2>&1 | tail -3)`

### 3d. Verify daemon 存活且在消费候选
```bash
pgrep -f verify_daemon && echo "daemon alive" || echo "DEAD"
wc -l /tmp/verify.log 2>/dev/null || echo "0 verified"
# 期望：daemon alive；如果已有候选，verify.log 应该有条目
```
失败（daemon dead）→ `cat /tmp/verify_main.log` 看错误。常见原因：fast-screen binary 路径错。

### 3e. Verify 结果格式正确
```bash
tail -3 /tmp/verify.log 2>/dev/null
# 期望格式：<nonce> | RESULT qubits=... classical=N phase=M ancilla=0
# 如果看到空行或奇怪格式 → fast-screen 输出解析有问题
```

### 3f. 候选率合理
```bash
# 等 5 分钟后
total_nonces=$(grep -h "progress ~" /tmp/hunt_z3_g*.log | grep -o "~[0-9]*" | tr -d ~ | \
               awk '{s+=$1} END{print s}')
total_cand=$(grep -ch CLEAN_CANDIDATE /tmp/hunt_z3_g*.log | paste -sd+ | bc)
echo "候选率: $total_cand / $total_nonces = $(echo "scale=2; $total_cand * 1000000 / $total_nonces" | bc) per M"
# 期望: ~10-20 per M nonce（具体值取决于电路配置，与上次 hunt 对比）
# 如果 < 1/M：GPU filter 太严，可能 binary stale
# 如果 > 100/M：GPU filter 太松，validate_pipeline 确认方向
```

### ✅ Checklist 通过标准

| 检查项 | 通过条件 |
|---|---|
| 进程数 | `pgrep -c gpu-nonce` = 7 |
| 进度行 | 每卡 log 有 `nonce/s` 行，无 CUDA_ERROR |
| 候选生成 | 60s 内 > 0 个 CLEAN_CANDIDATE |
| Verify daemon | `pgrep -f verify_daemon` 存活 |
| Verify 输出 | `/tmp/verify.log` 有格式正确的条目 |
| 候选率 | 5min 后在合理范围（10–20/M，或与历史吻合） |

**所有项通过后才算正式开跑。** 有任何一项失败，停下来修好再重启。

---

## §4 定期巡检（每 30 分钟）

```bash
# 进程还在吗？
pgrep -c gpu-nonce; pgrep -f verify_daemon

# 候选和验证进度
grep -ch CLEAN_CANDIDATE /tmp/hunt_z3_g*.log | paste -sd+ | bc   # 累计候选
wc -l /tmp/verify.log                                              # 累计已验证

# 有 winner 吗？
cat /tmp/WINNER 2>/dev/null || echo "(none yet)"

# 速率估算（最近 1 分钟的 nonce/s）
tail -1 /tmp/hunt_z3_g1.log
```

---

## §5 常见失败模式

| 症状 | 原因 | 修法 |
|---|---|---|
| `pgrep -c gpu-nonce` < 7，启动后很快 | binary 路径错 / CUDA 初始化失败 | `cat /tmp/hunt_z3_g<N>.log` 看第一行错误 |
| 60s 后候选数仍 0 | GPU filter 过严，或 stale binary | validate_pipeline 确认 filter 方向；重新 `cargo build` |
| verify.log 有条目但全是 classical>0 | P_winner 太低，电路 λ 太大 | 查 N_cls+N_phase（Phase 5.5），考虑换 knob |
| verify daemon 死掉 | fast-screen binary 路径错 / OOM | `cat /tmp/verify_main.log`；检查 FS_BIN 路径 |
| hunt 进程 5 分钟后消失 | HUNT_COUNT 太小（跑完了就退出） | 增大 HUNT_COUNT 或改成循环脚本 |
| verify.log 行数不增 | daemon sleep 住了 / 没有新候选 | `cat /tmp/new.txt`；看 allcand vs seen 差值 |
